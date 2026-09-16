//! The operation layer: what an integrating engineer would otherwise
//! write on top of the mappers. It is generated because the routing —
//! a table name to a mapper, a request's values to that mapper's column
//! types — is what the type map already knows, and what drifts first
//! when the schema moves.
//!
//! `Data` is the first operation: one request dict in, one result out,
//! through whichever mapper the request names. It takes its request
//! from Python, so it is written under the pyo3 feature, beside the
//! mappers' bridge it stands on.

use super::plan::{self, Kind};
use super::{OWNED, Opts, Rendered, escape, header, indent};
use crate::introspect::Model;
use crate::naming;

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
// the schema — `route` above all, which is the routing table and is not
// yours to edit. Everything else in this file is yours: an operation of
// your own beside `Data` stays where you put it.

";

/// An identifier for composing a longer one: `naming::ident` without
/// the `r#` a keyword gets, which has no place inside a name.
fn bare_ident(name: &str) -> String {
    naming::ident(name).trim_start_matches("r#").to_string()
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

    let bridge = format!("{}::python", opts.mapper_path);
    let owned = indent(OWNED, 4);

    // Bare names route only where one table has the name.
    let mut bare: std::collections::BTreeMap<&str, usize> = Default::default();
    for source in sources {
        for model in source.models {
            *bare.entry(model.table.name.as_str()).or_default() += 1;
        }
    }

    let mut arms = String::new();
    let mut key_arms = String::new();
    let mut store_arms = String::new();
    let mut update_arms = String::new();
    let mut handlers = String::new();
    let mut needs_values = false;
    let mut needs_fixed = false;
    let mut needs_cannot = false;
    let mut taken = std::collections::BTreeSet::new();
    for source in sources {
        for model in source.models {
            let table = &model.table;
            let ops = plan::operations(table);
            let has = |kind: Kind| ops.iter().any(|op| op.kind == kind);
            needs_values |= has(Kind::Insert) || has(Kind::Update);
            needs_fixed |= has(Kind::Update);
            needs_cannot |= !(has(Kind::Insert) && has(Kind::Update) && has(Kind::Delete));
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
            arms.push_str(&format!(
                "        {pattern} => Self::{handler}(py, db, request),\n"
            ));
            // `find` addresses a row by the table's key; a table
            // without one cannot be found that way, and says so.
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
            // `store` and `update` go by the model's type: the table
            // whose input, or whose row, it is.
            let row = naming::pascal_case(&table.name);
            let module = naming::ident(&table.name);
            let mapper = format!("{}::{module}", source.mapper_path);
            let model_path = format!("{}::{module}", source.model_path);
            if has(Kind::Insert) {
                store_arms.push_str(&format!(
                    r#"    if model.is_instance_of::<{model_path}::New{row}>() {{
        let new: {model_path}::New{row} = model.extract()?;
        let mapper = {mapper}::{row}Mapper::new(&db.pool);
        let row = run(db, py, mapper.create(&new)).map_err(|e| Self::database(py, e))?;
        return Ok(row.into_pyobject(py)?.into_any().unbind());
    }}
"#
                ));
            }
            if has(Kind::Update) {
                update_arms.push_str(&format!(
                    r#"    if model.is_instance_of::<{model_path}::{row}>() {{
        let row: {model_path}::{row} = model.extract()?;
        let mapper = {mapper}::{row}Mapper::new(&db.pool);
        let row = run(db, py, mapper.update(&row)).map_err(|e| Self::database(py, e))?;
        return Ok(row.into_pyobject(py)?.into_any().unbind());
    }}
"#
                ));
            }
            handlers.push_str(&indent(&handler_fn(model, source, &handler, opts), 4));
        }
    }

    // Only what some handler calls, or the file warns under the feature.
    // Methods rather than free functions, so that when the tables change
    // and one is no longer called, reconcile takes it back.
    let mut helpers = String::new();
    if needs_values {
        helpers.push_str(&format!(
            r#"
/// The `values` an operation needs.
{OWNED}fn values<'r, 'py>(request: &'r Request<'py>, op: &str) -> PyResult<&'r Bound<'py, PyDict>> {{
    request
        .values
        .as_ref()
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err(format!("`{{op}}` needs `values`")))
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
        "`{{column}}` of {{table}} is its key or the database's own; `update` cannot set it"
    ))
}}
"#
        ));
    }
    if needs_cannot {
        helpers.push_str(&format!(
            r#"
/// `ValueError`: the table has no such operation.
{OWNED}fn cannot(table: &str, op: &str) -> PyErr {{
    pyo3::exceptions::PyValueError::new_err(format!("{{table}} has no `{{op}}`"))
}}
"#
        ));
    }
    let helpers = indent(&helpers, 4);

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

