//! The operation layer: what an integrating engineer would otherwise
//! write on top of the mappers. It is generated because the routing —
//! a table name to a mapper, a request's values to that mapper's column
//! types — is what the type map already knows, and what drifts first
//! when the schema moves.
//!
//! `Data` is the first operation: one request in, one outcome out,
//! through whichever mapper the request names, with the table's own
//! types in and out and no interpreter in the path. Under `--pyo3` a
//! Python class of the same name wraps it: the request dict is read as
//! the table's types, runs through the Rust, and its outcome comes
//! back as the table's Python classes.

use super::plan::{self, Kind};
use super::{OWNED, Opts, Rendered, escape, header, indent};
use crate::introspect::Model;
use crate::naming;
use crate::typemap;

/// One schema's tables, and where their mappers and models are.
pub struct Source<'a> {
    /// The schema.
    pub schema: String,
    /// Its tables.
    pub models: &'a [Model],
    /// Module holding its mappers: `crate::mapper`, or
    /// `crate::mapper::shop` in a database run.
    pub mapper_path: String,
    /// Module holding its models, likewise.
    pub model_path: String,
}

/// Once per file, what the per-method notice is contrasting with.
const YOURS: &str = "\
// Methods whose doc comment says proto owns them are rewritten to match
// the schema — `execute` above all, which is the routing table and is not
// yours to edit — and the enums are replaced whole, like the generated
// enums are. Everything else in this file is yours: an operation of
// your own beside `Data` stays where you put it.

";

/// An identifier for composing a longer one: `naming::ident` without
/// the `r#` a keyword gets, which has no place inside a name.
fn bare_ident(name: &str) -> String {
    naming::ident(name).trim_start_matches("r#").to_string()
}

/// One table as `Data` sees it: its names, its paths, and what it can do.
struct Served<'a> {
    model: &'a Model,
    source: &'a Source<'a>,
    /// `shop_product`, the handler; `ShopProduct`, the variant.
    handler: String,
    variant: String,
    /// `"shop.product"`, escaped for a literal.
    qualified: String,
    /// `"shop.product" | "product"`, the match arm's pattern.
    pattern: String,
    row: String,
    module: String,
    insert: bool,
    update: bool,
    delete: bool,
}

impl Served<'_> {
    fn mapper(&self) -> String {
        format!(
            "{}::{}::{}Mapper",
            self.source.mapper_path, self.module, self.row
        )
    }
    fn row_ty(&self) -> String {
        format!("{}::{}::{}", self.source.model_path, self.module, self.row)
    }
    fn input_ty(&self) -> String {
        format!(
            "{}::{}::New{}",
            self.source.model_path, self.module, self.row
        )
    }
    fn patch_ty(&self) -> String {
        format!(
            "{}::{}::{}Patch",
            self.source.model_path, self.module, self.row
        )
    }
}

/// `data.rs`: the `Data` operation over every table in `sources`.
///
/// `aliases` says whether a table also routes by its bare name where
/// only one table has it: a schema run does, a database run names
/// every table by its schema.
pub fn data_file(sources: &[Source], opts: &Opts, aliases: bool) -> Rendered {
    let schemas = sources
        .iter()
        .map(|s| s.schema.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut code = header(opts, &schemas, "data operation");
    code.push_str(YOURS);

    // Bare names route only where one table has the name.
    let mut bare: std::collections::BTreeMap<&str, usize> = Default::default();
    for source in sources {
        for model in source.models {
            *bare.entry(model.table.name.as_str()).or_default() += 1;
        }
    }
    let mut taken = std::collections::BTreeSet::new();
    let mut served = Vec::new();
    for source in sources {
        for model in source.models {
            let table = &model.table;
            let ops = plan::operations(table);
            let has = |kind: Kind| ops.iter().any(|op| op.kind == kind);
            // `foo_bar.baz` and `foo.bar_baz` compose the same name; the
            // second to arrive takes a number.
            let base = format!("{}_{}", bare_ident(&table.schema), bare_ident(&table.name));
            let mut handler = base.clone();
            let mut n = 2;
            while !taken.insert(handler.clone()) {
                handler = format!("{base}_{n}");
                n += 1;
            }
            let qualified = escape(&format!("{}.{}", table.schema, table.name));
            let mut pattern = format!("\"{qualified}\"");
            if aliases && bare[table.name.as_str()] == 1 {
                pattern.push_str(&format!(" | \"{}\"", escape(&table.name)));
            }
            served.push(Served {
                model,
                source,
                variant: naming::pascal_case(&handler),
                handler,
                qualified,
                pattern,
                row: naming::pascal_case(&table.name),
                module: naming::ident(&table.name),
                insert: has(Kind::Insert),
                update: has(Kind::Update),
                delete: has(Kind::Delete),
            });
        }
    }

    let generate = opts.generate;
    let owned = indent(OWNED, 4);
    let feature = &generate.pyo3_feature;
    let bridge = format!("{}::python", opts.mapper_path);
    let query = format!("{}::query::Query", opts.mapper_path);

    // ── The enums: what goes in, what comes out ──
    let mut values = String::new();
    let mut rows = String::new();
    let mut row = String::new();
    let mut arms = String::new();
    let mut handlers = String::new();
    for t in &served {
        let (variant, qualified) = (&t.variant, &t.qualified);
        if t.insert {
            values.push_str(&format!(
                "    /// A row to insert into `{qualified}`.\n    {variant}Input({}),\n",
                t.input_ty()
            ));
        }
        if t.update {
            values.push_str(&format!(
                "    /// What to set on rows of `{qualified}`.\n    {variant}Patch({}),\n",
                t.patch_ty()
            ));
        }
        rows.push_str(&format!("    {variant}(Vec<{}>),\n", t.row_ty()));
        row.push_str(&format!("    {variant}({}),\n", t.row_ty()));
        arms.push_str(&format!(
            "            {} => self.{}(request).await,\n",
            t.pattern, t.handler
        ));
        handlers.push_str(&indent(&handler_fn(t, opts), 4));
    }
    let derives = |list: &[String], with_serde: bool| {
        let mut out = Vec::new();
        if typemap::has_derive(list, "Debug") {
            out.push("Debug");
        }
        if with_serde && typemap::has_derive(list, "Serialize") {
            out.push("serde::Serialize");
        }
        if out.is_empty() {
            String::new()
        } else {
            format!("#[derive({})]\n", out.join(", "))
        }
    };
    let outcome_derives = derives(&generate.derives, true);
    let values_derives = derives(&generate.input_derives, false);

    code.push_str(&format!(
        r#"use sqlx::PgPool;

use {query};

/// What a request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {{
    Find,
    Count,
    Create,
    Update,
    Delete,
}}

impl Op {{
    /// The operation a request names: `find`, `count`, `create`,
    /// `update` or `delete`. `None` for any other word.
{owned}    pub fn parse(op: &str) -> Option<Self> {{
        Some(match op {{
            "find" => Self::Find,
            "count" => Self::Count,
            "create" => Self::Create,
            "update" => Self::Update,
            "delete" => Self::Delete,
            _ => return None,
        }})
    }}

    /// The word a request names it by.
{owned}    pub fn name(self) -> &'static str {{
        match self {{
            Self::Find => "find",
            Self::Count => "count",
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }}
    }}
}}

