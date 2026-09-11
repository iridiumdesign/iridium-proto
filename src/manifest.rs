//! The consuming crate's `Cargo.toml`, and what the generated code needs
//! from it.
//!
//! Generated source names `sqlx`, `serde`, `uuid`, `chrono` and the rest
//! by full path, so the crate it lands in has to declare them. proto
//! knows exactly which ones a run used, so it says so in the manifest
//! rather than leaving the first `cargo build` to. What is already
//! declared is left alone, whatever its version or features: the manifest
//! is the developer's, and proto only fills in what is missing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use crate::error::{Error, Result};
use crate::introspect::Model;
use crate::output::{Change, Journal};
use crate::render::{Opts, has_serde};
use crate::typemap;

/// One crate the generated code needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// The crate, as named on crates.io.
    pub name: String,
    /// The version requirement proto's output is written against.
    pub version: &'static str,
    /// Cargo features the output needs, beyond the crate's defaults.
    pub features: Vec<String>,
    /// Declared `optional = true`, for a dependency a feature turns on.
    pub optional: bool,
}

impl Requirement {
    fn new(name: &str, version: &'static str, features: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            version,
            features: features.iter().map(|f| (*f).to_string()).collect(),
            optional: false,
        }
    }
}

/// What a run's output needs from the manifest.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Requirements {
    /// Dependencies, in name order.
    pub dependencies: Vec<Requirement>,
    /// The pyo3 feature and its entries, for a `--pyo3` run.
    pub feature: Option<(String, Vec<String>)>,
    /// Crates named by `[generate.types]` overrides. proto has no version
    /// to add these with, so a missing one is reported rather than
    /// written.
    pub foreign: Vec<String>,
}

/// What [`apply`] added, and what it noticed but left alone.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// What was written: dependency names, and a feature as
    /// `[features] <name>`.
    pub added: Vec<String>,
    /// Entries already there that lack something the output needs. Said,
    /// never edited: the line is the developer's.
    pub notes: Vec<String>,
}

// ── What the output needs ───────────────────────────────────────────────────

/// The crates the built-in type map can name, besides `sqlx` itself.
const KNOWN: [&str; 4] = ["chrono", "uuid", "rust_decimal", "serde_json"];

