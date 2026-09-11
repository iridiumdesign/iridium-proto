//! The `#[pymodule]` block: every generated class registered with Python,
//! so a consumer builds a cdylib and imports it without writing glue.
//!
//! The function pyo3 turns into `PyInit_<name>` does not have to live at
//! the crate root — a nested module exports it just as well — so this
//! file goes beside the models it registers, and the generated `mod.rs`
//! declares it behind the pyo3 feature.
//!
//! Classes are named by full path rather than imported. Two schemas can
//! each hold a `species` table, and an import list would collide where a
//! path cannot.

use super::{Opts, header};
use crate::introspect::Model;
use crate::naming;
use crate::render;

/// One schema's worth of models, and the module path they sit under.
pub struct Group<'a> {
    /// The schema name, which becomes the Python submodule name when
    /// there is more than one.
    pub schema: String,
    /// Path prefix the classes are reached by, e.g. `super::accounts`.
    pub path: String,
    /// The models registered under that path.
    pub models: &'a [Model],
}

/// Render the module that registers every class.
///
/// A single group becomes one flat module. Several become a parent with a
/// submodule each, because two schemas may hold tables of the same name
/// and Python has one namespace per module.
pub fn pymodule_file(name: &str, groups: &[Group], opts: &Opts) -> String {
    let source = groups
        .iter()
        .map(|g| g.schema.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut code = header(opts, &source, "python module");
    code.push_str("use pyo3::prelude::*;\n\n");

    match groups {
        [only] => code.push_str(&flat(name, only, opts.inputs)),
        many => code.push_str(&nested(name, many, opts.inputs)),
    }
    code
}

fn flat(name: &str, group: &Group, inputs: bool) -> String {
    let classes = registrations(group, "m", 4, inputs);
    format!(
        r#"/// Register every generated class in `{}` on a module.
///
/// Call this from your own `#[pymodule]` when the extension needs
/// functions of its own — an extension has room for exactly one module
/// initialiser, so a generated one cannot be added to.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {{
{classes}    Ok(())
}}

/// The classes as an extension module, for when that is all you need.
///
/// The function name is what Python imports, so it has to match the
/// `[lib] name` of the crate this is built into.
#[pymodule]
pub fn {name}(m: &Bound<'_, PyModule>) -> PyResult<()> {{
    register(m)
}}
"#,
        group.schema
    )
}

fn nested(name: &str, groups: &[Group], inputs: bool) -> String {
    let calls: String = groups
        .iter()
        .map(|g| format!("    {}(m)?;\n", naming::ident(&g.schema)))
        .collect();

    let mut code = format!(
        r#"/// Register every generated class on a module, one submodule per
/// schema.
///
/// Call this from your own `#[pymodule]` when the extension needs
/// functions of its own — an extension has room for exactly one module
/// initialiser, so a generated one cannot be added to.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {{
{calls}    Ok(())
}}

/// The classes as an extension module, for when that is all you need.
///
/// The function name is what Python imports, so it has to match the
/// `[lib] name` of the crate this is built into.
#[pymodule]
pub fn {name}(m: &Bound<'_, PyModule>) -> PyResult<()> {{
    register(m)
}}
"#
    );

    for group in groups {
        let module = naming::ident(&group.schema);
        let schema = &group.schema;
        let classes = registrations(group, "child", 4, inputs);
        code.push_str(&format!(
            r#"
/// The `{schema}` schema.
fn {module}(parent: &Bound<'_, PyModule>) -> PyResult<()> {{
    let child = PyModule::new(parent.py(), "{module}")?;
{classes}    parent.add_submodule(&child)?;

    // `add_submodule` only sets an attribute on the parent. Registering
    // it in `sys.modules` too is what makes `import {name}.{module}` and
    // `from {name}.{module} import ...` work.
    parent
        .py()
        .import("sys")?
        .getattr("modules")?
        .set_item("{name}.{module}", &child)?;
    Ok(())
}}
"#
        ));
    }
    code
}