/// One request: the table by name, the operation, the query that picks
/// the rows, and for a write, what it writes.
///
/// `query` is what the mapper's `find_where` takes: the conditions,
/// the order, the limit and the offset. `count` uses its conditions
/// only, and `create` does not read it. `values` is the table's input
/// for `create` and its patch for `update`, `None` for the rest.
pub struct Request<'a> {{
    pub table: &'a str,
    pub op: Op,
    pub query: Query,
    pub values: Option<Values>,
}}

/// What a write carries, typed for its table: the input `create`
/// inserts, or the patch `update` sets on every row the query finds.
///
/// A variant is a whole input or patch; boxing one would put an
/// allocation in every request for the sake of a lint.
#[allow(clippy::large_enum_variant)]
{values_derives}pub enum Values {{
{values}}}

/// What a request produced: rows for `find` and `update`, one row for
/// `create`, a count, or how many rows `delete` removed.
///
/// `Row` carries a whole row beside a count; boxing it would put an
/// allocation in every outcome for the sake of a lint.
#[allow(clippy::large_enum_variant)]
{outcome_derives}pub enum Outcome {{
    Rows(Rows),
    Row(Row),
    Count(i64),
    Deleted(usize),
}}

/// Rows of one table.
{outcome_derives}pub enum Rows {{
{rows}}}

/// One row of one table.
#[allow(clippy::large_enum_variant)]
{outcome_derives}pub enum Row {{
{row}}}

/// Why a request could not run.
#[derive(Debug)]
pub enum Error {{
    /// The table is not one `Data` routes to.
    NoSuchTable(String),
    /// The table has no such operation: `create` on a view, `delete`
    /// on a table without a key.
    Cannot {{ table: String, op: Op }},
    /// A write came without its values, or with another table's.
    Values {{
        table: String,
        op: Op,
        needs: &'static str,
    }},
    /// The database refused or failed.
    Database(sqlx::Error),
}}

impl std::fmt::Display for Error {{
{owned}    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{
        match self {{
            Self::NoSuchTable(table) => write!(f, "`{{table}}` is not a table Data routes to"),
            Self::Cannot {{ table, op }} => write!(f, "{{table}} has no `{{}}`", op.name()),
            Self::Values {{ table, op, needs }} => {{
                write!(f, "`{{}}` on {{table}} needs `Values::{{needs}}`", op.name())
            }}
            Self::Database(e) => write!(f, "{{e}}"),
        }}
    }}
}}

impl std::error::Error for Error {{
{owned}    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {{
        match self {{
            Self::Database(e) => Some(e),
            _ => None,
        }}
    }}
}}

impl From<sqlx::Error> for Error {{
{owned}    fn from(e: sqlx::Error) -> Self {{
        Self::Database(e)
    }}
}}

/// One request in, one outcome out, through whichever mapper the
/// request names.
///
/// ```ignore
/// let data = Data::new(&pool);
/// let outcome = data
///     .execute(Request {{
///         table: "shop.product",
///         op: Op::Find,
///         query: Query::new().eq("status", "active").limit(20),
///         values: None,
///     }})
///     .await?;
/// if let Outcome::Rows(Rows::ShopProduct(rows)) = outcome {{ .. }}
/// ```
pub struct Data<'a> {{
    pool: &'a PgPool,
}}

impl<'a> Data<'a> {{
    /// Borrow a pool for the lifetime of this operation.
{owned}    pub fn new(pool: &'a PgPool) -> Self {{
        Self {{ pool }}
    }}

    /// Run one request. `table` is `schema.table`, or the bare name
    /// where only one table has it. This is the routing table: proto
    /// rewrites it whenever a table arrives or leaves, so a route added
    /// by hand is lost on the next run, by design.
{owned}    pub async fn execute(&self, request: Request<'_>) -> Result<Outcome, Error> {{
        match request.table {{
{arms}            _ => Err(Error::NoSuchTable(request.table.to_string())),
        }}
    }}
{handlers}}}
"#
    ));

    if opts.pyo3 {
        code.push_str(&python_block(&served, opts, &bridge, feature));
    }

    Rendered {
        code,
        warnings: Vec::new(),
    }
}

/// One table's handler: the mapper, and each operation through it.
fn handler_fn(t: &Served, opts: &Opts) -> String {
    let (handler, variant, qualified) = (&t.handler, &t.variant, &t.qualified);
    let cannot = |op: &str| {
        format!(
            "        Op::{op} => Err(Error::Cannot {{ table: \"{qualified}\".to_string(), op: Op::{op} }}),\n"
        )
    };
    let create = if t.insert {
        format!(
            r#"        Op::Create => match request.values {{
            Some(Values::{variant}Input(new)) => {{
                Ok(Outcome::Row(Row::{variant}(mapper.create(&new).await?)))
            }}
            _ => Err(Error::Values {{
                table: "{qualified}".to_string(),
                op: Op::Create,
                needs: "{variant}Input",
            }}),
        }},
"#
        )
    } else {
        cannot("Create")
    };
    let update = if t.update {
        format!(
            r#"        Op::Update => match request.values {{
            Some(Values::{variant}Patch(patch)) => {{
                let rows = mapper.find_where(request.query).await?;
                let mut updated = Vec::with_capacity(rows.len());
                for mut row in rows {{
                    patch.apply(&mut row);
                    updated.push(mapper.update(&row).await?);
                }}
                Ok(Outcome::Rows(Rows::{variant}(updated)))
            }}
            _ => Err(Error::Values {{
                table: "{qualified}".to_string(),
                op: Op::Update,
                needs: "{variant}Patch",
            }}),
        }},
