//! The repository struct: one type per table, holding a pool and owning
//! every statement that touches it.
//!
//! Both strategies produce the same Rust API. [`Strategy::Embedded`] writes
//! the SQL into the source; [`Strategy::Server`] calls the functions that
//! [`super::sql`] puts in a migration. Swapping one for the other changes
//! the bodies, never the signatures.

use std::collections::BTreeSet;

use super::plan::{self, Kind, Operation};
use super::{Opts, Rendered, Strategy, column_list, escape, header, import_block, indent};
use crate::introspect::{Column, Model, Table};
use crate::naming;
use crate::quoting;
use crate::typemap;

/// Render the mapper for one table.
///
/// The model types it refers to — the row struct and, when the table takes
/// inserts, its `New…` input — are imported from
/// [`Opts::model_path`](super::Opts::model_path).
pub fn mapper_file(model: &Model, opts: &Opts) -> Rendered {
    let table = &model.table;
    let row = opts
        .name_override
        .clone()
        .unwrap_or_else(|| naming::pascal_case(&table.name));
    let input = format!("New{row}");
    let module = naming::ident(&table.name);
    let ops = plan::operations(table);

    let mut imports = BTreeSet::new();
    imports.insert("sqlx::PgPool".to_string());
    imports.insert(format!("{}::{module}::{row}", opts.model_path));
    if ops.iter().any(|o| o.kind == Kind::Insert) {
        imports.insert(format!("{}::{module}::{input}", opts.model_path));
    }

    let mut methods = String::new();
    for op in &ops {
        methods.push_str(&indent(
            &method(table, opts, op, &row, &input, &mut imports),
            4,
        ));
    }

    let mut code = header(
        opts,
        &format!("{}.{}", table.schema, table.name),
        &format!("{} mapper", table.kind.label()),
    );
    code.push_str(&import_block(&imports));
    let (schema, name) = (&table.schema, &table.name);
    code.push_str(&format!(
        r#"/// Repository for `{schema}.{name}`. Holds the pool for the duration of a
/// unit of work (typically a single route handler).
pub struct {row}Mapper<'a> {{
    pool: &'a PgPool,
}}

impl<'a> {row}Mapper<'a> {{
    /// Borrow a pool for the lifetime of this mapper.
    pub fn new(pool: &'a PgPool) -> Self {{
        Self {{ pool }}
    }}
"#
    ));
    code.push_str(&methods);
    code.push_str("}\n");

    Rendered {
        code,
        warnings: Vec::new(),
    }
}

fn method(
    table: &Table,
    opts: &Opts,
    op: &Operation,
    row: &str,
    input: &str,
    imports: &mut BTreeSet<String>,
) -> String {
    // The statement goes inside a Rust string literal, and a quoted
    // identifier carries the one character that would end it early.
    let sql = escape(&statement(table, opts, op));
    let key = plan::joined(&op.columns, "`, `");

    // Templates are written at column zero and indented into the impl
    // block by the caller, so what is written here looks like what comes
    // out. The doubled braces are format!'s, not the output's.
    match op.kind {
        Kind::Insert => {
            let binds = bind_fields(&table.insert_columns(), "new", opts);
            format!(
                r#"
/// Insert a row, leaving the database to fill in what it owns.
pub async fn create(&self, new: &{input}) -> Result<{row}, sqlx::Error> {{
    sqlx::query_as(
        "{sql}",
    )
{binds}    .fetch_one(self.pool)
    .await
}}
"#
            )
        }

        Kind::Update => {
            let binds = bind_fields(&ordered_update_columns(table, opts), "row", opts);
            format!(
                r#"
/// Write every column back, addressed by `{key}`. A full replace,
/// not a patch: what is in `row` is what the table will hold.
pub async fn update(&self, row: &{row}) -> Result<{row}, sqlx::Error> {{
    sqlx::query_as(
        "{sql}",
    )
{binds}    .fetch_one(self.pool)
    .await
}}
"#
            )
        }

        Kind::Delete => {
            let (params, binds) = arguments(&op.columns, opts, imports);
            format!(
                r#"
/// Delete the row identified by `{key}`.
pub async fn delete(&self{params}) -> Result<(), sqlx::Error> {{
    sqlx::query("{sql}")
{binds}        .execute(self.pool)
        .await?;
    Ok(())
}}
"#
            )
        }

        Kind::FindOne => {
            let (params, binds) = arguments(&op.columns, opts, imports);
            let method = &op.method;
            format!(
                r#"
/// Look up the row identified by `{key}`.
pub async fn {method}(&self{params}) -> Result<Option<{row}>, sqlx::Error> {{
    sqlx::query_as("{sql}")
{binds}        .fetch_optional(self.pool)
        .await
}}
"#
            )
        }

        Kind::FindMany => {
            let (params, binds) = arguments(&op.columns, opts, imports);
            let method = &op.method;
            format!(
                r#"
/// Every row whose `{key}` matches.
pub async fn {method}(&self{params}) -> Result<Vec<{row}>, sqlx::Error> {{
    sqlx::query_as("{sql}")
{binds}        .fetch_all(self.pool)
        .await
}}
"#
            )
        }

        Kind::List => format!(
            r#"
/// Every row. Put a bound on this before pointing it at a
/// large table.
pub async fn list(&self) -> Result<Vec<{row}>, sqlx::Error> {{
    sqlx::query_as("{sql}")
        .fetch_all(self.pool)
        .await
}}
"#
        ),
    }
}

