//! The repository struct: one type per table, holding a pool and owning
//! every statement that touches it.
//!
//! Both strategies produce the same Rust API. [`Strategy::Embedded`] writes
//! the SQL into the source; [`Strategy::Server`] calls the functions that
//! [`super::sql`] puts in a migration. Swapping one for the other changes
//! the bodies, never the signatures.

use std::collections::BTreeSet;

use super::children::{self, ChildField};
use super::plan::{self, Kind, Operation};
use super::{OWNED, Opts, Rendered, Strategy, column_list, escape, header, import_block, indent};
use crate::introspect::{Column, Model, PgType, Table};
use crate::naming;
use crate::quoting;
use crate::typemap;

/// Once per file, what the per-method notice is contrasting with.
const YOURS: &str = "\
// Methods whose doc comment says proto owns them are rewritten to match
// the schema. Everything else in this file is yours — methods, comments,
// imports — and a regeneration leaves it exactly as it is.

";

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
    let mut warnings = Vec::new();
    let children = children::of(table, opts.generate);
    for child in &children.fields {
        methods.push_str(&indent(
            &child_methods(table, opts, child, &row, &ops, &mut imports, &mut warnings),
            4,
        ));
    }
    methods.push_str(&indent(&where_methods(table, opts, &row), 4));

    let mut code = header(
        opts,
        &format!("{}.{}", table.schema, table.name),
        &format!("{} mapper", table.kind.label()),
    );
    code.push_str(YOURS);
    code.push_str(&import_block(&imports));
    let (schema, name) = (&table.schema, &table.name);
    let owned = indent(OWNED, 4);
    code.push_str(&format!(
        r#"/// Repository for `{schema}.{name}`. Holds the pool for the duration of a
/// unit of work (typically a single route handler).
pub struct {row}Mapper<'a> {{
    pool: &'a PgPool,
}}

impl<'a> {row}Mapper<'a> {{
    /// Borrow a pool for the lifetime of this mapper.
{owned}    pub fn new(pool: &'a PgPool) -> Self {{
        Self {{ pool }}
    }}
"#
    ));
    code.push_str(&methods);
    code.push_str("}\n");
    if opts.pyo3 {
        code.push_str(&python_block(
            table,
            opts,
            &ops,
            &children.fields,
            &row,
            &input,
            &mut warnings,
        ));
    }

    Rendered { code, warnings }
}

// ── Python ──────────────────────────────────────────────────────────────────

/// The mapper as a Python class: the same methods, each run to
/// completion on the runtime the generated `Database` holds, since there
/// is no `await` on the far side of the crossing.
///
/// Everything here is behind the pyo3 feature, and every method carries
/// the owned notice, so [`crate::reconcile`] treats the block exactly as
/// it treats the Rust one above it.
fn python_block(
    table: &Table,
    opts: &Opts,
    ops: &[Operation],
    children: &[ChildField],
    row: &str,
    input: &str,
    warnings: &mut Vec<String>,
) -> String {
    let feature = &opts.generate.pyo3_feature;
    let bridge = &opts.bridge_path;
    let owned = indent(OWNED, 4);

    let mut methods = String::new();
    for op in ops {
        methods.push_str(&indent(&python_method(op, opts, row, input), 4));
    }
    for child in children {
        methods.push_str(&indent(
            &python_child_methods(table, child, ops, opts, row, warnings),
            4,
        ));
    }
    methods.push_str(&indent(&python_where_methods(opts, row), 4));
    let converter = python_query_fn(table, opts);

    format!(
        r#"
// ── Python ──────────────────────────────────────────────────────────

/// `{row}Mapper` for Python: the same methods, each run to completion on
/// the `Database` it was built with.
#[cfg(feature = "{feature}")]
#[pyo3::pyclass(name = "{row}Mapper", frozen)]
pub struct Py{row}Mapper {{
    db: pyo3::Py<{bridge}::Database>,
}}

#[cfg(feature = "{feature}")]
#[pyo3::pymethods]
impl Py{row}Mapper {{
    /// Bind to a database.
{owned}    #[new]
    fn new(db: pyo3::Py<{bridge}::Database>) -> Self {{
        Self {{ db }}
    }}
{methods}}}
{converter}"#
    )
}