"#
        )
    } else {
        cannot("Update")
    };
    let delete = if t.delete {
        let key = t
            .model
            .table
            .primary_key_columns()
            .iter()
            .map(|c| {
                let field = naming::ident(&c.name);
                // Borrowed exactly where the mapper's signature borrows.
                let mapped = typemap::map(&c.ty, opts.generate);
                if super::mapper::param_type(&mapped.text).starts_with('&') {
                    format!("&row.{field}")
                } else {
                    format!("row.{field}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"        Op::Delete => {{
            let rows = mapper.find_where(request.query).await?;
            let deleted = rows.len();
            for row in rows {{
                mapper.delete({key}).await?;
            }}
            Ok(Outcome::Deleted(deleted))
        }}
"#
        )
    } else {
        cannot("Delete")
    };
    format!(
        r#"
/// `{qualified}`, through its mapper.
{OWNED}async fn {handler}(&self, request: Request<'_>) -> Result<Outcome, Error> {{
    let mapper = {mapper}::new(self.pool);
    match request.op {{
        Op::Find => Ok(Outcome::Rows(Rows::{variant}(
            mapper.find_where(request.query).await?,
        ))),
        Op::Count => Ok(Outcome::Count(mapper.count_where(request.query).await?)),
{create}{update}{delete}    }}
}}
"#,
        mapper = t.mapper()
    )
}

// ── Python ──────────────────────────────────────────────────────────────────

/// `Data` for Python, over the Rust one: the request dict read as the
/// table's types, run, and its outcome handed back as the table's
/// classes. Every item is behind the pyo3 feature.
fn python_block(served: &[Served], opts: &Opts, bridge: &str, feature: &str) -> String {
    let owned = indent(OWNED, 4);
    let cfg = format!("#[cfg(feature = \"{feature}\")]\n");
    let needs_values = served.iter().any(|t| t.insert || t.update);
    let needs_fixed = served.iter().any(|t| t.update);

    let mut read_arms = String::new();
    let mut rows_arms = String::new();
    let mut row_arms = String::new();
    let mut key_arms = String::new();
    let mut store_arms = String::new();
    let mut update_arms = String::new();
    let mut patch_fns = String::new();
    for t in served {
        let (variant, qualified, pattern, handler) =
            (&t.variant, &t.qualified, &t.pattern, &t.handler);
        let create = if t.insert {
            format!(
                r#"                Op::Create => Some(Values::{variant}Input(
                    conditions
                        .py()
                        .get_type::<{input}>()
                        .call((), Some(Self::values(values, op)?))?
                        .extract()?,
                )),
"#,
                input = t.input_ty()
            )
        } else {
            String::new()
        };
        let update = if t.update {
            format!(
                r#"                Op::Update => Some(Values::{variant}Patch(Self::{handler}_patch(
                    Self::values(values, op)?,
                )?)),
"#
            )
        } else {
            String::new()
        };
        let values = if t.insert || t.update {
            format!(
                "            let values = match op {{\n{create}{update}                _ => None,\n            }};\n"
            )
        } else {
            "            let values = None;\n".to_string()
        };
        let mapper_module = format!("{}::{}", t.source.mapper_path, t.module);
        read_arms.push_str(&format!(
            r#"        {pattern} => {{
            let query = {mapper_module}::query_from_python(conditions, order_by, limit, offset)?;
{values}            Ok((query, values))
        }}
