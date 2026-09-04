//! The migration behind [`Strategy::Server`](super::Strategy::Server): one
//! `LANGUAGE sql` function per operation, so the statements live on the
//! server and the mapper only calls them.
//!
//! Functions are named `<table>_<operation>` in the table's own schema —
//! `shop.product_insert`, `shop.product_by_slug`. Readers are `STABLE`,
//! writers are left `VOLATILE`.
//!
//! Every argument is required. Postgres only allows `DEFAULT` on trailing
//! parameters, and a defaulted column can sit anywhere in a table, so
//! giving some parameters defaults would mean reordering them away from
//! the column order. The generated mapper passes all of them regardless.

use super::plan::{self, Kind};
use super::{Opts, column_list, header_with};
use crate::introspect::{Column, Model, Table};
use crate::quoting;

/// Render the migration defining a table's CRUD functions.
///
/// The file opens by dropping any earlier definition of exactly these
/// function names, whatever their signature, so a regenerated set replaces
/// the previous one instead of overloading it.
pub fn migration_file(model: &Model, opts: &Opts) -> String {
    let table = &model.table;
    let ops = plan::operations(table);

    let mut sql = header_with(
        opts,
        &format!("{}.{}", table.schema, table.name),
        &format!("{} functions", table.kind.label()),
        "--",
    );

    let names: Vec<String> = ops
        .iter()
        .map(|op| quoting::literal(&plan::function_name(table, &op.call)))
        .collect();

    let schema = quoting::literal(&table.schema);
    let names = names.join(", ");
    sql.push_str(&format!(
        r#"-- Replace any earlier definition of these functions, whatever the
-- signature, so a regenerated set does not overload the old one.
DO $$
DECLARE
    fn record;
BEGIN
    FOR fn IN
        SELECT p.oid::regprocedure AS signature
          FROM pg_proc p
          JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname = {schema}
           AND p.proname IN ({names})
    LOOP
        EXECUTE format('DROP FUNCTION IF EXISTS %s', fn.signature);
    END LOOP;
END $$;
"#
    ));

    for op in &ops {
        sql.push('\n');
        sql.push_str(&function(table, op));
    }

    sql
}

fn function(table: &Table, op: &plan::Operation) -> String {
    let relation = quoting::qualified(&table.schema, &table.name);
    let name = plan::function(table, &op.call);

    let (params, returns, volatility, body) = match op.kind {
        Kind::Insert => {
            let columns = table.insert_columns();
            let values: Vec<String> = columns
                .iter()
                .map(|c| match c.literal_default() {
                    // NULL means 'use the default', which the column's own
                    // default expression cannot express from the call site.
                    Some(default) => format!("COALESCE({}, {default})", param(c)),
                    None => param(c),
                })
                .collect();
            (
                signature(&columns),
                relation.clone(),
                "",
                format!(
                    "    INSERT INTO {relation}\n        ({})\n    VALUES\n        ({})\n    RETURNING *;",
                    column_list(&columns, ", "),
                    values.join(", ")
                ),
            )
        }
        Kind::Update => {
            let columns = table.update_columns();
            let all: Vec<&Column> = op.columns.iter().copied().chain(columns.clone()).collect();
            let sets: Vec<String> = columns
                .iter()
                .map(|c| format!("{} = {}", quoting::ident(&c.name), param(c)))
                .collect();
            (
                signature(&all),
                relation.clone(),
                "",
                format!(
                    "    UPDATE {relation}\n       SET {}\n     WHERE {}\n    RETURNING *;",
                    sets.join(",\n           "),
                    predicate(&op.columns)
                ),
            )
        }
        Kind::Delete => (
            signature(&op.columns),
            "void".to_string(),
            "",
            format!(
                "    DELETE FROM {relation}\n     WHERE {};",
                predicate(&op.columns)
            ),
        ),
        Kind::FindOne | Kind::FindMany => (
            signature(&op.columns),
            format!("SETOF {relation}"),
            "\nSTABLE",
            format!(
                "    SELECT *\n      FROM {relation}\n     WHERE {};",
                predicate(&op.columns)
            ),
        ),
        Kind::List => {
            let mut body = format!("    SELECT *\n      FROM {relation}");
            if !table.primary_key.is_empty() {
                body.push_str(&format!(
                    "\n     ORDER BY {}",
                    column_list(&table.primary_key_columns(), ", ")
                ));
            }
            body.push(';');
            (String::new(), format!("SETOF {relation}"), "\nSTABLE", body)
        }
    };

    format!(
        r#"CREATE OR REPLACE FUNCTION {name}({params})
RETURNS {returns}
LANGUAGE sql{volatility}
AS $$
{body}
$$;
"#
    )
}

