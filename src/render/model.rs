//! Row structs, the Rust enums behind Postgres enum types, and the insert
//! input types that go with them.

use std::collections::BTreeSet;

use super::Rendered;
use super::children::{self, ChildField};
use super::{
    OWNED, Opts, dedupe_enums, derive_line, doc_comment, escape, has_serde, header, import_block,
    indent, reexport_block,
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
    let mut reexports = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut body = String::new();

    match enum_path {
        // Defined here, so nothing to bring in.
        None => {
            for e in &model.enums {
                body.push_str(&enum_block(e, opts, &mut imports));
                body.push('\n');
            }
        }
        // Defined next door, and named in a field of this module's
        // struct, so it is re-exported for whoever holds one.
        Some(path) => {
            for e in &model.enums {
                reexports.insert(format!("{path}::{}", naming::pascal_case(&e.name)));
            }
        }
    }

    // A child's type lives in the sibling module `--out-dir` writes it
    // to, which is also where a single `proto model` run assumes it is.
    body.push_str(&struct_block(
        &model.table,
        opts,
        Some("super"),
        &mut imports,
        &mut warnings,
    ));
    if opts.inputs {
        body.push_str(&input_block(&model.table, opts, &mut imports));
    }

    let source = format!("{}.{}", model.table.schema, model.table.name);
    let mut code = header(opts, &source, model.table.kind.label());
    code.push_str(&import_block(&imports));
    code.push_str(&reexport_block(&reexports));
    if !reexports.is_empty() {
        code.push('\n');
    }
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
        // Every struct is in this one file, so a child needs no import.
        body.push_str(&struct_block(
            &model.table,
            opts,
            None,
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
///
/// `python` names the Cargo feature that gates a generated `#[pymodule]`
/// sitting beside them, if one was generated.
pub fn mod_file(modules: &[String], source: &str, opts: &Opts, python: Option<&str>) -> String {
    let mut code = header(opts, source, "module list");
    for m in modules {
        code.push_str(&format!("pub mod {};\n", naming::ident(m)));
    }
    if let Some(feature) = python {
        code.push_str(&format!(
            "\n#[cfg(feature = \"{feature}\")]\npub mod python;\n"
        ));
    }
    code
}

// ── Blocks ──────────────────────────────────────────────────────────────────

/// The doc comment above a struct: the table's own comment, its key, and
/// a warning where `NOT NULL` means nothing.
fn struct_docs(table: &Table) -> String {
    let mut docs = String::new();
    if let Some(comment) = &table.comment {
        docs.push_str(&doc_comment(comment, ""));
    }
    if !table.primary_key.is_empty() {
        let key = table.primary_key.join("`, `");
        docs.push_str(&format!("/// Primary key: `{key}`.\n"));
    }
    if !table.kind.nullability_is_known() {
        if !docs.is_empty() {
            docs.push_str("///\n");
        }
        let kind = table.kind.label();
        docs.push_str(&format!(
            "/// Every column of a {kind} reports as nullable, so every field\n\
             /// here is `Option`. Tighten by hand where you know better.\n"
        ));
    }
    docs
}

/// One field: its doc comment, an optional note after it, whatever
/// rename it needs to keep matching the column, and the declaration.
/// Written at column zero.
///
/// The renames follow the derives. A row struct derives `sqlx::FromRow`
/// and needs the sqlx rename; an input type does not, and does not.
fn field(column: &Column, ty: &str, derives: &[String], note: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(comment) = &column.comment {
        out.push_str(&doc_comment(comment, ""));
    }
    // Anything the caller wants said after the column's own comment.
    if let Some(note) = note {
        out.push_str(note);
    }

    let name = naming::ident(&column.name);
    if name.trim_start_matches("r#") != column.name {
        let renamed = escape(&column.name);
        if derives.iter().any(|d| d == "sqlx::FromRow") {
            out.push_str(&format!("#[sqlx(rename = \"{renamed}\")]\n"));
        }
        if has_serde(derives) {
            out.push_str(&format!("#[serde(rename = \"{renamed}\")]\n"));
        }
    }
    out.push_str(&format!("pub {name}: {ty},\n"));
    out
}

/// The pyo3 attribute for a generated struct, if it was asked for.
///
/// `get_all, set_all` rather than a `#[pyo3(get, set)]` on every field:
/// pyclass never sees a field-level `cfg_attr`, which is expanded after
/// the macro runs, so per-field gating does not compile.
fn pyclass(opts: &Opts) -> String {
    if !opts.pyo3 {
        return String::new();
    }
    let feature = &opts.generate.pyo3_feature;
    format!("#[cfg_attr(feature = \"{feature}\", pyo3::pyclass(get_all, set_all))]\n")
}

/// The row struct. `sibling` is the module path a child's type is
/// imported from — `super` beside the other model files — or `None`
/// when the child is in this file too.
fn struct_block(
    table: &Table,
    opts: &Opts,
    sibling: Option<&str>,
    imports: &mut BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> String {
    let name = opts
        .name_override
        .clone()
        .unwrap_or_else(|| naming::pascal_case(&table.name));
    let docs = struct_docs(table);
    let derives = derive_line(&opts.generate.derives, imports);
    let pyclass = pyclass(opts);

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut fields = String::new();
    for column in &table.columns {
        let ident = naming::ident(&column.name);
        if !seen.insert(ident.clone()) {
            warnings.push(format!(
                "{}.{}: two columns reduce to the field name `{ident}`; \
                 rename one or the generated struct will not compile",
                table.schema, table.name
            ));
        }

        let mapped = typemap::map(&column.ty, opts.generate);
        imports.extend(mapped.imports.iter().cloned());
        if let Some(unknown) = &mapped.unmapped {
            fields.push_str(&format!(
                "    // TODO: unmapped Postgres type `{unknown}` — set \
                 [generate.types] {unknown} = \"...\"\n"
            ));
            warnings.push(format!(
                "{}.{}.{}: unmapped Postgres type '{unknown}', using String",
                table.schema, table.name, column.name
            ));
        }

        let ty = if column.not_null {
            mapped.text
        } else {
            format!("Option<{}>", mapped.text)
        };
        fields.push_str(&indent(
            &field(column, &ty, &opts.generate.derives, None),
            4,
        ));
    }

    let children = children::of(table, opts.generate);
    warnings.extend(children.warnings);
    for child in &children.fields {
        if let Some(path) = sibling
            && !child.is_self(table)
        {
            imports.insert(format!("{path}::{}::{}", child.module, child.ty));
        }
        fields.push_str(&indent(&child_field(child, table, &name, opts), 4));
    }

    format!("{docs}{derives}{pyclass}pub struct {name} {{\n{fields}}}\n")
}

/// The field a parent holds its child rows in. Not a column, so sqlx is
/// told to skip it and serde to default it; the mapper fills it.
fn child_field(child: &ChildField, table: &Table, parent: &str, opts: &Opts) -> String {
    let derives = &opts.generate.derives;
    let ty = if child.is_self(table) {
        parent.to_string()
    } else {
        child.ty.clone()
    };
    let mut out = format!(
        "/// Rows of `{}.{}` whose `{}` is this row's `{}`. Not a column:\n\
         /// `{parent}Mapper::load_{}` fills it, and it is empty until then.\n",
        child.child.schema,
        child.child.table,
        child.child.column,
        child.child.ref_column,
        child.stem
    );
    if derives.iter().any(|d| d == "sqlx::FromRow") {
        out.push_str("#[sqlx(skip)]\n");
    }
    if has_serde(derives) {
        out.push_str("#[serde(default)]\n");
    }
    out.push_str(&format!("pub {}: Vec<{ty}>,\n", child.field));
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

    // sqlx matches the type name the server reports. A type outside
    // `public` is not on the default search_path, so the server names it
    // with its schema and the attribute has to as well — without this an
    // enum column fails to decode at run time, which compiling never
    // catches.
    let type_name = if e.schema == "public" {
        e.name.clone()
    } else {
        format!("{}.{}", e.schema, e.name)
    };

    let mut out = format!("/// The `{}.{}` enum type.\n", e.schema, e.name);
    out.push_str(&derive_line(&opts.generate.enum_derives, imports));
    if round_trips {
        out.push_str(&format!(
            "#[sqlx(type_name = \"{}\", rename_all = \"snake_case\")]\n",
            escape(&type_name)
        ));
        if has_serde(&opts.generate.enum_derives) {
            out.push_str("#[serde(rename_all = \"snake_case\")]\n");
        }
    } else {
        out.push_str(&format!(
            "#[sqlx(type_name = \"{}\")]\n",
            escape(&type_name)
        ));
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
/// Under `--pyo3` the input is a class too, with a constructor: a column
/// that is `NOT NULL` without a default is a required argument, and the
/// rest are keyword-only and default to `None`. Without the constructor
/// a `#[pyclass]` cannot be built from Python, and `create` would have
/// nothing to take.
fn input_block(table: &Table, opts: &Opts, imports: &mut BTreeSet<String>) -> String {
    let columns = table.insert_columns();
    if !table.writable() || columns.is_empty() {
        return String::new();
    }

    let name = opts
        .name_override
        .clone()
        .unwrap_or_else(|| naming::pascal_case(&table.name));
    let derives = derive_line(&opts.generate.input_derives, imports);

    let mut fields = String::new();
    for column in &columns {
        let mapped = typemap::map(&column.ty, opts.generate);
        imports.extend(mapped.imports.iter().cloned());

        let note = column.literal_default().map(|default| {
            format!(
                "/// `None` leaves this to the column default, `{}`.\n",
                escape(default)
            )
        });
        let ty = input_type(column, &mapped.text);
        fields.push_str(&indent(
            &field(column, &ty, &opts.generate.input_derives, note.as_deref()),
            4,
        ));
    }

    let pyclass = pyclass(opts);
    let mut out = format!(
        "\n/// Insert input for `{}.{}`. Columns the database fills in on its\n\
         /// own are absent.\n{derives}{pyclass}pub struct New{name} {{\n{fields}}}\n",
        table.schema, table.name
    );
    if opts.pyo3 {
        out.push_str(&constructor(&columns, opts, &format!("New{name}")));
    }
    out
}

/// The Python constructor for an input type.
///
/// Required columns come first and positional; the rest sit after a
/// bare `*`, so a caller names them and can leave them out.
fn constructor(columns: &[&Column], opts: &Opts, name: &str) -> String {
    let feature = &opts.generate.pyo3_feature;
    // pyo3 wants the parameters in the order the signature names them,
    // so the required ones lead on both, and the struct is filled by
    // name.
    let mut required = Vec::new();
    let mut optional = Vec::new();
    let mut fields = Vec::new();
    for column in columns {
        let ident = naming::ident(&column.name);
        let mapped = typemap::map(&column.ty, opts.generate);
        let ty = input_type(column, &mapped.text);
        let param = format!("{ident}: {ty}");
        if ty.starts_with("Option<") {
            optional.push((format!("{ident}=None"), param));
        } else {
            required.push((ident.clone(), param));
        }
        fields.push(ident);
    }
    let mut signature: Vec<String> = required.iter().map(|(s, _)| s.clone()).collect();
    let mut params: Vec<String> = required.into_iter().map(|(_, p)| p).collect();
    if !optional.is_empty() {
        signature.push("*".to_string());
        for (s, p) in optional {
            signature.push(s);
            params.push(p);
        }
    }
    let owned = indent(OWNED, 4);
    format!(
        r#"
#[cfg(feature = "{feature}")]
#[pyo3::pymethods]
impl {name} {{
    /// Build an input. Columns that are `NOT NULL` without a default are
    /// required; the rest are keyword-only and default to `None`, which
    /// leaves a defaulted column to the database.
{owned}    #[new]
    #[pyo3(signature = (
        {signature},
    ))]
    fn new(
        {params},
    ) -> Self {{
        Self {{
            {fields},
        }}
    }}
}}
"#,
        signature = signature.join(",\n        "),
        params = params.join(",\n        "),
        fields = fields.join(",\n            "),
    )
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
    fn the_input_gets_a_constructor_under_pyo3() {
        let out = render(true).code;
        // Required first, then keyword-only with None defaults: slug, name
        // and org_id have no default; status has a literal one, and
        // price is nullable.
        assert!(
            out.contains(
                "    #[pyo3(signature = (\n        slug,\n        name,\n        org_id,\n        \
                 *,\n        status=None,\n        price=None,\n    ))]"
            ),
            "{out}"
        );
        assert!(
            out.contains("#[cfg(feature = \"python\")]\n#[pyo3::pymethods]\nimpl NewProduct {"),
            "{out}"
        );
        assert!(
            out.contains("pyo3::pyclass(get_all, set_all))]\npub struct NewProduct {"),
            "{out}"
        );
        assert!(out.contains("proto owns this method"), "{out}");
        // And without the flag, the input is a plain struct.
        let plain = render(false).code;
        assert!(!plain.contains("impl NewProduct"), "{plain}");
    }

    #[test]
    fn the_constructor_survives_its_own_reconcile() {
        let out = render(true).code;
        assert_eq!(
            crate::reconcile::reconcile(&out, &out).as_deref(),
            Some(out.as_str())
        );
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
    fn a_parent_holds_its_children_beside_its_columns() {
        let out = render(false).code;
        assert!(out.contains("use super::variant::Variant;"), "{out}");
        assert!(
            out.contains(
                "    #[sqlx(skip)]\n    #[serde(default)]\n    pub children: Vec<Variant>,\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("`ProductMapper::load_children` fills it"),
            "{out}"
        );
        // Not a column, so the insert input does not carry it.
        let input = &out[out.find("pub struct NewProduct").unwrap()..];
        assert!(!input.contains("children"), "{input}");

        // In one flat file the child is right there: nothing to import.
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let flat = schema_file(&[fixture::product()], "shop", &opts).code;
        assert!(!flat.contains("use super::variant"), "{flat}");
        assert!(flat.contains("pub children: Vec<Variant>,"), "{flat}");
    }

    #[test]
    fn a_tree_holds_its_own_kind() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let out = model_file(&fixture::category(), &opts, None).code;
        assert!(out.contains("pub children: Vec<Category>,"), "{out}");
        assert!(!out.contains("use super::category"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
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

    /// A type named in a struct field has to be nameable by whoever
    /// holds that struct. `cargo check` never catches this, because
    /// nothing inside the generated crate refers to the enum by path —
    /// building a Python extension from it did.
    #[test]
    fn enum_types_from_a_sibling_module_are_reexported() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let out = model_file(&fixture::product(), &opts, Some("super::enums")).code;
        assert!(
            out.contains("pub use super::enums::ProductStatus;"),
            "{out}"
        );

        // Defined in the same file, there is nothing to re-export.
        let inline = model_file(&fixture::product(), &opts, None).code;
        assert!(!inline.contains("pub use"), "{inline}");
        assert!(inline.contains("pub enum ProductStatus {"), "{inline}");
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

    /// sqlx compares against the name the server reports, and a type
    /// outside `public` is reported with its schema. Getting this wrong
    /// compiles fine and fails when a row comes back.
    #[test]
    fn enum_types_outside_public_are_schema_qualified() {
        let generate = Generate::default();
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let mut imports = BTreeSet::new();

        let e = PgEnum {
            schema: "shop".into(),
            name: "product_status".into(),
            labels: vec!["draft".into()],
        };
        let out = enum_block(&e, &opts, &mut imports);
        assert!(
            out.contains(r#"type_name = "shop.product_status""#),
            "{out}"
        );

        // `public` is on the default search_path, so the server names it
        // bare and so does the attribute.
        let public = PgEnum {
            schema: "public".into(),
            ..e
        };
        let out = enum_block(&public, &opts, &mut imports);
        assert!(out.contains(r#"type_name = "product_status""#), "{out}");
        // The doc comment still names the schema; the attribute must not.
        assert!(!out.contains(r#"type_name = "public."#), "{out}");
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