"#
        ));
        rows_arms.push_str(&format!(
            "            Rows::{variant}(rows) => rows.into_pyobject(py)?.into_any().unbind(),\n"
        ));
        row_arms.push_str(&format!(
            "            Row::{variant}(row) => row.into_pyobject(py)?.into_any().unbind(),\n"
        ));
        if t.update {
            patch_fns.push_str(&patch_from_python(t, opts, bridge));
        }

        // `find` addresses a row by the table's key; a table without
        // one cannot be found that way, and says so.
        let table = &t.model.table;
        if table.primary_key.is_empty() {
            key_arms.push_str(&format!(
                "        {pattern} => Err(OperationError::new_err(\"`{qualified}` has no key\")),\n"
            ));
        } else {
            let columns = table
                .primary_key
                .iter()
                .map(|c| format!("\"{}\"", escape(c)))
                .collect::<Vec<_>>()
                .join(", ");
            key_arms.push_str(&format!("        {pattern} => Ok(&[{columns}]),\n"));
        }
        // `store` and `update` go by the model's type: the table whose
        // input, or whose row, it is. The log line names the row by
        // its key, read off the Python object by attribute, so a key
        // the database filled in is there too.
        let attrs = table
            .primary_key
            .iter()
            .map(|c| format!("\"{}\"", escape(&bare_ident(c))))
            .collect::<Vec<_>>()
            .join(", ");
        let detail = if table.primary_key.is_empty() {
            format!("\"{qualified}\".to_string()")
        } else {
            format!("format!(\"{qualified} pk={{}}\", Self::key_of(&object, &[{attrs}])?)")
        };
        let (mapper, row_ty, input_ty) = (t.mapper(), t.row_ty(), t.input_ty());
        if t.insert {
            store_arms.push_str(&format!(
                r#"    if model.is_instance_of::<{input_ty}>() {{
        let new: {input_ty} = model.extract()?;
        let mapper = {mapper}::new(&db.pool);
        let row = run(db, py, mapper.create(&new)).map_err(|e| Self::database(py, e))?;
        let object = row.into_pyobject(py)?.into_any();
        let detail = {detail};
        return Ok((object.unbind(), detail));
    }}
"#
            ));
        }
        if t.update {
            update_arms.push_str(&format!(
                r#"    if model.is_instance_of::<{row_ty}>() {{
        let row: {row_ty} = model.extract()?;
        let mapper = {mapper}::new(&db.pool);
        let row = run(db, py, mapper.update(&row)).map_err(|e| Self::database(py, e))?;
        let object = row.into_pyobject(py)?.into_any();
        let detail = {detail};
        return Ok((object.unbind(), detail));
    }}
"#
            ));
        }
    }

    // Only what some arm calls, or the file warns under the feature.
    let mut helpers = String::new();
    if needs_values {
        helpers.push_str(&format!(
            r#"
/// The `values` a write needs.
{OWNED}fn values<'r, 'py>(values: Option<&'r Bound<'py, PyDict>>, op: Op) -> PyResult<&'r Bound<'py, PyDict>> {{
    values.ok_or_else(|| {{
        pyo3::exceptions::PyValueError::new_err(format!("`{{}}` needs `values`", op.name()))
    }})
}}
"#
        ));
    }
    if needs_fixed {
        helpers.push_str(&format!(
            r#"
/// `ValueError`: `update` cannot set the column — it is the table's
/// key, or the database's own.
{OWNED}fn fixed(table: &str, column: &str) -> PyErr {{
    pyo3::exceptions::PyValueError::new_err(format!(
        "`{{column}}` of {{table}} is not yours to set: it is the key, or the database's own"
    ))
}}
"#
        ));
    }
    let helpers = indent(&helpers, 4);
    let patch_fns = indent(&patch_fns, 4);
    let routes = indent(
        &format!(
            r#"
/// One request, read as the table's types, run through the Rust
/// `Data`, and its outcome as Python objects.
#[allow(clippy::too_many_arguments)]
{OWNED}fn execute(
    py: Python<'_>,
    db: &Database,
    table: &str,
    op: Op,
    conditions: &Bound<'_, PyDict>,
    order_by: Option<&Bound<'_, PyAny>>,
    limit: Option<i64>,
    offset: Option<i64>,
    values: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {{
    let (query, values) = Self::read(table, op, conditions, order_by, limit, offset, values)?;
    let data = Data::new(&db.pool);
    let request = Request {{
        table,
        op,
        query,
        values,
    }};
    let outcome = block_on(db, py, data.execute(request)).map_err(Self::error)?;
    Self::outcome(py, outcome)
}}

/// The dict's `where`, `order_by`, `limit` and `offset` as the table's
/// `Query`, and its `values` as the table's input or patch, each value
/// read as its column's own type. This mirrors the routing table
/// above, and is proto's as that is.
#[allow(clippy::too_many_arguments)]
{OWNED}fn read(
    table: &str,
    op: Op,
    conditions: &Bound<'_, PyDict>,
    order_by: Option<&Bound<'_, PyAny>>,
    limit: Option<i64>,
    offset: Option<i64>,
    values: Option<&Bound<'_, PyDict>>,
) -> PyResult<(Query, Option<Values>)> {{
    match table {{
{read_arms}        _ => Err(pyo3::exceptions::PyKeyError::new_err(format!(
            "`{{table}}` is not a table Data routes to"
        ))),
    }}
}}

/// An outcome as Python objects: rows as a list of the table's class,
/// a row as one of it, a number as an int.
{OWNED}fn outcome(py: Python<'_>, outcome: Outcome) -> PyResult<Py<PyAny>> {{
    Ok(match outcome {{
        Outcome::Count(n) => n.into_pyobject(py)?.into_any().unbind(),
        Outcome::Deleted(n) => n.into_pyobject(py)?.into_any().unbind(),
        Outcome::Rows(rows) => match rows {{
{rows_arms}        }},
        Outcome::Row(row) => match row {{
{row_arms}        }},
    }})
}}

/// `Error` as Python sees it: a table not routed is a `KeyError`, an
/// operation the table cannot do or a write without its values a
/// `ValueError`, and the database's a `ProtoError`.
{OWNED}fn error(error: Error) -> PyErr {{
    match error {{
        Error::NoSuchTable(_) => pyo3::exceptions::PyKeyError::new_err(error.to_string()),
        Error::Cannot {{ .. }} | Error::Values {{ .. }} => {{
            pyo3::exceptions::PyValueError::new_err(error.to_string())
        }}
        Error::Database(e) => {bridge}::error(e),
    }}
}}
"#
        ),
        4,
    );

    let by_key = indent(
        &format!(
            r#"
/// The key of `table`: its columns, for `find` to address a row by.
{OWNED}fn key(table: &str) -> PyResult<&'static [&'static str]> {{
    match table {{
{key_arms}        _ => Err(OperationError::new_err(format!(
            "`{{table}}` is not a table Data routes to"
        ))),
    }}
}}

/// `find` behind its log line: the key as a `where`, through `execute`,
/// and the one row out of what it finds. Each key column takes one
/// value: a list would bind as `= ANY` and find several.
{OWNED}fn find_one(py: Python<'_>, db: &Database, table: &str, pk: &Bound<'_, PyAny>) -> PyResult<(Py<PyAny>, String)> {{
    let columns = Self::key(table)?;
    let conditions = PyDict::new(py);
    if let [column] = columns {{
        Self::one_value(table, pk)?;
        conditions.set_item(column, pk)?;
    }} else {{
        let parts: Vec<Bound<'_, PyAny>> = pk.try_iter()?.collect::<PyResult<_>>()?;
        if parts.len() != columns.len() {{
            return Err(OperationError::new_err(format!(
                "`{{table}}` has a key of {{}} columns; the tuple has {{}}",
                columns.len(),
                parts.len()
            )));
        }}
        for (column, part) in columns.iter().zip(parts) {{
            Self::one_value(table, &part)?;
            conditions.set_item(column, part)?;
        }}
    }}
    let rows = Self::execute(py, db, table, Op::Find, &conditions, None, Some(1), None, None)
        .map_err(|e| Self::database(py, e))?;
    let rows = rows.into_bound(py).downcast_into::<pyo3::types::PyList>()?;
    let row = match rows.len() {{
        0 => py.None(),
        _ => rows.get_item(0)?.unbind(),
    }};
    Ok((row, String::new()))
}}

/// One key value, as `find` is addressed by: a list, tuple or set is
/// several, and is refused before it can bind as `= ANY`.
{OWNED}fn one_value(table: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {{
    if value.is_instance_of::<pyo3::types::PyList>()
        || value.is_instance_of::<pyo3::types::PyTuple>()
        || value.is_instance_of::<pyo3::types::PySet>()
        || value.is_instance_of::<pyo3::types::PyFrozenSet>()
    {{
        return Err(OperationError::new_err(format!(
            "`{{table}}` is addressed by one value per key column; a collection is not one"
        )));
    }}
    Ok(())
}}

/// The key of `object`, a row, as `str()` gives each column: one value,
/// or a tuple of them, for a log line.
{OWNED}fn key_of(object: &Bound<'_, PyAny>, attrs: &[&str]) -> PyResult<String> {{
    let mut parts = Vec::with_capacity(attrs.len());
    for attr in attrs {{
        parts.push(object.getattr(*attr)?.str()?.to_string());
    }}
    Ok(match parts.as_slice() {{
        [one] => one.clone(),
        _ => format!("({{}})", parts.join(", ")),
    }})
}}

/// `store` by the model's type: the table whose input it is. The row
/// comes back with the table and key it was written under, for the
/// log line.
{OWNED}fn store(py: Python<'_>, db: &Database, model: &Bound<'_, PyAny>) -> PyResult<(Py<PyAny>, String)> {{
{store_arms}    Err(OperationError::new_err(format!(
        "`{{}}` is not an input Data knows",
        Self::type_name(model)
    )))
}}

/// `update` by the model's type: the table whose row it is. The row
/// comes back with the table and key it was written under, for the
/// log line.
{OWNED}fn update(py: Python<'_>, db: &Database, model: &Bound<'_, PyAny>) -> PyResult<(Py<PyAny>, String)> {{
{update_arms}    Err(OperationError::new_err(format!(
        "`{{}}` is not a row Data knows",
        Self::type_name(model)
    )))
}}

/// The outcome, logged: one line to the `proto.data` logger with the
/// ids that trace a request, INFO when it went through and ERROR, with
/// the error, when it did not. One line whatever the ids, the key or
/// the error carry: a control character is written escaped, so a
/// caller's input cannot end the record or start another. The outcome
/// is handed on either way.
{OWNED}fn logged(
    py: Python<'_>,
    user_id: &str,
    request_id: &str,
    what: &str,
    outcome: PyResult<(Py<PyAny>, String)>,
) -> PyResult<Py<PyAny>> {{
    let logger = py.import("logging")?.call_method1("getLogger", ("proto.data",))?;
    match outcome {{
        Ok((value, detail)) => {{
            let result = if value.is_none(py) {{ "not found" }} else {{ "ok" }};
            let detail = if detail.is_empty() {{ detail }} else {{ format!(" {{detail}}") }};
            let line = format!("request_id={{request_id}} user_id={{user_id}} {{what}}{{detail}}: {{result}}");
            logger.call_method1("info", (Self::one_line(&line),))?;
            Ok(value)
        }}
        Err(error) => {{
            let line = format!("request_id={{request_id}} user_id={{user_id}} {{what}}: failed: {{error}}");
            logger.call_method1("error", (Self::one_line(&line),))?;
            Err(error)
        }}
    }}
}}

/// `text` on one line: every control character as its escape.
{OWNED}fn one_line(text: &str) -> String {{
    text.chars()
        .map(|c| {{
            if c.is_control() {{
                c.escape_default().to_string()
            }} else {{
                c.to_string()
            }}
        }})
        .collect()
}}

/// A mapper's `ProtoError` as the operation's `DatabaseError`; any
/// other error as it is.
{OWNED}fn database(py: Python<'_>, error: PyErr) -> PyErr {{
    if error.is_instance_of::<{bridge}::ProtoError>(py) {{
        DatabaseError::new_err(error.value(py).to_string())
    }} else {{
        error
    }}
}}