/// `find_where` and `count_where` for Python: a dict of conditions, read
/// through [`python_query_fn`], and the Rust method behind it.
fn python_where_methods(opts: &Opts, row: &str) -> String {
    let bridge = &opts.bridge_path;
    format!(
        r#"
/// Rows matching `conditions`: a dict of column name to value. A key
/// may carry an operator after a double underscore — `price__lt`,
/// `slug__like`, `id__in` — and is `=` without one. `None` is
/// `IS NULL` (`IS NOT NULL` under `__ne`); a list is `= ANY`.
/// `order_by` is a column name or a list of them, `-name` for
/// descending.
{OWNED}#[pyo3(signature = (conditions, *, order_by = None, limit = None, offset = None))]
fn find_where(
    &self,
    py: pyo3::Python<'_>,
    conditions: &pyo3::Bound<'_, pyo3::types::PyDict>,
    order_by: Option<&pyo3::Bound<'_, pyo3::PyAny>>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> pyo3::PyResult<Vec<{row}>> {{
    let query = query_from_python(conditions, order_by, limit, offset)?;
    let db = self.db.get();
    let mapper = {row}Mapper::new(&db.pool);
    {bridge}::run(db, py, mapper.find_where(query))
}}

/// How many rows match `conditions`, read as `find_where` reads them.
{OWNED}fn count_where(
    &self,
    py: pyo3::Python<'_>,
    conditions: &pyo3::Bound<'_, pyo3::types::PyDict>,
) -> pyo3::PyResult<i64> {{
    let query = query_from_python(conditions, None, None, None)?;
    let db = self.db.get();
    let mapper = {row}Mapper::new(&db.pool);
    {bridge}::run(db, py, mapper.count_where(query))
}}
"#
    )
}

/// The dict-to-`Query` conversion for one table, as a free function
/// beside the class: each column's value is read as that column's own
/// Rust type, so a `uuid` column takes a `uuid.UUID` and refuses a
/// `str`. A column whose type does not cross from Python, or is not
/// proto's to vouch for, is refused by name.
fn python_query_fn(table: &Table, opts: &Opts) -> String {
    let bridge = &opts.bridge_path;
    let query = format!("{}::Query", opts.query_path);
    let feature = &opts.generate.pyo3_feature;
    // Named in two string literals below, so escaped for them.
    let relation = escape(&format!("{}.{}", table.schema, table.name));
    let module = naming::ident(&table.name);
    let mut arms = String::new();
    for column in &table.columns {
        let name = escape(&column.name);
        match python_bindable(column, &module, opts) {
            Some(ty) => arms.push_str(&format!(
                "            \"{name}\" => {bridge}::bind::<{ty}>(query, column, op, value),\n"
            )),
            None => arms.push_str(&format!(
                "            \"{name}\" => Err({bridge}::unsupported(\"{relation}\", column)),\n"
            )),
        }
    }
    format!(
        r#"
/// The `conditions` dict as a `Query`, each value read as its column's
/// own type. proto owns this function and rewrites it when the schema
/// changes.
#[cfg(feature = "{feature}")]
pub fn query_from_python(
    conditions: &pyo3::Bound<'_, pyo3::types::PyDict>,
    order_by: Option<&pyo3::Bound<'_, pyo3::PyAny>>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> pyo3::PyResult<{query}> {{
    {bridge}::query(conditions, order_by, limit, offset, |query, column, op, value| {{
        match column {{
{arms}            _ => Err({bridge}::no_column("{relation}", column)),
        }}
    }})
}}
"#
    )
}