/// The dependencies the output for `models` names, under `opts`.
///
/// Only what a run actually used is required: a schema with no `numeric`
/// column needs no `rust_decimal`. `sqlx` is always there — the row
/// structs derive from it and the mappers run on it — with the features
/// for whichever of its type integrations the columns use.
///
/// # Examples
///
/// ```
/// use iridium_proto::config::Generate;
/// use iridium_proto::introspect::{Column, Model, PgType, RelKind, Table};
/// use iridium_proto::manifest::requirements;
/// use iridium_proto::render::{Opts, Strategy};
///
/// let generate = Generate::default();
/// let opts = Opts {
///     generate: &generate,
///     pyo3: false,
///     inputs: false,
///     model_path: "crate::model".to_string(),
///     strategy: Strategy::Embedded,
///     target: "dev",
///     command: "proto model shop.tag".to_string(),
///     name_override: None,
/// };
/// let model = Model {
///     table: Table {
///         schema: "shop".into(),
///         name: "tag".into(),
///         kind: RelKind::Table,
///         comment: None,
///         columns: vec![Column {
///             name: "id".into(),
///             ty: PgType::Scalar("uuid".into()),
///             sql_type: "uuid".into(),
///             not_null: true,
///             comment: None,
///             has_default: true,
///             default_expr: None,
///             identity: false,
///             generated: false,
///         }],
///         primary_key: vec!["id".into()],
///         unique_keys: vec![],
///         foreign_keys: vec![],
///         children: vec![],
///     },
///     enums: vec![],
/// };
///
/// let reqs = requirements([&model], &opts);
/// let names: Vec<&str> = reqs.dependencies.iter().map(|d| d.name.as_str()).collect();
/// assert_eq!(names, ["serde", "sqlx", "uuid"]);
/// ```
pub fn requirements<'a>(models: impl IntoIterator<Item = &'a Model>, opts: &Opts) -> Requirements {
    let generate = opts.generate;
    let mut roots = BTreeSet::new();
    let mut foreign = BTreeSet::new();
    let mut enums = false;

    for model in models {
        enums |= !model.enums.is_empty();
        for column in &model.table.columns {
            let mapped = typemap::map(&column.ty, generate);
            for import in &mapped.imports {
                let root = import.split("::").next().unwrap_or(import);
                match KNOWN.iter().find(|known| **known == root) {
                    Some(known) => {
                        roots.insert(*known);
                    }
                    None if matches!(root, "std" | "core" | "alloc" | "sqlx") => {}
                    None => {
                        foreign.insert(root.to_string());
                    }
                }
            }
        }
    }

    let serde = has_serde(&generate.derives)
        || (enums && has_serde(&generate.enum_derives))
        || (opts.inputs && has_serde(&generate.input_derives));
    let with_serde: &[&str] = if serde { &["serde"] } else { &[] };

    let mut dependencies = Vec::new();
    let mut sqlx = vec!["runtime-tokio", "postgres"];
    for root in ["chrono", "uuid", "rust_decimal"] {
        if roots.contains(root) {
            dependencies.push(Requirement::new(root, version_of(root), with_serde));
            sqlx.push(root);
        }
    }
    if roots.contains("serde_json") {
        dependencies.push(Requirement::new("serde_json", "1", &[]));
        sqlx.push("json");
    }
    if serde {
        dependencies.push(Requirement::new("serde", "1", &["derive"]));
    }
    dependencies.push(Requirement::new("sqlx", "0.8", &sqlx));
    if opts.pyo3 {
        dependencies.push(Requirement {
            optional: true,
            ..Requirement::new("pyo3", "0.26", &[])
        });
    }
    dependencies.sort_by(|a, b| a.name.cmp(&b.name));

    let feature = opts.pyo3.then(|| {
        let mut entries = vec!["dep:pyo3".to_string()];
        for root in ["chrono", "uuid", "rust_decimal"] {
            if roots.contains(root) {
                entries.push(format!("pyo3/{root}"));
            }
        }
        (generate.pyo3_feature.clone(), entries)
    });

    Requirements {
        dependencies,
        feature,
        foreign: foreign.into_iter().collect(),
    }
}

/// The version requirement the output is written against. These follow
/// what sqlx's own type integrations pin, so the two cannot disagree.
fn version_of(root: &str) -> &'static str {
    match root {
        "chrono" => "0.4",
        "uuid" | "rust_decimal" => "1",
        other => unreachable!("no version for {other}"),
    }
}

// ── The manifest ────────────────────────────────────────────────────────────

/// The nearest `Cargo.toml` at or above `from`. A relative `from` walks
/// up to the current directory and stops there.
pub fn locate(from: &Path) -> Option<PathBuf> {
    from.ancestors()
        .map(|dir| dir.join("Cargo.toml"))
        .find(|path| path.is_file())
}

/// Bring the nearest `Cargo.toml` above `from` up to `reqs`, recording
/// what happened in `journal`. No manifest, or one without a `[package]`
/// — a workspace root — means the output is not landing in a crate, and
/// there is nothing to do.
///
/// In check mode nothing is written; a missing dependency is reported as
/// drift, the same as a missing column.
///
/// # Errors
///
/// [`Error::ParseManifest`] for a `Cargo.toml` that is not TOML, else
/// [`Error::ReadFile`] or [`Error::WriteFile`] as the filesystem
/// reports.
pub fn sync(journal: &mut Journal, from: &Path, reqs: &Requirements) -> Result<()> {
    let Some(path) = locate(from) else {
        return Ok(());
    };
    let Some(mut doc) = parse(&path)? else {
        return Ok(());
    };
    if !doc.contains_key("package") {
        return Ok(());
    }

    let workspace = workspace_above(&path);
    let outcome = apply(&mut doc, reqs, workspace.as_ref());
    for note in &outcome.notes {
        crate::output::warn(&format!("{}: {note}", path.display()));
    }
    if outcome.added.is_empty() {
        journal.record(Change::Unchanged, &path, None);
        return Ok(());
    }

    if !journal.is_check() {
        std::fs::write(&path, doc.to_string()).map_err(|source| Error::WriteFile {
            path: path.clone(),
            source,
        })?;
    }
    let detail = outcome
        .added
        .iter()
        .map(|a| format!("+{a}"))
        .collect::<Vec<_>>()
        .join(", ");
    journal.record(Change::Updated, &path, Some(detail));
    Ok(())
}

