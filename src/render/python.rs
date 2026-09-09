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
        [only] => code.push_str(&flat(name, only)),
        many => code.push_str(&nested(name, many)),
    }
    code
}

fn flat(name: &str, group: &Group) -> String {
    let classes = registrations(group, "m", 4);
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

fn nested(name: &str, groups: &[Group]) -> String {
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
        let classes = registrations(group, "child", 4);
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
/// other.
fn registrations(group: &Group, target: &str, by: usize) -> String {
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
    }
    out
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
    fn a_schema_without_enums_registers_only_structs() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let models = [fixture::awkward()];
        let out = pymodule_file("odd", &[group("odd", "super", &models)], &opts);
        assert!(!out.contains("::enums::"), "{out}");
        assert!(out.contains("super::order::Order"), "{out}");
    }
}