/// The full Rust path a column's value is read as from Python, or
/// `None` when it cannot be: an array, a composite, a type from
/// `[generate.types]`, one proto does not know, or one with no
/// conversion on the Python side.
fn python_bindable(column: &Column, module: &str, opts: &Opts) -> Option<String> {
    let generate = opts.generate;
    let mapped = typemap::map(&column.ty, generate);
    let pg_name = match &column.ty {
        PgType::Array(_) | PgType::Composite { .. } => return None,
        PgType::Enum { name, .. } | PgType::Scalar(name) => name,
    };
    if mapped.unmapped.is_some()
        || generate.types.contains_key(pg_name)
        || generate.types.contains_key(pg_name.trim_start_matches('_'))
    {
        return None;
    }
    if let PgType::Enum { name, .. } = &column.ty {
        // Re-exported from the model's own module, wherever it is filed.
        return Some(format!(
            "{}::{module}::{}",
            opts.model_path,
            naming::pascal_case(name)
        ));
    }
    // Only what pyo3's conversions cover: the numbers, text, and the
    // uuid, decimal and chrono types its features turn on.
    const CROSSES: [&str; 14] = [
        "bool",
        "i8",
        "i16",
        "i32",
        "i64",
        "f32",
        "f64",
        "String",
        "Uuid",
        "Decimal",
        "DateTime<Utc>",
        "NaiveDateTime",
        "NaiveDate",
        "NaiveTime",
    ];
    if !CROSSES.contains(&mapped.text.as_str()) {
        return None;
    }
    // Written in full, since the file's imports are for the Rust side
    // and a name used only under the feature would be an unused import
    // without it.
    let mut path = mapped.text.clone();
    for import in &mapped.imports {
        let leaf = import.rsplit("::").next().unwrap_or(import);
        path = path.replace(leaf, import);
    }
    Some(path)
}

/// One Python method: the Rust method's doc, its arguments as Python
/// takes them, and the call run on the runtime.
fn python_method(op: &Operation, opts: &Opts, row: &str, input: &str) -> String {
    let (params, args) = python_arguments(&op.columns, opts);
    let bridge = &opts.bridge_path;
    let method = &op.method;
    let key = plan::joined(&op.columns, "`, `");
    let (doc, params, args, ret) = match op.kind {
        Kind::Insert => (
            "Insert a row, leaving the database to fill in what it owns.".to_string(),
            vec![format!("new: pyo3::PyRef<'_, {input}>")],
            "&new".to_string(),
            row.to_string(),
        ),
        Kind::Update => (
            format!("Write every column back, addressed by `{key}`. A full replace."),
            vec![format!("row: pyo3::PyRef<'_, {row}>")],
            "&row".to_string(),
            row.to_string(),
        ),
        Kind::Delete => (
            format!("Delete the row identified by `{key}`."),
            params,
            args,
            "()".to_string(),
        ),
        Kind::FindOne => (
            format!("Look up the row identified by `{key}`."),
            params,
            args,
            format!("Option<{row}>"),
        ),
        Kind::FindMany => (
            format!("Every row whose `{key}` matches."),
            params,
            args,
            format!("Vec<{row}>"),
        ),
        Kind::List => (
            "Every row.".to_string(),
            params,
            args,
            format!("Vec<{row}>"),
        ),
    };
    let params: String = params.iter().map(|p| format!(",\n    {p}")).collect();
    format!(
        r#"
/// {doc}
{OWNED}fn {method}(
    &self,
    py: pyo3::Python<'_>{params},
) -> pyo3::PyResult<{ret}> {{
    let db = self.db.get();
    let mapper = {row}Mapper::new(&db.pool);
    {bridge}::run(db, py, mapper.{method}({args}))
}}
"#
    )
}

/// The children loaders for Python. Python has no `&mut`, so
/// `load_<field>` hands back the row with the field filled rather than
/// changing the one it was given; the `_with_<field>` finder wraps the
/// Rust one as any other finder is wrapped. The same collision rule as
/// [`child_methods`] decides whether the finder exists at all, so the
/// two surfaces agree.
///
/// Handing back a copy needs `Clone` on the row. The default derives
/// give it; a configuration that dropped it gets the finder, a warning,
/// and no loader, rather than a wrapper that does not compile.
fn python_child_methods(
    table: &Table,
    child: &ChildField,
    ops: &[Operation],
    opts: &Opts,
    row: &str,
    warnings: &mut Vec<String>,
) -> String {
    let bridge = &opts.bridge_path;
    let (field, stem) = (&child.field, &child.stem);
    let (c_schema, c_table) = (&child.child.schema, &child.child.table);

    let mut out = String::new();
    if opts.generate.derives.iter().any(|d| d == "Clone") {
        out.push_str(&format!(
            r#"
/// The row with `{field}` loaded: every `{c_schema}.{c_table}` row that
/// refers to it. Hands back a filled copy rather than changing the row
/// it was given.
{OWNED}fn load_{stem}(
    &self,
    py: pyo3::Python<'_>,
    row: pyo3::PyRef<'_, {row}>,
) -> pyo3::PyResult<{row}> {{
    let db = self.db.get();
    let mapper = {row}Mapper::new(&db.pool);
    let mut row = (*row).clone();
    {bridge}::run(db, py, async move {{
        mapper.load_{stem}(&mut row).await?;
        Ok(row)
    }})
}}
"#
        ));
    } else {
        warnings.push(format!(
            "{}.{}: `load_{stem}` needs `Clone` in [generate] derives to hand a \
             row back to Python; the Python class gets no loader for `{field}`",
            table.schema, table.name
        ));
    }

    if let Some((key, wrapper)) = finder_wrapper(ops, stem) {
        let (params, args) = python_arguments(&key.columns, opts);
        let params: String = params.iter().map(|p| format!(",\n    {p}")).collect();
        let finder = &key.method;
        out.push_str(&format!(
            r#"
/// `{finder}`, with `{field}` loaded.
{OWNED}fn {wrapper}(
    &self,
    py: pyo3::Python<'_>{params},
) -> pyo3::PyResult<Option<{row}>> {{
    let db = self.db.get();
    let mapper = {row}Mapper::new(&db.pool);
    {bridge}::run(db, py, mapper.{wrapper}({args}))
}}
"#
        ));
    }
    out
}

