//! The repository struct: one type per table, holding a pool and owning
//! every statement that touches it.
//!
//! Both strategies produce the same Rust API. [`Strategy::Embedded`] writes
//! the SQL into the source; [`Strategy::Server`] calls the functions that
//! [`super::sql`] puts in a migration. Swapping one for the other changes
//! the bodies, never the signatures.

use std::collections::BTreeSet;

use super::plan::{self, Kind, Operation};
use super::{Opts, Rendered, Strategy, header, import_block};
use crate::introspect::{Column, Model, Table};
use crate::naming;
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
        methods.push_str(&method(table, opts, op, &row, &input, &mut imports));
    }

    let mut code = header(
        opts,
        &format!("{}.{}", table.schema, table.name),
        &format!("{} mapper", table.kind.label()),
    );
    code.push_str(&import_block(&imports));
    code.push_str(&format!(
        "/// Repository for `{}.{}`. Holds the pool for the duration of a\n\
         /// unit of work (typically a single route handler).\n\
         pub struct {row}Mapper<'a> {{\n    pool: &'a PgPool,\n}}\n\n\
         impl<'a> {row}Mapper<'a> {{\n    \
         /// Borrow a pool for the lifetime of this mapper.\n    \
         pub fn new(pool: &'a PgPool) -> Self {{\n        Self {{ pool }}\n    }}\n",
        table.schema, table.name
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
    let sql = statement(table, opts, op);
    match op.kind {
        Kind::Insert => {
            let binds = bind_fields(&table.insert_columns(), "new");
            format!(
                "\n    /// Insert a row, leaving the database to fill in what it owns.\n    \
                 pub async fn create(&self, new: &{input}) -> Result<{row}, sqlx::Error> {{\n        \
                 sqlx::query_as(\n            \"{sql}\",\n        )\n{binds}        \
                 .fetch_one(self.pool)\n        .await\n    }}\n"
            )
        }
        Kind::Update => {
            let columns = ordered_update_columns(table, opts);
            let binds = bind_fields(&columns, "row");
            format!(
                "\n    /// Write every column back, addressed by `{}`. A full replace,\n    \
                 /// not a patch: what is in `row` is what the table will hold.\n    \
                 pub async fn update(&self, row: &{row}) -> Result<{row}, sqlx::Error> {{\n        \
                 sqlx::query_as(\n            \"{sql}\",\n        )\n{binds}        \
                 .fetch_one(self.pool)\n        .await\n    }}\n",
                plan::joined(&op.columns, "`, `")
            )
        }
        Kind::Delete => {
            let (params, binds) = arguments(&op.columns, opts, imports);
            format!(
                "\n    /// Delete the row identified by `{}`.\n    \
                 pub async fn delete(&self{params}) -> Result<(), sqlx::Error> {{\n        \
                 sqlx::query(\"{sql}\")\n{binds}            \
                 .execute(self.pool)\n            .await?;\n        Ok(())\n    }}\n",
                plan::joined(&op.columns, "`, `")
            )
        }
        Kind::FindOne => {
            let (params, binds) = arguments(&op.columns, opts, imports);
            format!(
                "\n    /// Look up the row identified by `{}`.\n    \
                 pub async fn {}(&self{params}) -> Result<Option<{row}>, sqlx::Error> {{\n        \
                 sqlx::query_as(\"{sql}\")\n{binds}            \
                 .fetch_optional(self.pool)\n            .await\n    }}\n",
                plan::joined(&op.columns, "`, `"),
                op.method
            )
        }
        Kind::FindMany => {
            let (params, binds) = arguments(&op.columns, opts, imports);
            format!(
                "\n    /// Every row whose `{}` matches.\n    \
                 pub async fn {}(&self{params}) -> Result<Vec<{row}>, sqlx::Error> {{\n        \
                 sqlx::query_as(\"{sql}\")\n{binds}            \
                 .fetch_all(self.pool)\n            .await\n    }}\n",
                plan::joined(&op.columns, "`, `"),
                op.method
            )
        }
        Kind::List => format!(
            "\n    /// Every row. Put a bound on this before pointing it at a\n    \
             /// large table.\n    \
             pub async fn list(&self) -> Result<Vec<{row}>, sqlx::Error> {{\n        \
             sqlx::query_as(\"{sql}\")\n            \
             .fetch_all(self.pool)\n            .await\n    }}\n"
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

    let relation = format!("{}.{}", table.schema, table.name);
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
            format!(
                "INSERT INTO {relation}\n                 ({})\n             VALUES ({})\n             RETURNING *",
                names(&columns, ", "),
                values.join(", ")
            )
        }
        Kind::Update => {
            let columns = table.update_columns();
            let sets: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{} = ${}", c.name, i + 1))
                .collect();
            format!(
                "UPDATE {relation}\n                SET {}\n              WHERE {}\n          RETURNING *",
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
                    names(&table.primary_key_columns(), ", ")
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

fn bind_fields(columns: &[&Column], binding: &str) -> String {
    columns
        .iter()
        .map(|c| format!("        .bind(&{binding}.{})\n", naming::ident(&c.name)))
        .collect()
}

/// Parameter list and bind calls for a set of lookup columns.
fn arguments(columns: &[&Column], opts: &Opts, imports: &mut BTreeSet<String>) -> (String, String) {
    let mut params = String::new();
    let mut binds = String::new();
    for column in columns {
        let mapped = typemap::map(&column.ty, &opts.generate.types);
        for import in &mapped.imports {
            imports.insert(import.clone());
        }
        let name = naming::ident(&column.name);
        params.push_str(&format!(", {name}: {}", borrowed(&mapped.text)));
        binds.push_str(&format!("            .bind({name})\n"));
    }
    (params, binds)
}

/// Lookup arguments are borrowed where borrowing is the natural Rust shape.
fn borrowed(ty: &str) -> String {
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
        .map(|(i, c)| format!("{} = ${}", c.name, start + i))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn placeholders(count: usize) -> String {
    (1..=count)
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn names(columns: &[&Column], sep: &str) -> String {
    columns
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(sep)
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
    fn update_is_a_full_replace_that_skips_server_owned_columns() {
        let out = render(Strategy::Embedded);
        let update = &out[out.find("pub async fn update").unwrap()..];
        assert!(update.contains("slug = $1"), "{update}");
        assert!(!update.contains("created_at ="), "{update}");
        assert!(update.contains("WHERE id = $6"), "{update}");
    }
}