/// `m.add_class::<super::item::Item>()?;` for every class in a group.
///
/// Enums come first: a struct field is typed by one, and reading the
/// registrations in that order matches how the types depend on each
/// other. With `inputs`, each writable table's `New…` follows its row.
fn registrations(group: &Group, target: &str, by: usize, inputs: bool) -> String {
    let pad = " ".repeat(by);
    let path = &group.path;
    let mut out = String::new();

    if render::uses_enums(group.models) {
        for e in dedupe(group.models) {
            let name = naming::pascal_case(&e);
            out.push_str(&format!(
                "{pad}{target}.add_class::<{path}::enums::{name}>()?;\n"
            ));
        }
    }
    for model in group.models {
        let module = naming::ident(&model.table.name);
        let name = naming::pascal_case(&model.table.name);
        out.push_str(&format!(
            "{pad}{target}.add_class::<{path}::{module}::{name}>()?;\n"
        ));
        if inputs && model.table.writable() && !model.table.insert_columns().is_empty() {
            out.push_str(&format!(
                "{pad}{target}.add_class::<{path}::{module}::New{name}>()?;\n"
            ));
        }
    }
    out
}

// ── The mapper side ─────────────────────────────────────────────────────────

/// The bridge the mappers' Python classes stand on: a `Database` holding
/// a pool and the runtime that drives it, the one exception every
/// database error becomes, and a `register` for the mapper classes.
///
/// This goes beside the mappers, so a mapper finds it at
/// [`Opts::bridge_path`](super::Opts::bridge_path). It has no
/// `#[pymodule]` of its own: an extension has room for one initialiser,
/// and the model side may already have written it. Call this `register`
/// from yours, after the models'.
pub fn bridge_file(groups: &[Group], opts: &Opts) -> String {
    let source = groups
        .iter()
        .map(|g| g.schema.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut code = header(opts, &source, "python bridge");

    // One schema registers its mapper classes flat. Several put each
    // schema's in a submodule, as the models' module does: two schemas
    // may each hold a `product`, and one namespace cannot hold both
    // `ProductMapper`s.
    let (classes, submodules) = match groups {
        [only] => (mapper_registrations(only, "m"), String::new()),
        many => (
            many.iter()
                .map(|g| format!("    {}(m)?;\n", naming::ident(&g.schema)))
                .collect(),
            many.iter().map(mapper_submodule).collect(),
        ),
    };

    code.push_str(&format!(
        r#"use pyo3::prelude::*;
use sqlx::PgPool;

pyo3::create_exception!(
    proto,
    ProtoError,
    pyo3::exceptions::PyException,
    "A database error, carrying the message the driver gave."
);

/// A connection pool and the runtime that drives it.
///
/// The generated mappers are `async`; Python, from here, is not. Each
/// call runs to completion on this runtime with the GIL released, so
/// other Python threads keep going meanwhile.
#[pyclass(frozen)]
pub struct Database {{
    /// The pool, for a mapper to borrow.
    pub pool: PgPool,
    runtime: tokio::runtime::Runtime,
}}

#[pymethods]
impl Database {{
    /// Connect. The URL is what sqlx takes: `postgres://user@host/db`.
    ///
    /// The handshake runs with the GIL released, like every call after
    /// it; only the URL is read while it is held.
    #[new]
    fn new(py: Python<'_>, url: &str) -> PyResult<Self> {{
        let url = url.to_string();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| ProtoError::new_err(e.to_string()))?;
        let pool = py
            .detach(|| runtime.block_on(PgPool::connect(&url)))
            .map_err(error)?;
        Ok(Self {{ pool, runtime }})
    }}
}}

/// Run a mapper call to completion on `db`, releasing the GIL meanwhile.
///
/// A free function rather than a method: the class has one `impl` block,
/// the `#[pymethods]`, and everything in that block is Python's.
///
/// # Errors
///
/// The database error as a `ProtoError`.
pub fn run<T: Send>(
    db: &Database,
    py: Python<'_>,
    future: impl std::future::Future<Output = Result<T, sqlx::Error>> + Send,
) -> PyResult<T> {{
    py.detach(|| db.runtime.block_on(future)).map_err(error)
}}

/// A database error as Python sees it.
pub fn error(e: sqlx::Error) -> PyErr {{
    ProtoError::new_err(e.to_string())
}}

/// Register `Database`, `ProtoError`, and every generated mapper class
/// on a module. Call it after the models' own `register`, which the
/// mappers' return types depend on.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {{
    m.add("ProtoError", m.py().get_type::<ProtoError>())?;
    m.add_class::<Database>()?;
{classes}    Ok(())
}}
{submodules}"#
    ));
    code
}