/// Parameters as Python hands them over, and the expressions that pass
/// them on to the Rust method. A `&str` crosses as itself; a slice
/// cannot, so Python gives a `Vec` and the call borrows it.
fn python_arguments(columns: &[&Column], opts: &Opts) -> (Vec<String>, String) {
    let mut params = Vec::new();
    let mut args = Vec::new();
    for column in columns {
        let name = naming::ident(&column.name);
        // The Rust signature is the reference; only the slice case differs.
        let mapped = typemap::map(&column.ty, opts.generate);
        let rust = param_type(&mapped.text);
        match rust.strip_prefix("&[").and_then(|t| t.strip_suffix(']')) {
            Some(inner) => {
                params.push(format!("{name}: Vec<{inner}>"));
                args.push(format!("&{name}"));
            }
            None => {
                params.push(format!("{name}: {rust}"));
                args.push(name);
            }
        }
    }
    (params, args.join(", "))
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
{OWNED}pub async fn create(&self, new: &{input}) -> Result<{row}, sqlx::Error> {{
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
{OWNED}pub async fn update(&self, row: &{row}) -> Result<{row}, sqlx::Error> {{
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
{OWNED}pub async fn delete(&self{params}) -> Result<(), sqlx::Error> {{
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
{OWNED}pub async fn {method}(&self{params}) -> Result<Option<{row}>, sqlx::Error> {{
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
{OWNED}pub async fn {method}(&self{params}) -> Result<Vec<{row}>, sqlx::Error> {{
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
{OWNED}pub async fn list(&self) -> Result<Vec<{row}>, sqlx::Error> {{
    sqlx::query_as("{sql}")
        .fetch_all(self.pool)
        .await
}}
"#
        ),
    }
}

/// The methods that fill one children field: `load_<field>` on a row
/// already in hand, and `find_by_id_with_<field>` when the table has a
/// key to find one by. The child's rows come from the child's own
/// statement — under [`Strategy::Server`], the finder function the
/// child's migration defines — so nothing new is needed on the server
/// for the parent to have its children. The wrapper is skipped, with a
/// warning, when a column happens to give a planned finder its name.
#[allow(clippy::too_many_arguments)]
fn child_methods(
    table: &Table,
    opts: &Opts,
    child: &ChildField,
    row: &str,
    ops: &[Operation],
    imports: &mut BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> String {
    // The child's type is never named: the field it is assigned to
    // carries it, and an import would only sit unused.
    let (field, stem) = (&child.field, &child.stem);
    let (c_schema, c_table, c_column) =
        (&child.child.schema, &child.child.table, &child.child.column);
    let function = quoting::qualified(c_schema, &format!("{c_table}_{}", child.call));

    let sql = escape(&match opts.strategy {
        Strategy::Server => format!("SELECT * FROM {function}($1)"),
        Strategy::Embedded => format!(
            "SELECT * FROM {} WHERE {} = $1",
            quoting::qualified(c_schema, c_table),
            quoting::ident(c_column)
        ),
    });
    // The parent's referenced column, bound the way its type wants.
    let by_ref = table
        .column(&child.child.ref_column)
        .is_some_and(|c| !typemap::map(&c.ty, opts.generate).copy);
    let by_ref = if by_ref { "&" } else { "" };
    let ref_field = naming::ident(&child.child.ref_column);

    // The function is the child's, written by the child's migration.
    // A parent generated on its own has no way to write it.
    let server_note = match opts.strategy {
        Strategy::Server => format!(
            "/// Calls `{function}`, which the migration for\n\
             /// `{c_schema}.{c_table}` defines: generate that mapper first.\n"
        ),
        Strategy::Embedded => String::new(),
    };

    let mut out = format!(
        r#"
/// Every `{c_schema}.{c_table}` row whose `{c_column}` is this row's
/// `{}`, into `{field}`. One level: the children's own children
/// are not loaded.
{server_note}{OWNED}pub async fn load_{stem}(&self, row: &mut {row}) -> Result<(), sqlx::Error> {{
    row.{field} = sqlx::query_as("{sql}")
        .bind({by_ref}row.{ref_field})
        .fetch_all(self.pool)
        .await?;
    Ok(())
}}
"#,
        child.child.ref_column
    );

    if let Some(key) = ops.iter().find(|op| op.call == "get") {
        let Some((_, wrapper)) = finder_wrapper(ops, stem) else {
            warnings.push(format!(
                "{}.{}: a column already gives a finder the name `{}_with_{stem}`, \
                 so `{field}` gets `load_{stem}` and no finder of its own",
                table.schema, table.name, key.method
            ));
            return out;
        };
        let (params, _) = arguments(&key.columns, opts, imports);
        let args = key
            .columns
            .iter()
            .map(|c| naming::ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let finder = &key.method;
        out.push_str(&format!(
            r#"
/// `{finder}`, with `{field}` loaded.
{OWNED}pub async fn {wrapper}(&self{params}) -> Result<Option<{row}>, sqlx::Error> {{
    match self.{finder}({args}).await? {{
        Some(mut row) => {{
            self.load_{stem}(&mut row).await?;
            Ok(Some(row))
        }}
        None => Ok(None),
    }}
}}
"#
        ));
    }
    out
}

/// `find_where` and `count_where`: the one place a mapper's SQL is
/// assembled at run time, under either strategy, since a `Query` names
/// columns the caller chooses and no fixed function can take that.
/// Every value is still bound, and the column list the `Query` is
/// checked against is written here, where a regeneration corrects it.
fn where_methods(table: &Table, opts: &Opts, row: &str) -> String {
    // Named in full rather than imported: a table named `query` has a
    // row type `Query` of its own, and the two must not meet.
    let query = format!("{}::Query", opts.query_path);
    let relation = escape(&quoting::qualified(&table.schema, &table.name));
    let qualified = format!("{}.{}", table.schema, table.name);
    let columns: Vec<String> = table
        .columns
        .iter()
        .map(|c| {
            format!(
                "(\"{}\", \"{}\")",
                escape(&c.name),
                escape(&quoting::ident(&c.name))
            )
        })
        .collect();
    let columns = columns.join(", ");
    format!(
        r#"
/// Rows matching `query`, in the order it asks for.
///
/// The clause is assembled here, so this is the one method whose SQL
/// is not a fixed string under either strategy: a `Query` names
/// columns at run time, and no function can take that. Every value is
/// still bound, never written into the statement, and a name that is
/// not a column of `{qualified}` is an error before anything is sent.
{OWNED}pub async fn find_where(&self, query: {query}) -> Result<Vec<{row}>, sqlx::Error> {{
    let (sql, args) = query.select("{relation}", Self::columns())?;
    sqlx::query_as_with(&sql, args).fetch_all(self.pool).await
}}

/// How many rows match `query`. Its order, limit and offset do not
/// apply.
{OWNED}pub async fn count_where(&self, query: {query}) -> Result<i64, sqlx::Error> {{
    let (sql, args) = query.count("{relation}", Self::columns())?;
    sqlx::query_scalar_with(&sql, args).fetch_one(self.pool).await
}}

/// Every column of `{qualified}` by its Postgres name, with the
/// identifier as a statement writes it. What a `Query` may name.
{OWNED}fn columns() -> &'static [(&'static str, &'static str)] {{
    &[{columns}]
}}
"#
    )
}

/// The key finder a child field gets a `_with_<stem>` wrapper on, and
/// the wrapper's name — unless a column already claims that name, in
/// which case there is no wrapper. Both the Rust and the Python surface
/// ask this, so neither can have a finder the other lacks.
fn finder_wrapper<'o, 't>(
    ops: &'o [Operation<'t>],
    stem: &str,
) -> Option<(&'o Operation<'t>, String)> {
    let key = ops.iter().find(|op| op.call == "get")?;
    let wrapper = format!("{}_with_{stem}", key.method);
    (!ops.iter().any(|op| op.method == wrapper)).then_some((key, wrapper))
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
pub(crate) fn param_type(ty: &str) -> String {
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
    fn every_method_proto_generates_says_so() {
        let out = render(Strategy::Embedded);
        // The async ones, plus `new`, plus the `columns` list behind
        // `find_where`, which is a method too and proto's to rewrite.
        let methods = out.matches("    pub async fn ").count()
            + out.matches("    pub fn ").count()
            + out.matches("    fn ").count();
        assert_eq!(
            out.matches("proto owns this method").count(),
            methods,
            "{out}"
        );
        // The notice is part of the doc comment, so it travels with the
        // method when reconcile moves or replaces it.
        assert!(
            out.contains("    /// large table.\n    ///\n    /// proto owns this method"),
            "{out}"
        );
        // And the file says, once, that the rest is not proto's.
        assert_eq!(
            out.matches("Everything else in this file is yours").count(),
            1
        );
        assert!(syn::parse_file(&out).is_ok(), "{out}");
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
    fn a_parent_can_load_its_children() {
        let out = render(Strategy::Embedded);
        // The field's type carries the child's; naming it here would
        // only be an unused import in the crate this lands in.
        assert!(!out.contains("variant::Variant"), "{out}");
        assert!(
            out.contains(
                "pub async fn load_children(&self, row: &mut Product) -> Result<(), sqlx::Error>"
            ),
            "{out}"
        );
        assert!(
            out.contains("SELECT * FROM shop.variant WHERE product_id = $1"),
            "{out}"
        );
        // The key is Copy, so it binds by value.
        assert!(out.contains(".bind(row.id)"), "{out}");
        assert!(
            out.contains(
                "pub async fn find_by_id_with_children(&self, id: Uuid) -> \
                 Result<Option<Product>, sqlx::Error>"
            ),
            "{out}"
        );

        // On the server, the child's own finder function does the work,
        // so the parent needs nothing new in its migration — and the
        // method says whose migration writes it.
        let server = render(Strategy::Server);
        assert!(
            server.contains("SELECT * FROM shop.variant_by_product_id($1)"),
            "{server}"
        );
        assert!(
            server.contains("/// Calls `shop.variant_by_product_id`, which the migration for"),
            "{server}"
        );
        assert!(!out.contains("which the migration for"), "{out}");
    }

    /// A column named `id_with_children` would plan a finder called
    /// `find_by_id_with_children`. The loader still comes; the wrapper
    /// that would collide with it does not, and the run is told.
    #[test]
    fn a_wrapper_that_would_shadow_a_finder_is_skipped_and_said() {
        let mut model = fixture::product();
        model.table.columns.push(fixture::column(
            "id_with_children",
            crate::introspect::PgType::Scalar("text".into()),
            "text",
            true,
        ));
        model
            .table
            .unique_keys
            .push(vec!["id_with_children".into()]);
        let generate = Generate::default();
        let rendered = mapper_file(&model, &fixture::opts(&generate, Strategy::Embedded));
        let out = rendered.code;
        assert!(out.contains("pub async fn load_children"), "{out}");
        assert_eq!(
            out.matches("pub async fn find_by_id_with_children").count(),
            1
        );
        assert!(
            out.contains("find_by_id_with_children(&self, id_with_children: &str)"),
            "{out}"
        );
        assert_eq!(rendered.warnings.len(), 1, "{:?}", rendered.warnings);
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    #[test]
    fn a_tree_loads_its_own_kind() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let out = mapper_file(&fixture::category(), &opts).code;
        assert!(out.contains("row: &mut Category"), "{out}");
        assert!(
            out.contains("SELECT * FROM shop.category WHERE parent_id = $1"),
            "{out}"
        );
        assert!(syn::parse_file(&out).is_ok(), "{out}");
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

    fn render_python(strategy: Strategy) -> String {
        let generate = Generate::default();
        let mut opts = fixture::opts(&generate, strategy);
        opts.pyo3 = true;
        mapper_file(&fixture::product(), &opts).code
    }

    #[test]
    fn without_pyo3_nothing_crosses() {
        assert!(!render(Strategy::Embedded).contains("pyo3"));
    }

    #[test]
    fn the_python_class_is_feature_gated_and_stands_on_the_bridge() {
        let out = render_python(Strategy::Embedded);
        assert!(
            out.contains(
                "#[cfg(feature = \"python\")]\n\
                 #[pyo3::pyclass(name = \"ProductMapper\", frozen)]\n\
                 pub struct PyProductMapper {\n    db: pyo3::Py<super::python::Database>,\n}"
            ),
            "{out}"
        );
        // Each method runs the Rust one on the Database's runtime.
        assert!(
            out.contains(
                "    fn find_by_slug(\n        &self,\n        py: pyo3::Python<'_>,\n        \
                 slug: &str,\n    ) -> pyo3::PyResult<Option<Product>> {"
            ),
            "{out}"
        );
        assert!(
            out.contains("super::python::run(db, py, mapper.find_by_slug(slug))"),
            "{out}"
        );
        assert!(
            out.contains("super::python::run(db, py, mapper.create(&new))"),
            "{out}"
        );
        assert!(out.contains("new: pyo3::PyRef<'_, NewProduct>,"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    #[test]
    fn a_query_finds_and_counts_under_both_strategies() {
        for strategy in [Strategy::Embedded, Strategy::Server] {
            let out = render(strategy);
            // Named in full: a table named `query` has a `Query` of its own.
            assert!(!out.contains("use super::query::Query;"), "{out}");
            assert!(
                out.contains(
                    "    pub async fn find_where(&self, query: super::query::Query) -> \
                     Result<Vec<Product>, sqlx::Error> {\n        \
                     let (sql, args) = query.select(\"shop.product\", Self::columns())?;"
                ),
                "{out}"
            );
            assert!(
                out.contains("    pub async fn count_where(&self, query: super::query::Query) -> Result<i64, sqlx::Error> {"),
                "{out}"
            );
            // The column list is what the query is checked against, and
            // it is written where a regeneration corrects it.
            assert!(
                out.contains("&[(\"id\", \"id\"), (\"slug\", \"slug\"), (\"name\", \"name\")"),
                "{out}"
            );
        }
    }

    #[test]
    fn a_table_that_would_take_a_reserved_module_name_is_named() {
        let mut query = fixture::product();
        query.table.name = "Query".into();
        let models = [fixture::product(), query];
        assert_eq!(
            crate::render::reserved_module(&models, &["query", "python"]).as_deref(),
            Some("shop.Query")
        );
        assert_eq!(
            crate::render::reserved_module(&models[..1], &["query", "python"]),
            None
        );
    }

    #[test]
    fn a_quoted_column_is_listed_as_the_statement_writes_it() {
        let generate = Generate::default();
        let out = mapper_file(
            &fixture::awkward(),
            &fixture::opts(&generate, Strategy::Embedded),
        )
        .code;
        assert!(
            out.contains("(\"Mixed Case\", \"\\\"Mixed Case\\\"\")"),
            "{out}"
        );
    }

    #[test]
    fn a_dict_query_crosses_by_column_type() {
        let out = render_python(Strategy::Embedded);
        let python = &out[out.find("── Python").unwrap()..];
        assert!(
            python.contains(
                "#[pyo3(signature = (conditions, *, order_by = None, limit = None, offset = None))]"
            ),
            "{python}"
        );
        assert!(python.contains("    fn find_where(\n"), "{python}");
        assert!(python.contains("    fn count_where(\n"), "{python}");
        // The converter is a free function beside the class, gated, and
        // reads each column as its own type by full path.
        assert!(
            python.contains("#[cfg(feature = \"python\")]\npub fn query_from_python("),
            "{python}"
        );
        for arm in [
            "\"id\" => super::python::bind::<uuid::Uuid>(query, column, op, value),",
            "\"slug\" => super::python::bind::<String>(query, column, op, value),",
            "\"price\" => super::python::bind::<rust_decimal::Decimal>(query, column, op, value),",
            "\"created_at\" => super::python::bind::<chrono::DateTime<chrono::Utc>>(query, column, op, value),",
            "\"status\" => super::python::bind::<crate::model::product::ProductStatus>(query, column, op, value),",
            "_ => Err(super::python::no_column(\"shop.product\", column)),",
        ] {
            assert!(python.contains(arm), "missing {arm}\n{python}");
        }
        // Without the feature nothing of it is named, so the Rust build
        // carries no unused import for a type only Python reads.
        let plain = render(Strategy::Embedded);
        assert!(!plain.contains("query_from_python"), "{plain}");
        assert!(!plain.contains("rust_decimal"), "{plain}");
    }

    #[test]
    fn a_column_that_does_not_cross_is_refused_by_name() {
        let mut generate = Generate::default();
        generate
            .types
            .insert("numeric".into(), "bigdecimal::BigDecimal".into());
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = true;
        let out = mapper_file(&fixture::product(), &opts).code;
        assert!(
            out.contains("\"price\" => Err(super::python::unsupported(\"shop.product\", column)),"),
            "{out}"
        );
    }

    #[test]
    fn children_cross_to_python_as_a_loader_and_a_finder() {
        let out = render_python(Strategy::Embedded);
        let python = &out[out.find("── Python").unwrap()..];
        assert!(
            python.contains(
                "    fn load_children(\n        &self,\n        py: pyo3::Python<'_>,\n        \
                 row: pyo3::PyRef<'_, Product>,\n    ) -> pyo3::PyResult<Product> {"
            ),
            "{python}"
        );
        assert!(
            python.contains("mapper.load_children(&mut row).await?;"),
            "{python}"
        );
        assert!(
            python.contains("    fn find_by_id_with_children(\n"),
            "{python}"
        );
        assert!(
            python.contains("super::python::run(db, py, mapper.find_by_id_with_children(id))"),
            "{python}"
        );

        // Without `Clone` there is no copy to hand back: the finder stays,
        // the loader goes, and the run says why.
        let mut generate = Generate::default();
        generate.derives.retain(|d| d != "Clone");
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = true;
        let rendered = mapper_file(&fixture::product(), &opts);
        let python = &rendered.code[rendered.code.find("── Python").unwrap()..];
        assert!(!python.contains("fn load_children("), "{python}");
        assert!(python.contains("fn find_by_id_with_children("), "{python}");
        assert!(
            rendered.warnings.iter().any(|w| w.contains("`Clone`")),
            "{:?}",
            rendered.warnings
        );

        // The off switch takes both away, as it does on the Rust side.
        let generate = Generate {
            children_field: String::new(),
            ..Generate::default()
        };
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = true;
        let out = mapper_file(&fixture::product(), &opts).code;
        assert!(!out.contains("load_children"), "{out}");
        assert!(!out.contains("with_children"), "{out}");
    }

    #[test]
    fn the_python_surface_is_the_same_under_both_strategies() {
        let python = |code: &str| code[code.find("── Python").unwrap()..].to_string();
        assert_eq!(
            python(&render_python(Strategy::Embedded)),
            python(&render_python(Strategy::Server))
        );
    }

    #[test]
    fn the_python_block_survives_its_own_reconcile() {
        // Reconcile keys impls by type name and takes back owned methods
        // the render lacks. `impl PyProductMapper` must be matched against
        // itself, never against `impl ProductMapper`, or the loaders on
        // one and the wrappers on the other would take each other back.
        let out = render_python(Strategy::Embedded);
        assert_eq!(
            crate::reconcile::reconcile(&out, &out).as_deref(),
            Some(out.as_str())
        );
        // Both impls carry a `load_children` now — the Rust loader and
        // its Python wrapper — so a reconcile that keyed methods by name
        // alone would take one for the other. Both are still here.
        assert!(out.contains("pub async fn load_children"), "{out}");
        assert!(
            out.contains("fn load_children(\n        &self,\n        py"),
            "{out}"
        );
        assert_eq!(out.matches("fn load_children(").count(), 2, "{out}");
    }

    #[test]
    fn every_python_method_says_proto_owns_it() {
        let out = render_python(Strategy::Embedded);
        let python = &out[out.find("── Python").unwrap()..];
        let methods = python.matches("    fn ").count();
        assert_eq!(
            python.matches("proto owns this method").count(),
            methods,
            "{python}"
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