/// `find` behind its log line: the key as a `where`, through `route`,
/// and the one row out of what it finds.
{OWNED}fn find_one(py: Python<'_>, db: &Database, table: &str, pk: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {{
    let columns = Self::key(table)?;
    let conditions = PyDict::new(py);
    if let [column] = columns {{
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
            conditions.set_item(column, part)?;
        }}
    }}
    let request = Request {{
        op: Op::Find,
        r#where: conditions,
        order_by: None,
        limit: Some(1),
        offset: None,
        values: None,
    }};
    let rows = Self::route(py, db, table, &request).map_err(|e| Self::database(py, e))?;
    let rows = rows.into_bound(py).downcast_into::<pyo3::types::PyList>()?;
    Ok(match rows.len() {{
        0 => py.None(),
        _ => rows.get_item(0)?.unbind(),
    }})
}}

/// `store` by the model's type: the table whose input it is.
{OWNED}fn store(py: Python<'_>, db: &Database, model: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {{
{store_arms}    Err(OperationError::new_err(format!(
        "`{{}}` is not an input Data knows",
        Self::type_name(model)
    )))
}}

/// `update` by the model's type: the table whose row it is.
{OWNED}fn update(py: Python<'_>, db: &Database, model: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {{
{update_arms}    Err(OperationError::new_err(format!(
        "`{{}}` is not a row Data knows",
        Self::type_name(model)
    )))
}}

/// The outcome, logged: one line to the `proto.data` logger with the
/// ids that trace a request, INFO when it went through and ERROR, with
/// the error, when it did not. The outcome is handed on either way.
{OWNED}fn logged(
    py: Python<'_>,
    user_id: &str,
    request_id: &str,
    what: &str,
    outcome: PyResult<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {{
    let logger = py.import("logging")?.call_method1("getLogger", ("proto.data",))?;
    match outcome {{
        Ok(value) => {{
            let result = if value.is_none(py) {{ "not found" }} else {{ "ok" }};
            logger.call_method1(
                "info",
                (format!("request_id={{request_id}} user_id={{user_id}} {{what}}: {{result}}"),),
            )?;
            Ok(value)
        }}
        Err(error) => {{
            logger.call_method1(
                "error",
                (format!("request_id={{request_id}} user_id={{user_id}} {{what}}: failed: {{error}}"),),
            )?;
            Err(error)
        }}
    }}
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

    code.push_str(&format!(
        r#"use pyo3::prelude::*;
use pyo3::types::PyDict;

use {bridge}::{{Database, run}};

pyo3::create_exception!(
    proto,
    OperationError,
    pyo3::exceptions::PyException,
    "What an operation raises unless it has a more specific error: a table `Data` does not route to, a model it does not know, a table with no key."
);
pyo3::create_exception!(
    proto,
    DatabaseError,
    OperationError,
    "The database refused or failed, in the driver's words."
);
pyo3::create_exception!(
    proto,
    PermissionError,
    OperationError,
    "The caller may not do this. Nothing generated raises it; it is here for an operation of your own."
);

/// What a request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {{
    Find,
    Count,
    Create,
    Update,
    Delete,
}}

impl Op {{
    /// The operation a request names.
{owned}    fn parse(op: &str) -> PyResult<Self> {{
        Ok(match op {{
            "find" => Self::Find,
            "count" => Self::Count,
            "create" => Self::Create,
            "update" => Self::Update,
            "delete" => Self::Delete,
            other => {{
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "`{{other}}` is not an operation: find, count, create, update or delete"
                )));
            }}
        }})
    }}
}}

/// One request, read from its dict.
struct Request<'py> {{
    op: Op,
    r#where: Bound<'py, PyDict>,
    order_by: Option<Bound<'py, PyAny>>,
    limit: Option<i64>,
    offset: Option<i64>,
    values: Option<Bound<'py, PyDict>>,
}}

/// One request dict in, one result out, through whichever mapper the
/// request names.
///
/// ```python
/// data = Data(db)
/// rows = data.execute({{"table": "shop.product", "op": "find",
///                      "where": {{"status": "active"}}, "limit": 20}})
/// ```
#[pyclass(frozen)]
pub struct Data {{
    db: Py<Database>,
}}