/// `p_slug text, p_status text` — one parameter per column, in table order.
fn signature(columns: &[&Column]) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let params: Vec<String> = columns
        .iter()
        .map(|c| format!("    {} {}", param(c), c.sql_type))
        .collect();
    format!("\n{}\n", params.join(",\n"))
}

/// Parameters are prefixed so a body can never mistake one for the column
/// of the same name.
fn param(column: &Column) -> String {
    quoting::ident(&format!("p_{}", column.name))
}

fn predicate(columns: &[&Column]) -> String {
    columns
        .iter()
        .map(|c| format!("{} = {}", quoting::ident(&c.name), param(c)))
        .collect::<Vec<_>>()
        .join("\n       AND ")
}

#[cfg(test)]
mod tests {
    use super::super::Strategy;
    use super::super::fixture;
    use super::*;
    use crate::config::Generate;

    fn render() -> String {
        let generate = Generate::default();
        migration_file(
            &fixture::product(),
            &fixture::opts(&generate, Strategy::Server),
        )
    }

    #[test]
    fn every_planned_operation_gets_a_function() {
        let out = render();
        for name in [
            "shop.product_insert",
            "shop.product_get",
            "shop.product_by_slug",
            "shop.product_by_org_id",
            "shop.product_list",
            "shop.product_update",
            "shop.product_delete",
        ] {
            assert!(
                out.contains(&format!("CREATE OR REPLACE FUNCTION {name}(")),
                "missing {name} in {out}"
            );
        }
    }

    #[test]
    fn readers_are_stable_and_writers_are_not() {
        let out = render();
        let get = &out[out.find("FUNCTION shop.product_get").unwrap()..];
        assert!(get[..200].contains("LANGUAGE sql\nSTABLE"), "{get}");
        let insert = &out[out.find("FUNCTION shop.product_insert").unwrap()..];
        assert!(!insert[..400].contains("STABLE"), "{insert}");
    }

    #[test]
    fn the_drop_block_names_only_this_tables_functions() {
        let out = render();
        assert!(out.contains("'product_insert'"), "{out}");
        assert!(out.contains("nspname = 'shop'"), "{out}");
        // Never a wildcard: 'product_%' would also match product_variant_*.
        assert!(!out.contains("LIKE"), "{out}");
    }

    #[test]
    fn awkward_identifiers_are_quoted() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Server);
        let out = migration_file(&fixture::awkward(), &opts);

        // The relation needs quoting; the function name derived from it
        // does not, since `order_insert` is not itself a reserved word.
        assert!(out.contains(r#"RETURNS shop."order""#), "{out}");
        assert!(
            out.contains("CREATE OR REPLACE FUNCTION shop.order_insert("),
            "{out}"
        );
        assert!(out.contains(r#"INSERT INTO shop."order""#), "{out}");
        assert!(out.contains(r#"p_select text"#), "{out}");
        // A parameter named after a folding column needs quoting too.
        assert!(out.contains(r#""p_Mixed Case" integer"#), "{out}");
        // The drop block compares names as literals, not identifiers.
        assert!(out.contains("'order_insert'"), "{out}");
        assert!(out.contains("nspname = 'shop'"), "{out}");
    }

    #[test]
    fn parameters_are_prefixed_and_defaults_coalesced() {
        let out = render();
        assert!(out.contains("p_slug text"), "{out}");
        assert!(
            out.contains("COALESCE(p_status, 'draft'::shop.product_status)"),
            "{out}"
        );
    }
}