// ── SQL ─────────────────────────────────────────────────────────────────────

/// The statement a method runs, in whichever strategy is in force.
fn statement(table: &Table, opts: &Opts, op: &Operation) -> String {
    if opts.strategy == Strategy::Server {
        let arity = match op.kind {
            Kind::Insert => table.insert_columns().len(),
            Kind::Update => op.columns.len() + table.update_columns().len(),
            _ => op.columns.len(),
        };
        let call = format!(
            "{}({})",
            plan::function(table, &op.call),
            placeholders(arity)
        );
        // A void function is selected, not selected from.
        return if op.kind == Kind::Delete {
            format!("SELECT {call}")
        } else {
            format!("SELECT * FROM {call}")
        };
    }

    let relation = quoting::qualified(&table.schema, &table.name);
    match op.kind {
        Kind::Insert => {
            let columns = table.insert_columns();
            let values: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| match c.literal_default() {
                    // None means 'leave it to the default', which the
                    // database cannot infer from a NULL on its own.
                    Some(default) => format!("COALESCE(${}, {default})", i + 1),
                    None => format!("${}", i + 1),
                })
                .collect();
            // The continuation lines are written for the column the
            // template puts this literal at, so the statement stays
            // readable in the file that ends up holding it.
            format!(
                "INSERT INTO {relation}\n             ({})\n         VALUES ({})\n         RETURNING *",
                column_list(&columns, ", "),
                values.join(", ")
            )
        }
        Kind::Update => {
            let columns = table.update_columns();
            let sets: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{} = ${}", quoting::ident(&c.name), i + 1))
                .collect();
            format!(
                "UPDATE {relation}\n            SET {}\n          WHERE {}\n      RETURNING *",
                sets.join(", "),
                predicate(&op.columns, columns.len() + 1)
            )
        }
        Kind::Delete => format!("DELETE FROM {relation} WHERE {}", predicate(&op.columns, 1)),
        Kind::FindOne | Kind::FindMany => format!(
            "SELECT * FROM {relation} WHERE {}",
            predicate(&op.columns, 1)
        ),
        Kind::List => {
            let mut sql = format!("SELECT * FROM {relation}");
            if !table.primary_key.is_empty() {
                sql.push_str(&format!(
                    " ORDER BY {}",
                    column_list(&table.primary_key_columns(), ", ")
                ));
            }
            sql
        }
    }
}

/// Update binds the key last when the SQL is embedded (it trails the SET
/// list) and first when a function is called (it leads the signature).
fn ordered_update_columns<'t>(table: &'t Table, opts: &Opts) -> Vec<&'t Column> {
    let key = table.primary_key_columns();
    let columns = table.update_columns();
    match opts.strategy {
        Strategy::Server => key.into_iter().chain(columns).collect(),
        Strategy::Embedded => columns.into_iter().chain(key).collect(),
    }
}

// ── Fragments ───────────────────────────────────────────────────────────────

fn bind_fields(columns: &[&Column], binding: &str, opts: &Opts) -> String {
    columns
        .iter()
        .map(|c| {
            // A `Copy` type binds by value. Borrowing one compiles, but it
            // is a clippy warning in whichever crate ends up with this
            // file, and generated code should not hand anyone a lint.
            let by_ref = if typemap::map(&c.ty, opts.generate).copy {
                ""
            } else {
                "&"
            };
            format!("    .bind({by_ref}{binding}.{})\n", naming::ident(&c.name))
        })
        .collect()
}

/// Parameter list and bind calls for a set of lookup columns.
fn arguments(columns: &[&Column], opts: &Opts, imports: &mut BTreeSet<String>) -> (String, String) {
    let mut params = String::new();
    let mut binds = String::new();
    for column in columns {
        let mapped = typemap::map(&column.ty, opts.generate);
        for import in &mapped.imports {
            imports.insert(import.clone());
        }
        let name = naming::ident(&column.name);
        params.push_str(&format!(", {name}: {}", param_type(&mapped.text)));
        binds.push_str(&format!("        .bind({name})\n"));
    }
    (params, binds)
}