#[pymethods]
impl Data {{
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
        let request = Request {{
            op: Op::parse(&op)?,
            r#where: match given(request, "where")? {{
                Some(conditions) => conditions.downcast_into::<PyDict>()?,
                None => PyDict::new(py),
            }},
            order_by: given(request, "order_by")?,
            limit: given(request, "limit")?.map(|n| n.extract()).transpose()?,
            offset: given(request, "offset")?.map(|n| n.extract()).transpose()?,
            values: given(request, "values")?
                .map(|v| v.downcast_into::<PyDict>())
                .transpose()?,
        }};
        Routes::route(py, self.db.get(), &table, &request)
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
fn required<'py>(request: &Bound<'py, PyDict>, key: &str) -> PyResult<Bound<'py, PyAny>> {{
    request
        .get_item(key)?
        .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(format!("a request needs `{{key}}`")))
}}

/// A key the request may carry; `None` in the dict counts as absent.
fn given<'py>(request: &Bound<'py, PyDict>, key: &str) -> PyResult<Option<Bound<'py, PyAny>>> {{
    Ok(request.get_item(key)?.filter(|v| !v.is_none()))
}}

/// The routing table, and one handler per table behind it.
struct Routes;

impl Routes {{
    /// Every table `Data` serves, by `schema.table`, or the bare name
    /// where only one table has it. This is the routing table: proto
    /// rewrites it whenever a table arrives or leaves, so a route added
    /// by hand is lost on the next run, by design.
{owned}    fn route(
        py: Python<'_>,
        db: &Database,
        table: &str,
        request: &Request<'_>,
    ) -> PyResult<Py<PyAny>> {{
        match table {{
{arms}            _ => Err(pyo3::exceptions::PyKeyError::new_err(format!(
                "`{{table}}` is not a table Data routes to"
            ))),
        }}
    }}
{helpers}{by_key}{handlers}}}

/// Register `Data` and the operation errors on a module. Call it after
/// the mappers' `register`, which puts the `Database` it takes and the
/// classes it returns there.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {{
    m.add_class::<Data>()?;
    m.add("OperationError", m.py().get_type::<OperationError>())?;
    m.add("DatabaseError", m.py().get_type::<DatabaseError>())?;
    m.add("PermissionError", m.py().get_type::<PermissionError>())
}}
"#
    ));

    Rendered {
        code,
        warnings: Vec::new(),
    }
}

/// One table's handler: the mapper, and each operation through it.
fn handler_fn(model: &Model, source: &Source, handler: &str, opts: &Opts) -> String {
    let table = &model.table;
    let row = naming::pascal_case(&table.name);
    let module = naming::ident(&table.name);
    let mapper = format!("{}::{module}", source.mapper_path);
    let model_path = format!("{}::{module}", source.model_path);
    let qualified = escape(&format!("{}.{}", table.schema, table.name));
    let ops = plan::operations(table);
    let has = |kind: Kind| ops.iter().any(|op| op.kind == kind);

    let create = if has(Kind::Insert) {
        format!(
            r#"        Op::Create => {{
            let values = Self::values(request, "create")?;
            let new: {model_path}::New{row} = py
                .get_type::<{model_path}::New{row}>()
                .call((), Some(values))?
                .extract()?;
            let row = run(db, py, mapper.create(&new))?;
            Ok(row.into_pyobject(py)?.into_any().unbind())
        }}
"#
        )
    } else {
        format!("        Op::Create => Err(Self::cannot(\"{qualified}\", \"create\")),\n")
    };

    let update = if has(Kind::Update) {
        // What `update` writes is what the mapper's `update` writes;
        // the key addresses the row and the rest is the database's.
        let updatable = table.update_columns();
        let fixed: Vec<String> = table
            .columns
            .iter()
            .filter(|c| !updatable.iter().any(|u| u.name == c.name))
            .map(|c| format!("\"{}\"", escape(&c.name)))
            .collect();
        let refuse = if fixed.is_empty() {
            String::new()
        } else {
            format!(
                r#"                    if matches!(key.to_str()?, {}) {{
                        return Err(Self::fixed("{qualified}", key.to_str()?));
                    }}
"#,
                fixed.join(" | ")
            )
        };
        format!(
            r#"        Op::Update => {{
            let values = Self::values(request, "update")?;
            let rows = run(db, py, mapper.find_where(conditions()?))?;
            let mut updated = Vec::with_capacity(rows.len());
            for row in rows {{
                let object = Py::new(py, row)?;
                for (key, value) in values.iter() {{
                    let key = key.downcast_into::<pyo3::types::PyString>()?;
{refuse}                    object.bind(py).setattr(&key, value)?;
                }}
                let row: {model_path}::{row} = object.extract(py)?;
                updated.push(run(db, py, mapper.update(&row))?);
            }}
            Ok(updated.into_pyobject(py)?.into_any().unbind())
        }}