/// `<target>.add_class::<super::item::PyItemMapper>()?;` for every
/// mapper in a group.
fn mapper_registrations(group: &Group, target: &str) -> String {
    let path = &group.path;
    let mut out = String::new();
    for model in group.models {
        let module = naming::ident(&model.table.name);
        let name = naming::pascal_case(&model.table.name);
        out.push_str(&format!(
            "    {target}.add_class::<{path}::{module}::Py{name}Mapper>()?;\n"
        ));
    }
    out
}

/// One schema's mapper classes, registered on its submodule. The models'
/// `register` runs first and has usually made that submodule already, so
/// this adds to it rather than replacing it; when it has not, the
/// submodule is made and put in `sys.modules` the same way.
fn mapper_submodule(group: &Group) -> String {
    let module = naming::ident(&group.schema);
    let schema = &group.schema;
    let classes = mapper_registrations(group, "child");
    format!(
        r#"
/// The `{schema}` schema's mappers.
fn {module}(parent: &Bound<'_, PyModule>) -> PyResult<()> {{
    let child = match parent.getattr("{module}") {{
        Ok(existing) => existing.downcast_into::<PyModule>()?,
        Err(_) => {{
            let child = PyModule::new(parent.py(), "{module}")?;
            parent.add_submodule(&child)?;
            parent
                .py()
                .import("sys")?
                .getattr("modules")?
                .set_item(format!("{{}}.{module}", parent.name()?), &child)?;
            child
        }}
    }};
{classes}    Ok(())
}}
"#
    )
}