/// The Rust type a lookup argument takes: borrowed where borrowing is the
/// natural shape for a caller, by value otherwise.
fn param_type(ty: &str) -> String {
    if ty == "String" {
        return "&str".to_string();
    }
    match ty.strip_prefix("Vec<").and_then(|t| t.strip_suffix('>')) {
        Some(inner) => format!("&[{inner}]"),
        None => ty.to_string(),
    }
}

fn predicate(columns: &[&Column], start: usize) -> String {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{} = ${}", quoting::ident(&c.name), start + i))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn placeholders(count: usize) -> String {
    (1..=count)
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;
    use crate::config::Generate;

    fn render(strategy: Strategy) -> String {
        let generate = Generate::default();
        mapper_file(&fixture::product(), &fixture::opts(&generate, strategy)).code
    }

    #[test]
    fn embedded_writes_its_own_statements() {
        let out = render(Strategy::Embedded);
        assert!(out.contains("INSERT INTO shop.product"), "{out}");
        // A literal default is reachable by passing None.
        assert!(
            out.contains("COALESCE($3, 'draft'::shop.product_status)"),
            "{out}"
        );
        assert!(
            out.contains("SELECT * FROM shop.product WHERE id = $1"),
            "{out}"
        );
        assert!(
            out.contains("DELETE FROM shop.product WHERE id = $1"),
            "{out}"
        );
    }

    #[test]
    fn server_calls_functions_instead() {
        let out = render(Strategy::Server);
        assert!(out.contains("SELECT * FROM shop.product_insert("), "{out}");
        assert!(out.contains("SELECT * FROM shop.product_get($1)"), "{out}");
        assert!(
            out.contains("SELECT * FROM shop.product_by_slug($1)"),
            "{out}"
        );
        // A void function is selected, not selected from.
        assert!(out.contains("SELECT shop.product_delete($1)"), "{out}");
        assert!(!out.contains("INSERT INTO"), "{out}");
    }

    #[test]
    fn both_strategies_expose_the_same_api() {
        let signature = |code: &str| {
            code.lines()
                .filter(|l| l.trim_start().starts_with("pub async fn"))
                .map(str::trim)
                .map(String::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            signature(&render(Strategy::Embedded)),
            signature(&render(Strategy::Server))
        );
    }

    #[test]
    fn finders_come_from_keys_and_foreign_keys() {
        let out = render(Strategy::Embedded);
        // Unique: at most one row. Foreign key: any number.
        assert!(
            out.contains("pub async fn find_by_slug(&self, slug: &str) -> Result<Option<Product>"),
            "{out}"
        );
        assert!(
            out.contains("pub async fn find_by_org_id(&self, org_id: Uuid) -> Result<Vec<Product>"),
            "{out}"
        );
    }

    #[test]
    fn awkward_identifiers_are_quoted() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let out = mapper_file(&fixture::awkward(), &opts).code;

        // The relation is a reserved word, so it cannot go in bare.
        assert!(out.contains(r#"INSERT INTO shop.\"order\""#), "{out}");
        assert!(
            out.contains(r#"DELETE FROM shop.\"order\" WHERE id = $1"#),
            "{out}"
        );
        // So are two of the columns, and one would fold.
        assert!(
            out.contains(r#"\"select\", \"Mixed Case\", \"desc\""#),
            "{out}"
        );
        assert!(out.contains(r#"WHERE \"select\" = $1"#), "{out}");
        // A plain column stays plain — quoting everything would be noise.
        assert!(out.contains("WHERE id = $1"), "{out}");
    }

    #[test]
    fn quoted_sql_survives_the_rust_string_literal() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let out = mapper_file(&fixture::awkward(), &opts).code;
        // Every double quote inside a query string must be escaped, or
        // the generated file does not parse. Count them per line: a line
        // holding SQL should have no bare `"` between the delimiters.
        for line in out.lines().filter(|l| l.contains("order")) {
            let bare = line
                .char_indices()
                .filter(|(i, c)| *c == '"' && *i > 0 && !line[..*i].ends_with('\\'))
                .count();
            assert!(bare <= 2, "unescaped quote in generated Rust: {line}");
        }
    }

    #[test]
    fn update_is_a_full_replace_that_skips_server_owned_columns() {
        let out = render(Strategy::Embedded);
        let update = &out[out.find("pub async fn update").unwrap()..];
        assert!(update.contains("slug = $1"), "{update}");
        assert!(!update.contains("created_at ="), "{update}");
        assert!(update.contains("WHERE id = $6"), "{update}");
    }
}