/// Read and parse a manifest. `Ok(None)` when there is none.
fn parse(path: &Path) -> Result<Option<DocumentMut>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::ReadFile {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    text.parse()
        .map(Some)
        .map_err(|source| Error::ParseManifest {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
}

/// The workspace manifest a package at `manifest` belongs to, if one is
/// above it: the nearest `Cargo.toml` with a `[workspace]` table. One
/// that does not parse is skipped rather than fatal — it is not the
/// file being edited.
fn workspace_above(manifest: &Path) -> Option<DocumentMut> {
    manifest
        .parent()?
        .parent()?
        .ancestors()
        .map(|dir| dir.join("Cargo.toml"))
        .filter(|path| path.is_file())
        .filter_map(|path| parse(&path).ok().flatten())
        .find(|doc| doc.contains_key("workspace"))
}

/// Add what `reqs` needs and `doc` lacks, in place.
///
/// A dependency already declared is left exactly as it is, whatever its
/// version or features; where it lacks a feature the output needs, the
/// outcome says so. A crate the workspace declares is inherited with
/// `workspace = true` rather than declared again. Comments, ordering and
/// formatting elsewhere in the file survive: `toml_edit` patches the
/// document rather than printing it back out.
pub fn apply(
    doc: &mut DocumentMut,
    reqs: &Requirements,
    workspace: Option<&DocumentMut>,
) -> Outcome {
    let mut outcome = Outcome::default();
    let inherited = workspace
        .and_then(|w| w.get("workspace"))
        .and_then(|w| w.get("dependencies"))
        .and_then(Item::as_table_like);

    let deps = doc
        .entry("dependencies")
        .or_insert(Item::Table(Table::new()));
    let Some(deps) = deps.as_table_like_mut() else {
        return outcome;
    };
    let was_sorted = deps.iter().map(|(k, _)| k).is_sorted();

    for req in &reqs.dependencies {
        let from_workspace = inherited.and_then(|w| w.get(&req.name));
        match deps.get(&req.name) {
            Some(existing) => {
                if let Some(declared) = declared_features(existing, from_workspace) {
                    let missing: Vec<&str> = req
                        .features
                        .iter()
                        .filter(|f| !declared.contains(f.as_str()))
                        .map(String::as_str)
                        .collect();
                    if !missing.is_empty() {
                        outcome.notes.push(format!(
                            "{} is declared without the {} feature{} the generated code needs",
                            req.name,
                            missing.join(", "),
                            if missing.len() == 1 { "" } else { "s" },
                        ));
                    }
                }
            }
            None => {
                let item = match from_workspace {
                    Some(ws) => inherit(req, ws),
                    None => declare(req),
                };
                deps.insert(&req.name, item);
                outcome.added.push(req.name.clone());
            }
        }
    }
    for name in &reqs.foreign {
        if !deps.contains_key(name) {
            outcome.notes.push(format!(
                "[generate.types] names `{name}`, which is not declared; \
                 proto does not know its version, so add it yourself"
            ));
        }
    }
    // `dep:pyo3` only makes sense for an optional dependency. An extension
    // crate declares pyo3 outright, and its feature lists conversions only.
    let pyo3_optional = deps.get("pyo3").is_some_and(is_optional);

    if was_sorted && let Some(table) = doc["dependencies"].as_table_mut() {
        table.sort_values();
    }

    if let Some((name, entries)) = &reqs.feature {
        let entries: Vec<&str> = entries
            .iter()
            .map(String::as_str)
            .filter(|e| pyo3_optional || *e != "dep:pyo3")
            .collect();
        let features = doc.entry("features").or_insert(Item::Table(Table::new()));
        if let Some(features) = features.as_table_like_mut() {
            match features.get(name) {
                Some(existing) => {
                    let have: BTreeSet<&str> = existing
                        .as_array()
                        .into_iter()
                        .flat_map(Array::iter)
                        .filter_map(Value::as_str)
                        .collect();
                    let missing: Vec<&str> = entries
                        .iter()
                        .copied()
                        .filter(|e| !have.contains(e))
                        .collect();
                    if existing.is_array() && !missing.is_empty() {
                        outcome.notes.push(format!(
                            "feature `{name}` is declared without {} the generated code needs",
                            missing.join(", ")
                        ));
                    }
                }
                None => {
                    features.insert(name, Item::Value(string_array(&entries)));
                    outcome.added.push(format!("[features] {name}"));
                }
            }
        }
    }

    outcome
}

/// The features a declared dependency turns on, or `None` when that
/// cannot be told from here — a workspace inheritance with no workspace
/// manifest to hand.
fn declared_features(item: &Item, from_workspace: Option<&Item>) -> Option<BTreeSet<String>> {
    let mut features: BTreeSet<String> = features_of(item);
    if item.get("workspace").and_then(Item::as_bool) == Some(true) {
        features.extend(features_of(from_workspace?));
    }
    Some(features)
}

fn features_of(item: &Item) -> BTreeSet<String> {
    item.get("features")
        .and_then(Item::as_array)
        .into_iter()
        .flat_map(Array::iter)
        .filter_map(Value::as_str)
        .map(String::from)
        .collect()
}

fn is_optional(item: &Item) -> bool {
    item.get("optional").and_then(Item::as_bool) == Some(true)
}

/// `name = "1"`, or the inline table when there is more to say.
fn declare(req: &Requirement) -> Item {
    if req.features.is_empty() && !req.optional {
        return Item::Value(Value::from(req.version));
    }
    let mut table = InlineTable::new();
    table.insert("version", Value::from(req.version));
    if !req.features.is_empty() {
        let features: Vec<&str> = req.features.iter().map(String::as_str).collect();
        table.insert("features", string_array(&features));
    }
    if req.optional {
        table.insert("optional", Value::from(true));
    }
    Item::Value(Value::InlineTable(table))
}

/// `name = { workspace = true }`, plus whichever features the workspace
/// entry does not already turn on. Features are additive, so the member
/// may ask for more than the workspace gives.
fn inherit(req: &Requirement, from_workspace: &Item) -> Item {
    let have = features_of(from_workspace);
    let mut table = InlineTable::new();
    table.insert("workspace", Value::from(true));
    let extra: Vec<&str> = req
        .features
        .iter()
        .filter(|f| !have.contains(f.as_str()))
        .map(String::as_str)
        .collect();
    if !extra.is_empty() {
        table.insert("features", string_array(&extra));
    }
    if req.optional {
        table.insert("optional", Value::from(true));
    }
    Item::Value(Value::InlineTable(table))
}

fn string_array(items: &[&str]) -> Value {
    let mut array = Array::new();
    for item in items {
        array.push(*item);
    }
    Value::Array(array)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Generate;
    use crate::render::Strategy;
    use crate::render::fixture;

    fn product_requirements(pyo3: bool) -> Requirements {
        let generate = Generate::default();
        let mut opts = fixture::opts(&generate, Strategy::Embedded);
        opts.pyo3 = pyo3;
        requirements([&fixture::product()], &opts)
    }

    fn names(reqs: &Requirements) -> Vec<&str> {
        reqs.dependencies.iter().map(|d| d.name.as_str()).collect()
    }

    fn dependency<'a>(reqs: &'a Requirements, name: &str) -> &'a Requirement {
        reqs.dependencies
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    #[test]
    fn only_what_the_columns_use_is_required() {
        // shop.product: uuid, text, an enum, numeric, timestamptz. No
        // json, so no serde_json and no sqlx `json` feature.
        let reqs = product_requirements(false);
        assert_eq!(
            names(&reqs),
            ["chrono", "rust_decimal", "serde", "sqlx", "uuid"]
        );
        assert_eq!(
            dependency(&reqs, "sqlx").features,
            [
                "runtime-tokio",
                "postgres",
                "chrono",
                "uuid",
                "rust_decimal"
            ]
        );
        assert!(reqs.feature.is_none());
        assert!(reqs.foreign.is_empty());
    }

    #[test]
    fn serde_derives_put_serde_on_the_type_crates() {
        let reqs = product_requirements(false);
        assert_eq!(dependency(&reqs, "chrono").features, ["serde"]);
        assert_eq!(dependency(&reqs, "uuid").features, ["serde"]);
        assert_eq!(dependency(&reqs, "serde").features, ["derive"]);

        let mut generate = Generate::default();
        for list in [
            &mut generate.derives,
            &mut generate.enum_derives,
            &mut generate.input_derives,
        ] {
            list.retain(|d| d != "Serialize" && d != "Deserialize");
        }
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let reqs = requirements([&fixture::product()], &opts);
        assert!(!names(&reqs).contains(&"serde"));
        assert!(dependency(&reqs, "chrono").features.is_empty());
    }

    #[test]
    fn pyo3_is_optional_and_its_feature_lists_the_conversions() {
        let reqs = product_requirements(true);
        let pyo3 = dependency(&reqs, "pyo3");
        assert!(pyo3.optional);
        assert_eq!(
            reqs.feature,
            Some((
                "python".to_string(),
                ["dep:pyo3", "pyo3/chrono", "pyo3/uuid", "pyo3/rust_decimal"]
                    .map(String::from)
                    .to_vec()
            ))
        );
    }

    #[test]
    fn an_override_names_a_crate_proto_cannot_version() {
        let mut generate = Generate::default();
        generate
            .types
            .insert("numeric".into(), "bigdecimal::BigDecimal".into());
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let reqs = requirements([&fixture::product()], &opts);
        assert_eq!(reqs.foreign, ["bigdecimal"]);
        assert!(!names(&reqs).contains(&"rust_decimal"));
    }

    const BARE: &str = "\
[package]
name = \"shop\"
version = \"0.1.0\"
edition = \"2024\"

[dependencies]
# The web layer.
axum = \"0.8\"
tokio = { version = \"1\", features = [\"full\"] }
";

    #[test]
    fn missing_dependencies_are_added_and_the_rest_is_untouched() {
        let mut doc: DocumentMut = BARE.parse().unwrap();
        let outcome = apply(&mut doc, &product_requirements(true), None);
        assert_eq!(
            outcome.added,
            [
                "chrono",
                "pyo3",
                "rust_decimal",
                "serde",
                "sqlx",
                "uuid",
                "[features] python"
            ]
        );
        assert!(outcome.notes.is_empty());

        let text = doc.to_string();
        assert!(
            text.contains("# The web layer.\naxum = \"0.8\"\n"),
            "{text}"
        );
        assert!(
            text.contains("chrono = { version = \"0.4\", features = [\"serde\"] }\n"),
            "{text}"
        );
        assert!(
            text.contains("pyo3 = { version = \"0.26\", optional = true }\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "sqlx = { version = \"0.8\", features = [\"runtime-tokio\", \"postgres\", \
                 \"chrono\", \"uuid\", \"rust_decimal\"] }\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "[features]\npython = [\"dep:pyo3\", \"pyo3/chrono\", \"pyo3/uuid\", \
                 \"pyo3/rust_decimal\"]\n"
            ),
            "{text}"
        );
        // A sorted table stays sorted.
        let deps: Vec<&str> = doc["dependencies"]
            .as_table()
            .unwrap()
            .iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            deps,
            [
                "axum",
                "chrono",
                "pyo3",
                "rust_decimal",
                "serde",
                "sqlx",
                "tokio",
                "uuid"
            ]
        );
    }

    #[test]
    fn a_second_pass_changes_nothing() {
        let mut doc: DocumentMut = BARE.parse().unwrap();
        apply(&mut doc, &product_requirements(true), None);
        let once = doc.to_string();
        let outcome = apply(&mut doc, &product_requirements(true), None);
        assert!(outcome.added.is_empty());
        assert!(outcome.notes.is_empty());
        assert_eq!(doc.to_string(), once);
    }

    #[test]
    fn a_declared_dependency_is_left_alone_and_a_missing_feature_is_said() {
        let manifest = "\
[package]
name = \"shop\"

[dependencies]
sqlx = { version = \"0.7\", features = [\"runtime-tokio\", \"postgres\"] }
uuid = \"1\"
";
        let mut doc: DocumentMut = manifest.parse().unwrap();
        let outcome = apply(&mut doc, &product_requirements(false), None);
        assert_eq!(outcome.added, ["chrono", "rust_decimal", "serde"]);
        assert_eq!(
            outcome.notes,
            [
                "sqlx is declared without the chrono, uuid, rust_decimal features \
                 the generated code needs",
                "uuid is declared without the serde feature the generated code needs",
            ]
        );
        let text = doc.to_string();
        assert!(text.contains("sqlx = { version = \"0.7\""), "{text}");
        assert!(text.contains("\nuuid = \"1\"\n"), "{text}");
    }

    #[test]
    fn an_extension_crate_needs_no_dep_entry() {
        // pyo3 is not optional in a cdylib, so `dep:pyo3` would be wrong
        // there and its absence is not a complaint.
        let manifest = "\
[package]
name = \"shoppy\"

[features]
python = [\"pyo3/chrono\", \"pyo3/uuid\", \"pyo3/rust_decimal\"]

[dependencies]
pyo3 = { version = \"0.26\", features = [\"extension-module\"] }
";
        let mut doc: DocumentMut = manifest.parse().unwrap();
        let outcome = apply(&mut doc, &product_requirements(true), None);
        assert!(!outcome.added.iter().any(|a| a.starts_with("[features]")));
        assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
    }

    #[test]
    fn a_workspace_dependency_is_inherited() {
        let workspace: DocumentMut = "\
[workspace]
members = [\"shop\"]

[workspace.dependencies]
sqlx = { version = \"0.8\", features = [\"runtime-tokio\", \"postgres\", \"uuid\"] }
uuid = { version = \"1\", features = [\"serde\"] }
"
        .parse()
        .unwrap();
        let mut doc: DocumentMut = "[package]\nname = \"shop\"\n".parse().unwrap();
        let outcome = apply(&mut doc, &product_requirements(false), Some(&workspace));
        assert_eq!(
            outcome.added,
            ["chrono", "rust_decimal", "serde", "sqlx", "uuid"]
        );
        let text = doc.to_string();
        assert!(text.contains("uuid = { workspace = true }\n"), "{text}");
        assert!(
            text.contains(
                "sqlx = { workspace = true, features = [\"chrono\", \"rust_decimal\"] }\n"
            ),
            "{text}"
        );
        assert!(text.contains("serde = { version = \"1\""), "{text}");
    }

    #[test]
    fn a_foreign_crate_is_reported_not_added() {
        let mut generate = Generate::default();
        generate
            .types
            .insert("numeric".into(), "bigdecimal::BigDecimal".into());
        let opts = fixture::opts(&generate, Strategy::Embedded);
        let reqs = requirements([&fixture::product()], &opts);
        let mut doc: DocumentMut = BARE.parse().unwrap();
        let outcome = apply(&mut doc, &reqs, None);
        assert!(!outcome.added.iter().any(|a| a == "bigdecimal"));
        assert!(
            outcome.notes.iter().any(|n| n.contains("`bigdecimal`")),
            "{:?}",
            outcome.notes
        );
    }

    #[test]
    fn locate_walks_up_and_a_workspace_root_is_not_a_crate() {
        let root =
            std::env::temp_dir().join(format!("iridium-proto-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let crate_dir = root.join("ws").join("shop");
        let out = crate_dir.join("src").join("model");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            root.join("ws").join("Cargo.toml"),
            "[workspace]\nmembers = [\"shop\"]\n",
        )
        .unwrap();
        assert_eq!(
            locate(&out).as_deref(),
            Some(root.join("ws").join("Cargo.toml").as_path()),
            "the workspace root is the nearest manifest until the crate has one"
        );

        let mut journal = Journal::new(false);
        sync(&mut journal, &out, &product_requirements(false)).unwrap();
        assert!(journal.is_empty(), "a workspace root is not a crate");

        std::fs::write(crate_dir.join("Cargo.toml"), BARE).unwrap();
        assert_eq!(
            locate(&out).as_deref(),
            Some(crate_dir.join("Cargo.toml").as_path())
        );

        let mut check = Journal::new(true);
        sync(&mut check, &out, &product_requirements(false)).unwrap();
        assert!(check.changed(), "a missing dependency is drift");
        assert_eq!(
            std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap(),
            BARE,
            "check mode writes nothing"
        );

        let mut journal = Journal::new(false);
        sync(&mut journal, &out, &product_requirements(false)).unwrap();
        let written = std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap();
        assert!(written.contains("sqlx = {"), "{written}");
        assert!(
            journal
                .summary()
                .contains("+chrono, +rust_decimal, +serde, +sqlx, +uuid")
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