/// The Python class name of `model`, for a log line or an error.
{OWNED}fn type_name(model: &Bound<'_, PyAny>) -> String {{
    model
        .get_type()
        .name()
        .map(|name| name.to_string())
        .unwrap_or_else(|_| "?".to_string())
}}
"#
        ),
        4,
    );

    format!(
        r#"
// ── Python ──────────────────────────────────────────────────────────────────

{cfg}use pyo3::prelude::*;
{cfg}use pyo3::types::PyDict;

{cfg}use {bridge}::{{Database, block_on, run}};

{cfg}pyo3::create_exception!(
    proto,
    OperationError,
    pyo3::exceptions::PyException,
    "What an operation raises unless it has a more specific error: a table `Data` does not route to, a model it does not know, a table with no key."
);
{cfg}pyo3::create_exception!(
    proto,
    DatabaseError,
    OperationError,
    "The database refused or failed, in the driver's words."
);
{cfg}pyo3::create_exception!(
    proto,
    PermissionError,
    OperationError,
    "The caller may not do this. Nothing generated raises it; it is here for an operation of your own."
);

/// `Data` for Python: one request dict in, one result out, through the
/// Rust `Data` above.
///
/// ```python
/// data = Data(db)
/// rows = data.execute({{"table": "shop.product", "op": "find",
///                      "where": {{"status": "active"}}, "limit": 20}})
/// ```
{cfg}#[pyclass(frozen, name = "Data")]
pub struct PyData {{
    db: Py<Database>,
}}

{cfg}#[pymethods]
impl PyData {{
    /// Bind to a database.
{owned}    #[new]
    fn new(db: Py<Database>) -> Self {{
        Self {{ db }}
    }}

    /// Run one request. `table` names the table, `op` is `find`,
    /// `count`, `create`, `update` or `delete`; `where`, `order_by`,
    /// `limit` and `offset` are what the mapper's `find_where` takes,
    /// and `values` is what `create` inserts or `update` sets. `find`
    /// and `update` return the rows, `create` the row, `count` and
    /// `delete` a number.
{owned}    fn execute(&self, py: Python<'_>, request: &Bound<'_, PyDict>) -> PyResult<Py<PyAny>> {{
        let table: String = required(request, "table")?.extract()?;
        let op: String = required(request, "op")?.extract()?;
        let op = Op::parse(&op).ok_or_else(|| {{
            pyo3::exceptions::PyValueError::new_err(format!(
                "`{{op}}` is not an operation: find, count, create, update or delete"
            ))
        }})?;
        let conditions = match given(request, "where")? {{
            Some(conditions) => conditions.downcast_into::<PyDict>()?,
            None => PyDict::new(py),
        }};
        let order_by = given(request, "order_by")?;
        let limit = given(request, "limit")?.map(|n| n.extract()).transpose()?;
        let offset = given(request, "offset")?.map(|n| n.extract()).transpose()?;
        let values = given(request, "values")?
            .map(|v| v.downcast_into::<PyDict>())
            .transpose()?;
        Routes::execute(
            py,
            self.db.get(),
            &table,
            op,
            &conditions,
            order_by.as_ref(),
            limit,
            offset,
            values.as_ref(),
        )
    }}

    /// The row of `table` with key `pk`, or `None`. `table` is what
    /// `execute` takes; `pk` is read as the key's own type, a tuple in
    /// column order for a composite key. One log line, with both ids.
{owned}    fn find(
        &self,
        py: Python<'_>,
        table: &str,
        pk: &Bound<'_, PyAny>,
        user_id: &str,
        request_id: &str,
    ) -> PyResult<Py<PyAny>> {{
        let what = format!("find {{table}} pk={{pk}}");
        let outcome = Routes::find_one(py, self.db.get(), table, pk);
        Routes::logged(py, user_id, request_id, &what, outcome)
    }}

    /// Insert `model`, an input of one of the tables — a `New…` — and
    /// hand back the row as inserted, with what the database filled
    /// in. One log line, with both ids.
{owned}    fn store(
        &self,
        py: Python<'_>,
        model: &Bound<'_, PyAny>,
        user_id: &str,
        request_id: &str,
    ) -> PyResult<Py<PyAny>> {{
        let what = format!("store {{}}", Routes::type_name(model));
        let outcome = Routes::store(py, self.db.get(), model);
        Routes::logged(py, user_id, request_id, &what, outcome)
    }}

    /// Write `model`, a row, back in full — what the mapper's `update`
    /// writes — and hand back the row as written. One log line, with
    /// both ids.
{owned}    fn update(
        &self,
        py: Python<'_>,
        model: &Bound<'_, PyAny>,
        user_id: &str,
        request_id: &str,
    ) -> PyResult<Py<PyAny>> {{
        let what = format!("update {{}}", Routes::type_name(model));
        let outcome = Routes::update(py, self.db.get(), model);
        Routes::logged(py, user_id, request_id, &what, outcome)
    }}
}}

/// A key the request must carry.
{cfg}fn required<'py>(request: &Bound<'py, PyDict>, key: &str) -> PyResult<Bound<'py, PyAny>> {{
    request
        .get_item(key)?
        .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(format!("a request needs `{{key}}`")))
}}

