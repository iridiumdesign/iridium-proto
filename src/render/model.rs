//! Row structs, the Rust enums behind Postgres enum types, and the insert
//! input types that go with them.

use std::collections::BTreeSet;

use super::Rendered;
use super::{
    Opts, dedupe_enums, derive_line, doc_comment, escape, has_serde, header, import_block,
};
use crate::introspect::{Column, Model, PgEnum, Table};
use crate::naming;
use crate::typemap;

/// One table as a standalone module: its enum types, its row struct,
/// and — when [`Opts::inputs`](super::Opts::inputs) is set — its insert
/// input type.
///
/// `enum_path` names the module the enum types live in. `None` defines
/// them inline, which keeps a single `proto model` self-contained; a
/// schema run passes `Some("super::enums")` so a type shared by several
/// tables is defined once.
pub fn model_file(model: &Model, opts: &Opts, enum_path: Option<&str>) -> Rendered {
    let mut imports = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut body = String::new();

    if enum_path.is_none() {
        for e in &model.enums {
            body.push_str(&enum_block(e, opts, &mut imports));
            body.push('\n');
        }
    } else if let Some(path) = enum_path {
        for e in &model.enums {
            imports.insert(format!("{path}::{}", naming::pascal_case(&e.name)));
        }
    }

    body.push_str(&struct_block(
        &model.table,
        opts,
        &mut imports,
        &mut warnings,
    ));
    if opts.inputs {
        body.push_str(&input_block(&model.table, opts, &mut imports));
    }

    let source = format!("{}.{}", model.table.schema, model.table.name);
    let mut code = header(opts, &source, model.table.kind.label());
    code.push_str(&import_block(&imports));
    code.push_str(&body);

    Rendered { code, warnings }
}

/// Every table in a schema as one flat file: shared enums once at the top,
/// then a struct per table. This is what `proto schema` writes to stdout.
pub fn schema_file(models: &[Model], schema: &str, opts: &Opts) -> Rendered {
    let mut imports = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut body = String::new();

    for e in dedupe_enums(models) {
        body.push_str(&enum_block(&e, opts, &mut imports));
        body.push('\n');
    }
    for model in models {
        body.push_str(&struct_block(
            &model.table,
            opts,
            &mut imports,
            &mut warnings,
        ));
        if opts.inputs {
            body.push_str(&input_block(&model.table, opts, &mut imports));
        }
        body.push('\n');
    }

    let mut code = header(opts, schema, "schema");
    code.push_str(&import_block(&imports));
    code.push_str(body.trim_end());
    code.push('\n');

    Rendered { code, warnings }
}

/// The enum types of a schema, for the `enums.rs` beside the model files.
pub fn enums_file(models: &[Model], schema: &str, opts: &Opts) -> Rendered {
    let mut imports = BTreeSet::new();
    let mut body = String::new();
    for e in dedupe_enums(models) {
        body.push_str(&enum_block(&e, opts, &mut imports));
        body.push('\n');
    }

    let mut code = header(opts, schema, "enum types");
    code.push_str(&import_block(&imports));
    code.push_str(body.trim_end());
    code.push('\n');

    Rendered {
        code,
        warnings: Vec::new(),
    }
}

/// A `mod.rs` declaring the generated modules.
pub fn mod_file(modules: &[String], source: &str, opts: &Opts) -> String {
    let mut code = header(opts, source, "module list");
    for m in modules {
        code.push_str(&format!("pub mod {};\n", naming::ident(m)));
    }
    code
}

// ── Blocks ──────────────────────────────────────────────────────────────────