fn dedupe(models: &[Model]) -> Vec<String> {
    let mut seen = Vec::new();
    for model in models {
        for e in &model.enums {
            if !seen.contains(&e.name) {
                seen.push(e.name.clone());
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::super::{Strategy, fixture};
    use super::*;
    use crate::config::Generate;

    fn group<'a>(schema: &str, path: &str, models: &'a [Model]) -> Group<'a> {
        Group {
            schema: schema.to_string(),
            path: path.to_string(),
            models,
        }
    }

    #[test]
    fn one_schema_registers_flat() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let models = [fixture::product()];
        let out = pymodule_file("shop", &[group("shop", "super", &models)], &opts);

        assert!(
            out.contains("pub fn register(m: &Bound<'_, PyModule>)"),
            "{out}"
        );
        assert!(out.contains("#[pymodule]\npub fn shop("), "{out}");
        // Enums first: a struct field is typed by one.
        let enum_at = out.find("super::enums::ProductStatus").unwrap();
        let struct_at = out.find("super::product::Product").unwrap();
        assert!(enum_at < struct_at, "{out}");
        // Flat means no submodule machinery.
        assert!(!out.contains("add_submodule"), "{out}");
    }

    #[test]
    fn several_schemas_get_a_submodule_each() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let shop = [fixture::product()];
        let odd = [fixture::awkward()];
        let out = pymodule_file(
            "store",
            &[
                group("shop", "super::shop", &shop),
                group("warehouse", "super::warehouse", &odd),
            ],
            &opts,
        );

        assert!(
            out.contains("fn shop(parent: &Bound<'_, PyModule>)"),
            "{out}"
        );
        assert!(
            out.contains("fn warehouse(parent: &Bound<'_, PyModule>)"),
            "{out}"
        );
        assert!(out.contains("parent.add_submodule(&child)?;"), "{out}");
        // Without the sys.modules entry, `from store.shop import X` fails.
        assert!(
            out.contains(r#".set_item("store.shop", &child)?;"#),
            "{out}"
        );
        // Full paths, so two schemas may hold the same table name.
        assert!(out.contains("super::shop::product::Product"), "{out}");
        assert!(out.contains("super::warehouse::order::Order"), "{out}");
    }

    #[test]
    fn inputs_register_beside_their_rows() {
        let generate = Generate::default();
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.inputs = false;
        let models = [fixture::product()];
        let without = pymodule_file("shop", &[group("shop", "super", &models)], &opts);
        assert!(!without.contains("NewProduct"), "{without}");

        opts.inputs = true;
        let with = pymodule_file("shop", &[group("shop", "super", &models)], &opts);
        assert!(
            with.contains(
                "m.add_class::<super::product::Product>()?;\n    \
                 m.add_class::<super::product::NewProduct>()?;"
            ),
            "{with}"
        );
    }

    #[test]
    fn the_bridge_registers_the_mappers_and_has_no_initialiser() {
        let generate = Generate::default();
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = true;
        let shop = [fixture::product()];
        let odd = [fixture::awkward()];
        let out = bridge_file(
            &[
                group("shop", "super::shop", &shop),
                group("warehouse", "super::warehouse", &odd),
            ],
            &opts,
        );
        assert!(out.contains("pub struct Database {"), "{out}");
        assert!(out.contains("ProtoError"), "{out}");
        // Two schemas: each registers its mappers on its own submodule,
        // added to if the models' register made it first. Database and
        // the exception stay at the root.
        assert!(out.contains("    shop(m)?;\n    warehouse(m)?;\n"), "{out}");
        assert!(
            out.contains("fn shop(parent: &Bound<'_, PyModule>) -> PyResult<()>"),
            "{out}"
        );
        assert!(
            out.contains("child.add_class::<super::shop::product::PyProductMapper>()?;"),
            "{out}"
        );
        assert!(
            out.contains("child.add_class::<super::warehouse::order::PyOrderMapper>()?;"),
            "{out}"
        );
        assert!(
            out.contains("existing.downcast_into::<PyModule>()?"),
            "{out}"
        );
        assert!(out.contains("m.add_class::<Database>()?;"), "{out}");
        // No prelude is assumed for the future, and the connect runs
        // with the GIL released like everything after it.
        assert!(out.contains("impl std::future::Future<Output"), "{out}");
        assert!(
            out.contains("fn new(py: Python<'_>, url: &str) -> PyResult<Self>"),
            "{out}"
        );
        assert!(
            out.contains(".detach(|| runtime.block_on(PgPool::connect(&url)))"),
            "{out}"
        );
        // One initialiser per extension, and the model side owns it.
        assert!(!out.contains("#[pymodule]"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
        // Reconciled against itself it must come back unchanged, or a
        // rerun would report the file every time. Two `impl Database`
        // blocks did exactly that once.
        assert_eq!(
            crate::reconcile::reconcile(&out, &out).as_deref(),
            Some(out.as_str())
        );
    }

    #[test]
    fn one_schema_registers_its_mappers_flat() {
        let generate = Generate::default();
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = true;
        let models = [fixture::product()];
        let out = bridge_file(&[group("shop", "super", &models)], &opts);
        assert!(
            out.contains("    m.add_class::<super::product::PyProductMapper>()?;"),
            "{out}"
        );
        assert!(!out.contains("add_submodule"), "{out}");
    }

    #[test]
    fn a_schema_without_enums_registers_only_structs() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let models = [fixture::awkward()];
        let out = pymodule_file("odd", &[group("odd", "super", &models)], &opts);
        assert!(!out.contains("::enums::"), "{out}");
        assert!(out.contains("super::order::Order"), "{out}");
    }
}