"#
        )
    } else {
        format!("        Op::Update => Err(Self::cannot(\"{qualified}\", \"update\")),\n")
    };

    let delete = if has(Kind::Delete) {
        let key = table
            .primary_key_columns()
            .iter()
            .map(|c| {
                let field = naming::ident(&c.name);
                // Borrowed exactly where the mapper's signature borrows.
                let mapped = crate::typemap::map(&c.ty, opts.generate);
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
            let rows = run(db, py, mapper.find_where(conditions()?))?;
            let deleted = rows.len();
            for row in rows {{
                run(db, py, mapper.delete({key}))?;
            }}
            Ok(deleted.into_pyobject(py)?.into_any().unbind())
        }}
"#
        )
    } else {
        format!("        Op::Delete => Err(Self::cannot(\"{qualified}\", \"delete\")),\n")
    };

    format!(
        r#"
/// `{qualified}`, through its mapper.
{OWNED}fn {handler}(py: Python<'_>, db: &Database, request: &Request<'_>) -> PyResult<Py<PyAny>> {{
    let mapper = {mapper}::{row}Mapper::new(&db.pool);
    let conditions = || {{
        {mapper}::query_from_python(
            &request.r#where,
            request.order_by.as_ref(),
            request.limit,
            request.offset,
        )
    }};
    match request.op {{
        Op::Find => {{
            let rows = run(db, py, mapper.find_where(conditions()?))?;
            Ok(rows.into_pyobject(py)?.into_any().unbind())
        }}
        Op::Count => {{
            let query = {mapper}::query_from_python(&request.r#where, None, None, None)?;
            let n = run(db, py, mapper.count_where(query))?;
            Ok(n.into_pyobject(py)?.into_any().unbind())
        }}
{create}{update}{delete}    }}
}}
"#
    )
}