fn struct_block(
    table: &Table,
    opts: &Opts,
    imports: &mut BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> String {
    let name = opts
        .name_override
        .clone()
        .unwrap_or_else(|| naming::pascal_case(&table.name));
    let mut out = String::new();

    if let Some(comment) = &table.comment {
        out.push_str(&doc_comment(comment, ""));
    }
    if !table.primary_key.is_empty() {
        out.push_str(&format!(
            "/// Primary key: `{}`.\n",
            table.primary_key.join("`, `")
        ));
    }
    if !table.kind.nullability_is_known() {
        if !out.is_empty() {
            out.push_str("///\n");
        }
        out.push_str(&format!(
            "/// Every column of a {} reports as nullable, so every field\n\
             /// here is `Option`. Tighten by hand where you know better.\n",
            table.kind.label()
        ));
    }

    out.push_str(&derive_line(&opts.generate.derives, imports));
    if opts.pyo3 {
        // `get_all, set_all` rather than a `#[pyo3(get, set)]` on every
        // field: pyclass never sees a field-level `cfg_attr` (it is expanded
        // after the macro runs), so per-field gating does not compile.
        out.push_str(&format!(
            "#[cfg_attr(feature = \"{}\", pyo3::pyclass(get_all, set_all))]\n",
            opts.generate.pyo3_feature
        ));
    }
    out.push_str(&format!("pub struct {name} {{\n"));

    let serde = has_serde(&opts.generate.derives);
    let mut fields: BTreeSet<String> = BTreeSet::new();
    for column in &table.columns {
        if !fields.insert(naming::ident(&column.name)) {
            warnings.push(format!(
                "{}.{}: columns `{}` and another reduce to the same field \
                 name `{}`; rename one or the generated struct will not \
                 compile",
                table.schema,
                table.name,
                column.name,
                naming::ident(&column.name)
            ));
        }
        let mapped = typemap::map(&column.ty, &opts.generate.types);
        for import in &mapped.imports {
            imports.insert(import.clone());
        }

        if let Some(unknown) = &mapped.unmapped {
            out.push_str(&format!(
                "    // TODO: unmapped Postgres type `{unknown}` — set \
                 [generate.types] {unknown} = \"...\"\n"
            ));
            warnings.push(format!(
                "{}.{}.{}: unmapped Postgres type '{unknown}', using String",
                table.schema, table.name, column.name
            ));
        }
        if let Some(comment) = &column.comment {
            out.push_str(&doc_comment(comment, "    "));
        }

        let field = naming::ident(&column.name);
        if field.trim_start_matches("r#") != column.name {
            out.push_str(&format!(
                "    #[sqlx(rename = \"{}\")]\n",
                escape(&column.name)
            ));
            if serde {
                out.push_str(&format!(
                    "    #[serde(rename = \"{}\")]\n",
                    escape(&column.name)
                ));
            }
        }
        let ty = if column.not_null {
            mapped.text
        } else {
            format!("Option<{}>", mapped.text)
        };
        out.push_str(&format!("    pub {field}: {ty},\n"));
    }

    out.push_str("}\n");
    out
}

fn enum_block(e: &PgEnum, opts: &Opts, imports: &mut BTreeSet<String>) -> String {
    let name = naming::pascal_case(&e.name);
    let variants: Vec<String> = e.labels.iter().map(|l| naming::pascal_case(l)).collect();
    // Only lean on `rename_all` when every label survives the round trip;
    // otherwise each variant carries its own literal.
    let round_trips = e
        .labels
        .iter()
        .zip(&variants)
        .all(|(label, variant)| &naming::snake_case(variant) == label);

    let mut out = format!("/// The `{}.{}` enum type.\n", e.schema, e.name);
    out.push_str(&derive_line(&opts.generate.enum_derives, imports));
    if round_trips {
        out.push_str(&format!(
            "#[sqlx(type_name = \"{}\", rename_all = \"snake_case\")]\n",
            escape(&e.name)
        ));
        if has_serde(&opts.generate.enum_derives) {
            out.push_str("#[serde(rename_all = \"snake_case\")]\n");
        }
    } else {
        out.push_str(&format!("#[sqlx(type_name = \"{}\")]\n", escape(&e.name)));
    }
    if opts.pyo3 {
        out.push_str(&format!(
            "#[cfg_attr(feature = \"{}\", pyo3::pyclass(eq, eq_int))]\n",
            opts.generate.pyo3_feature
        ));
    }
    out.push_str(&format!("pub enum {name} {{\n"));
    for (label, variant) in e.labels.iter().zip(&variants) {
        if !round_trips {
            out.push_str(&format!("    #[sqlx(rename = \"{}\")]\n", escape(label)));
            if has_serde(&opts.generate.enum_derives) {
                out.push_str(&format!("    #[serde(rename = \"{}\")]\n", escape(label)));
            }
        }
        out.push_str(&format!("    {variant},\n"));
    }
    out.push_str("}\n");
    out
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// The insert input type, `NewSpecies` for `species`.
///
/// Columns the database owns — generated, identity, or defaulted to a
/// function call such as `gen_random_uuid()` or `now()` — are absent: an
/// insert never supplies them. A column with a literal default is
/// `Option`, where `None` means "leave it to the default".
///
/// No pyo3 attributes are emitted here. A `#[pyclass]` without a `#[new]`
/// constructor cannot be built from Python, and writing that constructor
/// is a judgement call about which columns are required.
fn input_block(table: &Table, opts: &Opts, imports: &mut BTreeSet<String>) -> String {
    let columns = table.insert_columns();
    if !table.writable() || columns.is_empty() {
        return String::new();
    }

    let name = format!(
        "New{}",
        opts.name_override
            .clone()
            .unwrap_or_else(|| naming::pascal_case(&table.name))
    );
    let mut out = format!(
        "\n/// Insert input for `{}.{}`. Columns the database fills in on its\n\
         /// own are absent.\n",
        table.schema, table.name
    );
    out.push_str(&derive_line(&opts.generate.input_derives, imports));
    out.push_str(&format!("pub struct {name} {{\n"));

    let serde = has_serde(&opts.generate.input_derives);
    for column in &columns {
        let mapped = typemap::map(&column.ty, &opts.generate.types);
        for import in &mapped.imports {
            imports.insert(import.clone());
        }
        if let Some(comment) = &column.comment {
            out.push_str(&doc_comment(comment, "    "));
        }
        if let Some(default) = column.literal_default() {
            out.push_str(&format!(
                "    /// `None` leaves this to the column default, `{}`.\n",
                escape(default)
            ));
        }

        let field = naming::ident(&column.name);
        if field.trim_start_matches("r#") != column.name && serde {
            out.push_str(&format!(
                "    #[serde(rename = \"{}\")]\n",
                escape(&column.name)
            ));
        }
        out.push_str(&format!(
            "    pub {field}: {},\n",
            input_type(column, &mapped.text)
        ));
    }
    out.push_str("}\n");
    out
}

/// A column with a literal default is optional on insert even when it is
/// `NOT NULL`, because omitting it is how you ask for the default.
fn input_type(column: &Column, ty: &str) -> String {
    if column.not_null && column.literal_default().is_none() {
        ty.to_string()
    } else {
        format!("Option<{ty}>")
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::super::{MARKER, Strategy};
    use super::*;
    use crate::config::Generate;
    use crate::introspect::PgEnum;

    fn render(pyo3: bool) -> Rendered {
        let generate = Generate::default();
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = pyo3;
        model_file(&fixture::product(), &opts, None)
    }

    #[test]
    fn nullable_columns_become_option() {
        let out = render(false).code;
        assert!(out.contains("pub id: Uuid,"), "{out}");
        assert!(out.contains("pub price: Option<Decimal>,"), "{out}");
        assert!(out.contains("use uuid::Uuid;"), "{out}");
        assert!(out.contains(MARKER), "{out}");
        assert!(!out.contains("pyo3"), "{out}");
    }

    #[test]
    fn pyo3_is_feature_gated() {
        let out = render(true).code;
        assert!(
            out.contains("#[cfg_attr(feature = \"python\", pyo3::pyclass(get_all, set_all))]"),
            "{out}"
        );
        // A field-level cfg_attr would not compile: pyclass runs before
        // cfg_attr expands, so the `pyo3` field attribute is left orphaned.
        assert!(!out.contains("pyo3(get, set)"), "{out}");
    }

    #[test]
    fn the_input_type_drops_what_the_database_owns() {
        let out = render(false).code;
        assert!(out.contains("pub struct NewProduct {"), "{out}");
        let input = &out[out.find("pub struct NewProduct").unwrap()..];
        // Server-owned: a gen_random_uuid() key and a now() timestamp.
        assert!(!input.contains("pub id:"), "{input}");
        assert!(!input.contains("pub created_at:"), "{input}");
        // A foreign key is not server-owned, and stays.
        assert!(input.contains("pub org_id: Uuid,"), "{input}");
        // A literal default stays, as an Option that means "use it".
        assert!(
            input.contains("pub status: Option<ProductStatus>,"),
            "{input}"
        );
        assert!(input.contains("pub slug: String,"), "{input}");
    }

    /// A field name never has to match the column character for
    /// character — the rename attribute carries that — so it is folded
    /// to something rustc will not lint on.
    #[test]
    fn a_folding_column_is_renamed_not_reproduced() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let out = model_file(&fixture::awkward(), &opts, None).code;

        assert!(out.contains("pub mixed_case: Option<i32>,"), "{out}");
        assert!(out.contains(r#"#[sqlx(rename = "Mixed Case")]"#), "{out}");
        assert!(out.contains(r#"#[serde(rename = "Mixed Case")]"#), "{out}");
        // Nothing needs a lint suppressed, so nothing suppresses one.
        assert!(!out.contains("non_snake_case"), "{out}");
    }

    #[test]
    fn colliding_field_names_are_reported() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let mut model = fixture::awkward();
        // `Mixed Case` and `mixed case` both reduce to `mixed_case`.
        model.table.columns.push(super::super::fixture::column(
            "mixed case",
            crate::introspect::PgType::Scalar("text".into()),
            "text",
            false,
        ));
        let rendered = model_file(&model, &opts, None);
        assert!(
            rendered.warnings.iter().any(|w| w.contains("mixed_case")),
            "{:?}",
            rendered.warnings
        );
    }

    #[test]
    fn enum_labels_that_round_trip_use_rename_all() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let mut imports = BTreeSet::new();
        let e = PgEnum {
            schema: "shop".into(),
            name: "order_status".into(),
            labels: vec!["open".into(), "needs_work".into()],
        };
        let out = enum_block(&e, &opts, &mut imports);
        assert!(out.contains("rename_all = \"snake_case\""), "{out}");
        assert!(out.contains("    Open,"), "{out}");
        assert!(out.contains("    NeedsWork,"), "{out}");
    }

    #[test]
    fn odd_enum_labels_get_explicit_renames() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let mut imports = BTreeSet::new();
        let e = PgEnum {
            schema: "public".into(),
            name: "grade".into(),
            labels: vec!["A+".into(), "B-".into()],
        };
        let out = enum_block(&e, &opts, &mut imports);
        assert!(out.contains("#[sqlx(rename = \"A+\")]"), "{out}");
        assert!(!out.contains("rename_all"), "{out}");
    }

    #[test]
    fn imports_group_by_parent() {
        let mut imports = BTreeSet::new();
        imports.insert("chrono::DateTime".to_string());
        imports.insert("chrono::Utc".to_string());
        imports.insert("uuid::Uuid".to_string());
        let out = import_block(&imports);
        assert!(out.contains("use chrono::{DateTime, Utc};"), "{out}");
        assert!(out.contains("use uuid::Uuid;"), "{out}");
    }
}