/// A key the request may carry; `None` in the dict counts as absent.
{cfg}fn given<'py>(request: &Bound<'py, PyDict>, key: &str) -> PyResult<Option<Bound<'py, PyAny>>> {{
    Ok(request.get_item(key)?.filter(|v| !v.is_none()))
}}

/// The Python side of the routing: each table's dict read as its
/// types, and its rows handed back as its class.
{cfg}struct Routes;

{cfg}impl Routes {{{routes}{helpers}{patch_fns}{by_key}}}

/// Register `Data` and the operation errors on a module. Call it after
/// the mappers' `register`, which puts the `Database` it takes and the
/// classes it returns there.
{cfg}pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {{
    m.add_class::<PyData>()?;
    m.add("OperationError", m.py().get_type::<OperationError>())?;
    m.add("DatabaseError", m.py().get_type::<DatabaseError>())?;
    m.add("PermissionError", m.py().get_type::<PermissionError>())
}}
"#
    )
}

/// The `values` of an `update` on one table as its patch, each value
/// read as its column's own type. The key and the columns the
/// database owns are refused, as `apply` has no field for them; a
/// column whose type does not cross from Python says so by name.
fn patch_from_python(t: &Served, opts: &Opts, bridge: &str) -> String {
    let generate = opts.generate;
    let table = &t.model.table;
    let (patch, qualified, handler) = (t.patch_ty(), &t.qualified, &t.handler);
    let updatable = table.update_columns();
    let mut arms = String::new();
    for column in &updatable {
        let mapped = typemap::map(&column.ty, generate);
        if !mapped.copy && !mapped.clone {
            continue; // not in the patch either
        }
        let name = escape(&column.name);
        let field = naming::ident(&column.name);
        match super::mapper::python_type(column, &t.module, &t.source.model_path, generate) {
            Some(ty) => {
                let ty = if column.not_null {
                    ty
                } else {
                    format!("Option<{ty}>")
                };
                arms.push_str(&format!(
                    "            \"{name}\" => patch.{field} = Some(value.extract::<{ty}>()?),\n"
                ));
            }
            None => arms.push_str(&format!(
                "            \"{name}\" => return Err({bridge}::unsupported(\"{qualified}\", column.to_str()?)),\n"
            )),
        }
    }
    let fixed: Vec<String> = table
        .columns
        .iter()
        .filter(|c| !updatable.iter().any(|u| u.name == c.name))
        .map(|c| format!("\"{}\"", escape(&c.name)))
        .collect();
    let fixed = if fixed.is_empty() {
        String::new()
    } else {
        format!(
            "            {} => return Err(Self::fixed(\"{qualified}\", column.to_str()?)),\n",
            fixed.join(" | ")
        )
    };
    format!(
        r#"
/// The `values` of an `update` on `{qualified}`, each read as its
/// column's own type.
{OWNED}fn {handler}_patch(values: &Bound<'_, PyDict>) -> PyResult<{patch}> {{
    let mut patch = {patch}::default();
    for (key, value) in values.iter() {{
        let column = key.downcast_into::<pyo3::types::PyString>()?;
        match column.to_str()? {{
{arms}{fixed}            other => return Err({bridge}::no_column("{qualified}", other)),
        }}
    }}
    Ok(patch)
}}
"#
    )
}