/// `mod.rs` for the operation directory: `data` behind the feature.
pub fn mod_file(opts: &Opts) -> String {
    let mut code = header(opts, "operations", "module list");
    code.push_str(&format!(
        "#[cfg(feature = \"{}\")]\npub mod data;\n",
        opts.generate.pyo3_feature
    ));
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
            "use crate::mapper::python::{Database, run};",
            "#[pyclass(frozen)]\npub struct Data {",
            "fn execute(&self, py: Python<'_>, request: &Bound<'_, PyDict>) -> PyResult<Py<PyAny>> {",
            "        \"shop.product\" | \"product\" => Self::shop_product(py, db, request),",
            "    fn shop_product(py: Python<'_>, db: &Database, request: &Request<'_>) -> PyResult<Py<PyAny>> {",
            "        let mapper = crate::mapper::product::ProductMapper::new(&db.pool);",
            "            crate::mapper::product::query_from_python(",
            "            let new: crate::model::product::NewProduct = py",
            "                let row: crate::model::product::Product = object.extract(py)?;",
            "                run(db, py, mapper.delete(row.id))?;",
            "pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {\n    m.add_class::<Data>()?;\n",
            "    m.add(\"PermissionError\", m.py().get_type::<PermissionError>())\n}",
        ] {
            assert!(out.contains(needed), "missing {needed}\n{out}");
        }
        // The routing table and every handler say proto owns them.
        let methods = out.matches("    fn ").count();
        assert_eq!(
            out.matches("proto owns this method").count(),
            methods,
            "{out}"
        );
        assert!(
            out.contains("is the routing table and is not\n// yours to edit"),
            "{out}"
        );
    }

    /// `find` goes by the table's key, `store` and `update` by the
    /// model's type, and each is one log line with the ids in it.
    #[test]
    fn find_store_and_update_go_by_key_and_by_type() {
        let models = [fixture::product()];
        let out = render(&models);
        syn::parse_file(&out).expect("data.rs parses");
        for needed in [
            "pyo3::create_exception!(\n    proto,\n    OperationError,\n    pyo3::exceptions::PyException,",
            "pyo3::create_exception!(\n    proto,\n    DatabaseError,\n    OperationError,",
            "pyo3::create_exception!(\n    proto,\n    PermissionError,\n    OperationError,",
            "    fn find(\n        &self,\n        py: Python<'_>,\n        table: &str,\n        pk: &Bound<'_, PyAny>,\n        user_id: &str,\n        request_id: &str,\n",
            "        let what = format!(\"find {table} pk={pk}\");",
            "    fn store(\n        &self,\n        py: Python<'_>,\n        model: &Bound<'_, PyAny>,\n",
            "    fn update(\n        &self,\n        py: Python<'_>,\n        model: &Bound<'_, PyAny>,\n",
            "        Routes::logged(py, user_id, request_id, &what, outcome)",
            "        \"shop.product\" | \"product\" => Ok(&[\"id\"]),",
            "        if model.is_instance_of::<crate::model::product::NewProduct>() {",
            "            let row = run(db, py, mapper.create(&new)).map_err(|e| Self::database(py, e))?;",
            "        if model.is_instance_of::<crate::model::product::Product>() {",
            "            let row = run(db, py, mapper.update(&row)).map_err(|e| Self::database(py, e))?;",
            "py.import(\"logging\")?.call_method1(\"getLogger\", (\"proto.data\",))?;",
            "format!(\"request_id={request_id} user_id={user_id} {what}: {result}\")",
            "format!(\"request_id={request_id} user_id={user_id} {what}: failed: {error}\")",
            "if error.is_instance_of::<crate::mapper::python::ProtoError>(py) {",
        ] {
            assert!(out.contains(needed), "missing {needed}\n{out}");
        }

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
            out.contains("        \"shop.product\" => Self::shop_product(py, db, request),"),
            "{out}"
        );
        assert!(
            out.contains("        \"outlet.product\" => Self::outlet_product(py, db, request),"),
            "{out}"
        );
        assert!(!out.contains("| \"product\""), "{out}");
        assert!(
            out.contains("crate::mapper::outlet::product::ProductMapper"),
            "{out}"
        );

        // A database run names every table by its schema, however
        // unambiguous the bare name would be.
        let out = data_file(&sources[..1], &opts, false).code;
        assert!(
            out.contains("        \"shop.product\" => Self::shop_product(py, db, request),"),
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
        assert!(
            out.contains("=> Self::foo_bar_baz(py, db, request),"),
            "{out}"
        );
        assert!(
            out.contains("=> Self::foo_bar_baz_2(py, db, request),"),
            "{out}"
        );
        assert!(out.contains("    fn foo_bar_baz_2("), "{out}");
        assert!(
            out.contains("=> Self::shop_type(py, db, request),"),
            "{out}"
        );
        // `r#where` is a field and `r#type` a module path; the handler
        // name itself must not carry the prefix.
        assert!(!out.contains("shop_r#"), "{out}");
    }

    #[test]
    fn update_refuses_the_key_and_what_the_database_owns() {
        let out = render(&[fixture::product()]);
        // shop.product: `id` has a server default, `created_at` too.
        assert!(
            out.contains("if matches!(key.to_str()?, \"id\" | \"created_at\") {"),
            "{out}"
        );
        assert!(
            out.contains("    fn fixed(table: &str, column: &str) -> PyErr {"),
            "{out}"
        );
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
        assert!(
            out.contains("Op::Create => Err(Self::cannot(\"shop.catalog\", \"create\")),"),
            "{out}"
        );
        assert!(
            out.contains("Op::Update => Err(Self::cannot(\"shop.catalog\", \"update\")),"),
            "{out}"
        );
        assert!(
            out.contains("Op::Delete => Err(Self::cannot(\"shop.catalog\", \"delete\")),"),
            "{out}"
        );
        assert!(out.contains("Op::Find => {"), "{out}");
        // A file over views alone has nothing to read `values` for, and
        // one over full tables nothing to refuse: neither helper is
        // written unused, which would warn under the feature.
        assert!(out.contains("    fn cannot("), "{out}");
        assert!(!out.contains("fn values<"), "{out}");
        assert!(!out.contains("fn fixed("), "{out}");
        let full = render(&[fixture::product()]);
        assert!(full.contains("    fn values<"), "{full}");
        assert!(full.contains("    fn fixed("), "{full}");
        assert!(!full.contains("fn cannot("), "{full}");
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
        assert!(mod_file(&opts).contains("#[cfg(feature = \"python\")]\npub mod data;\n"));
    }
}