/// `mod.rs` for the operation directory: `data`, unconditionally — the
/// Rust `Data` needs no feature, and the Python class inside it is
/// gated item by item.
pub fn mod_file(opts: &Opts) -> String {
    let mut code = header(opts, "operations", "module list");
    code.push_str("pub mod data;\n");
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Generate;
    use crate::render::{Strategy, fixture};

    fn shop<'a>(models: &'a [Model]) -> Source<'a> {
        Source {
            schema: "shop".into(),
            models,
            mapper_path: "crate::mapper".into(),
            model_path: "crate::model".into(),
        }
    }

    fn render(models: &[Model]) -> String {
        let generate = Generate::default();
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = true;
        data_file(&[shop(models)], &opts, true).code
    }

    #[test]
    fn data_routes_a_request_to_the_table_it_names() {
        let models = [fixture::product()];
        let out = render(&models);
        syn::parse_file(&out).expect("data.rs parses");
        for needed in [
            "use crate::mapper::query::Query;",
            "pub struct Request<'a> {\n    pub table: &'a str,\n    pub op: Op,\n    pub query: Query,\n    pub values: Option<Values>,\n}",
            "#[allow(clippy::large_enum_variant)]\n#[derive(Debug)]\npub enum Values {\n    /// A row to insert into `shop.product`.\n    ShopProductInput(crate::model::product::NewProduct),\n    /// What to set on rows of `shop.product`.\n    ShopProductPatch(crate::model::product::ProductPatch),\n}",
            "#[allow(clippy::large_enum_variant)]\n#[derive(Debug, serde::Serialize)]\npub enum Outcome {\n    Rows(Rows),\n    Row(Row),\n    Count(i64),\n    Deleted(usize),\n}",
            "pub enum Rows {\n    ShopProduct(Vec<crate::model::product::Product>),\n}",
            "pub enum Row {\n    ShopProduct(crate::model::product::Product),\n}",
            "pub struct Data<'a> {\n    pool: &'a PgPool,\n}",
            "    pub async fn execute(&self, request: Request<'_>) -> Result<Outcome, Error> {",
            "            \"shop.product\" | \"product\" => self.shop_product(request).await,",
            "            _ => Err(Error::NoSuchTable(request.table.to_string())),",
            "    async fn shop_product(&self, request: Request<'_>) -> Result<Outcome, Error> {",
            "        let mapper = crate::mapper::product::ProductMapper::new(self.pool);",
            "            Op::Find => Ok(Outcome::Rows(Rows::ShopProduct(\n                mapper.find_where(request.query).await?,\n            ))),",
            "            Op::Count => Ok(Outcome::Count(mapper.count_where(request.query).await?)),",
            "                Some(Values::ShopProductInput(new)) => {\n                    Ok(Outcome::Row(Row::ShopProduct(mapper.create(&new).await?)))\n                }",
            "                Some(Values::ShopProductPatch(patch)) => {",
            "                        patch.apply(&mut row);",
            "                    mapper.delete(row.id).await?;",
            "impl From<sqlx::Error> for Error {",
        ] {
            assert!(out.contains(needed), "missing {needed}\n{out}");
        }
        // The routing table and every handler say proto owns them.
        let methods = out
            .lines()
            .filter(|l| {
                [
                    "    fn ",
                    "    pub fn ",
                    "    async fn ",
                    "    pub async fn ",
                ]
                .iter()
                .any(|start| l.starts_with(start))
            })
            .count();
        assert_eq!(
            out.matches("proto owns this method").count(),
            methods,
            "{out}"
        );
        assert!(
            out.contains("is the routing table and is not\n// yours to edit"),
            "{out}"
        );

        // Without pyo3 there is no Python side at all.
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let plain = data_file(&[shop(&models)], &opts, true).code;
        assert!(!plain.contains("pyo3"), "{plain}");
        assert!(!plain.contains("Python"), "{plain}");
        syn::parse_file(&plain).expect("data.rs parses without pyo3");
    }

    /// A column the model's patch leaves out — a type proto cannot
    /// clone — is not read from Python either, so the two agree.
    #[test]
    fn a_column_the_patch_leaves_out_is_not_read_either() {
        let generate = Generate {
            enum_derives: vec!["sqlx::Type".into(), "Debug".into()],
            ..Generate::default()
        };
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = true;
        let models = [fixture::product()];
        let out = data_file(&[shop(&models)], &opts, true).code;
        assert!(!out.contains("\"status\" => patch.status"), "{out}");
        assert!(out.contains("\"slug\" => patch.slug"), "{out}");
    }

    /// The Python class reads the dict as the table's types, runs the
    /// Rust `Data`, and hands the outcome back as the table's classes.
    #[test]
    fn python_wraps_the_rust_data() {
        let out = render(&[fixture::product()]);
        for needed in [
            "#[cfg(feature = \"python\")]\nuse crate::mapper::python::{Database, block_on, run};",
            "#[cfg(feature = \"python\")]\n#[pyclass(frozen, name = \"Data\")]\npub struct PyData {",
            "    fn execute(&self, py: Python<'_>, request: &Bound<'_, PyDict>) -> PyResult<Py<PyAny>> {",
            "        let op = Op::parse(&op).ok_or_else(|| {",
            "            let query = crate::mapper::product::query_from_python(conditions, order_by, limit, offset)?;",
            "                Op::Create => Some(Values::ShopProductInput(",
            "                        .get_type::<crate::model::product::NewProduct>()",
            "                Op::Update => Some(Values::ShopProductPatch(Self::shop_product_patch(",
            "    let outcome = block_on(db, py, data.execute(request)).map_err(Self::error)?;",
            "            Rows::ShopProduct(rows) => rows.into_pyobject(py)?.into_any().unbind(),",
            "            Row::ShopProduct(row) => row.into_pyobject(py)?.into_any().unbind(),",
            "        Error::Database(e) => crate::mapper::python::error(e),",
            "    fn shop_product_patch(values: &Bound<'_, PyDict>) -> PyResult<crate::model::product::ProductPatch> {",
            "        let mut patch = crate::model::product::ProductPatch::default();",
            "            \"slug\" => patch.slug = Some(value.extract::<String>()?),",
            "            \"price\" => patch.price = Some(value.extract::<Option<rust_decimal::Decimal>>()?),",
            "            \"status\" => patch.status = Some(value.extract::<crate::model::product::ProductStatus>()?),",
            "            \"id\" | \"created_at\" => return Err(Self::fixed(\"shop.product\", column.to_str()?)),",
            "            other => return Err(crate::mapper::python::no_column(\"shop.product\", other)),",
            "pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {\n    m.add_class::<PyData>()?;\n",
            "    m.add(\"PermissionError\", m.py().get_type::<PermissionError>())\n}",
        ] {
            assert!(out.contains(needed), "missing {needed}\n{out}");
        }
    }

    /// `find` goes by the table's key, `store` and `update` by the
    /// model's type, and each is one log line with the ids in it.
    #[test]
    fn find_store_and_update_go_by_key_and_by_type() {
        let models = [fixture::product()];
        let out = render(&models);
        for needed in [
            "pyo3::create_exception!(\n    proto,\n    OperationError,\n    pyo3::exceptions::PyException,",
            "pyo3::create_exception!(\n    proto,\n    DatabaseError,\n    OperationError,",
            "pyo3::create_exception!(\n    proto,\n    PermissionError,\n    OperationError,",
            "    fn find(\n        &self,\n        py: Python<'_>,\n        table: &str,\n        pk: &Bound<'_, PyAny>,\n        user_id: &str,\n        request_id: &str,\n",
            "        let what = format!(\"find {table} pk={pk}\");",
            "        Routes::logged(py, user_id, request_id, &what, outcome)",
            "        \"shop.product\" | \"product\" => Ok(&[\"id\"]),",
            "    let rows = Self::execute(py, db, table, Op::Find, &conditions, None, Some(1), None, None)",
            "        if model.is_instance_of::<crate::model::product::NewProduct>() {",
            "            let row = run(db, py, mapper.create(&new)).map_err(|e| Self::database(py, e))?;",
            "            let detail = format!(\"shop.product pk={}\", Self::key_of(&object, &[\"id\"])?);",
            "        if model.is_instance_of::<crate::model::product::Product>() {",
            "            Self::one_value(table, pk)?;",
            "py.import(\"logging\")?.call_method1(\"getLogger\", (\"proto.data\",))?;",
            "format!(\"request_id={request_id} user_id={user_id} {what}{detail}: {result}\")",
            "logger.call_method1(\"info\", (Self::one_line(&line),))?;",
            "if error.is_instance_of::<crate::mapper::python::ProtoError>(py) {",
        ] {
            assert!(out.contains(needed), "missing {needed}\n{out}");
        }

        // A composite key: every column, in order, both where `find`
        // reads it and where the log line reads it back off the row.
        let mut model = fixture::product();
        model.table.primary_key = vec!["id".into(), "slug".into()];
        let out = render(&[model]);
        assert!(
            out.contains("        \"shop.product\" | \"product\" => Ok(&[\"id\", \"slug\"]),"),
            "{out}"
        );
        assert!(
            out.contains("Self::key_of(&object, &[\"id\", \"slug\"])?"),
            "{out}"
        );

        // A table without a key cannot be found by one, and says so;
        // it takes no store or update either.
        let mut model = fixture::product();
        model.table.primary_key.clear();
        let out = render(&[model]);
        assert!(
            out.contains(
                "        \"shop.product\" | \"product\" => Err(OperationError::new_err(\"`shop.product` has no key\")),"
            ),
            "{out}"
        );
        assert!(
            !out.contains("is_instance_of::<crate::model::product::Product>"),
            "{out}"
        );
    }

    #[test]
    fn a_bare_name_routes_only_where_it_is_unambiguous() {
        let mut other = fixture::product();
        other.table.schema = "outlet".into();
        let shop_models = [fixture::product()];
        let outlet_models = [other];
        let generate = Generate::default();
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = true;
        let sources = [
            shop(&shop_models),
            Source {
                schema: "outlet".into(),
                models: &outlet_models,
                mapper_path: "crate::mapper::outlet".into(),
                model_path: "crate::model::outlet".into(),
            },
        ];
        let out = data_file(&sources, &opts, true).code;
        assert!(
            out.contains("            \"shop.product\" => self.shop_product(request).await,"),
            "{out}"
        );
        assert!(
            out.contains("            \"outlet.product\" => self.outlet_product(request).await,"),
            "{out}"
        );
        assert!(!out.contains("| \"product\""), "{out}");
        assert!(
            out.contains("crate::mapper::outlet::product::ProductMapper"),
            "{out}"
        );
        // Two tables of one name are two variants, told apart by schema.
        assert!(
            out.contains("    ShopProduct(Vec<crate::model::product::Product>),"),
            "{out}"
        );
        assert!(
            out.contains("    OutletProduct(Vec<crate::model::outlet::product::Product>),"),
            "{out}"
        );

        // A database run names every table by its schema, however
        // unambiguous the bare name would be.
        let out = data_file(&sources[..1], &opts, false).code;
        assert!(
            out.contains("            \"shop.product\" => self.shop_product(request).await,"),
            "{out}"
        );
        assert!(!out.contains("| \"product\""), "{out}");
    }

    #[test]
    fn handler_names_cannot_collide_and_carry_no_raw_prefix() {
        let mut a = fixture::product();
        a.table.schema = "foo_bar".into();
        a.table.name = "baz".into();
        let mut b = fixture::product();
        b.table.schema = "foo".into();
        b.table.name = "bar_baz".into();
        let mut keyword = fixture::product();
        keyword.table.name = "type".into();
        let models = [a, b, keyword];
        let out = render(&models);
        syn::parse_file(&out).expect("data.rs parses");
        assert!(out.contains("=> self.foo_bar_baz(request).await,"), "{out}");
        assert!(
            out.contains("=> self.foo_bar_baz_2(request).await,"),
            "{out}"
        );
        assert!(out.contains("    async fn foo_bar_baz_2("), "{out}");
        assert!(out.contains("    FooBarBaz2(Vec<"), "{out}");
        assert!(out.contains("=> self.shop_type(request).await,"), "{out}");
        // `r#where` is a field and `r#type` a module path; the handler
        // name itself must not carry the prefix, nor the variant.
        assert!(!out.contains("shop_r#"), "{out}");
        assert!(out.contains("    ShopType(Vec<"), "{out}");
    }

    #[test]
    fn a_table_named_data_would_shadow_the_class() {
        let mut data = fixture::product();
        data.table.name = "data".into();
        let models = [fixture::product(), data];
        assert_eq!(
            crate::render::reserved_class(&models, "Data").as_deref(),
            Some("table shop.data")
        );
        assert_eq!(crate::render::reserved_class(&models[..1], "Data"), None);
    }

    #[test]
    fn a_table_without_writers_says_so_by_name() {
        let mut view = fixture::product();
        view.table.kind = crate::introspect::RelKind::View;
        view.table.name = "catalog".into();
        let models = [view];
        let out = render(&models);
        for op in ["Create", "Update", "Delete"] {
            assert!(
                out.contains(&format!(
                    "        Op::{op} => Err(Error::Cannot {{ table: \"shop.catalog\".to_string(), op: Op::{op} }}),"
                )),
                "{out}"
            );
        }
        assert!(out.contains("        Op::Find => Ok("), "{out}");
        // No input, no patch: the view has nothing to write.
        assert!(out.contains("pub enum Values {\n}"), "{out}");
        assert!(!out.contains("Patch"), "{out}");
        // A file over views alone has nothing to read `values` for, and
        // one over full tables nothing left unused: neither helper is
        // written unused, which would warn under the feature.
        assert!(!out.contains("fn values<"), "{out}");
        assert!(!out.contains("fn fixed("), "{out}");
        assert!(out.contains("            let values = None;"), "{out}");
        let full = render(&[fixture::product()]);
        assert!(full.contains("    fn values<"), "{full}");
        assert!(full.contains("    fn fixed("), "{full}");
    }

    #[test]
    fn the_file_reconciles_to_itself() {
        let models = [fixture::product(), fixture::category()];
        let out = render(&models);
        assert_eq!(
            crate::reconcile::reconcile(&out, &out).as_deref(),
            Some(out.as_str())
        );
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        assert!(mod_file(&opts).ends_with("pub mod data;\n"));
        assert!(!mod_file(&opts).contains("cfg"));
    }
}
