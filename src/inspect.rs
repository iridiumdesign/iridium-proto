//! Reading generated code back as structure rather than as text.
//!
//! Comparing two versions of a file line by line answers "these bytes
//! differ", which is not the question. The question is whether the types
//! in a file still say what the database says: does `NewTree` hold the
//! columns an insert supplies, is `width` still the Rust type its column
//! maps to, did somebody change a field by hand.
//!
//! So the file is parsed into a syntax tree and reduced to the shape
//! that matters — structs, their fields and types, enums and their
//! variants, impls and their methods. Reformatting, reordering, comments
//! and blank lines all vanish in the reduction, which is the point: what
//! is left differs only when something real has.

use quote::ToTokens;

/// What a generated file declares.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Every struct in the file, in declaration order.
    pub structs: Vec<Struct>,
    /// Every enum in the file.
    pub enums: Vec<Enum>,
    /// Method names by the type they are implemented on.
    pub impls: Vec<Impl>,
}

/// A struct and its public fields.
#[derive(Debug, PartialEq, Eq)]
pub struct Struct {
    /// The type's name.
    pub name: String,
    /// Its fields, in declaration order.
    pub fields: Vec<Field>,
}

/// One field, reduced to the pair that has to match the column.
#[derive(Debug, PartialEq, Eq)]
pub struct Field {
    /// The field name, with any raw-identifier prefix removed.
    pub name: String,
    /// The type, normalised so spacing cannot make two the same type
    /// look different.
    pub ty: String,
}

/// An enum and its variants.
#[derive(Debug, PartialEq, Eq)]
pub struct Enum {
    /// The type's name.
    pub name: String,
    /// Its variants, in declaration order.
    pub variants: Vec<String>,
}

/// An impl block, by what it is on.
#[derive(Debug, PartialEq, Eq)]
pub struct Impl {
    /// The type being implemented, without lifetimes or generics.
    pub type_name: String,
    /// The names of the methods it defines.
    pub methods: Vec<String>,
}

/// Parse a file into its structure.
///
/// Returns `None` when the source does not parse — a half-finished edit,
/// or something that is not Rust. Nothing downstream should treat that
/// as a difference; it is an absence of information.
pub fn summarize(source: &str) -> Option<Summary> {
    let file = syn::parse_file(source).ok()?;
    let mut summary = Summary::default();

    for item in &file.items {
        match item {
            syn::Item::Struct(item) => summary.structs.push(Struct {
                name: item.ident.to_string(),
                fields: item.fields.iter().filter_map(field).collect(),
            }),
            syn::Item::Enum(item) => summary.enums.push(Enum {
                name: item.ident.to_string(),
                variants: item.variants.iter().map(|v| v.ident.to_string()).collect(),
            }),
            syn::Item::Impl(item) => {
                if let Some(type_name) = impl_target(item) {
                    summary.impls.push(Impl {
                        type_name,
                        methods: item
                            .items
                            .iter()
                            .filter_map(|i| match i {
                                syn::ImplItem::Fn(f) => Some(f.sig.ident.to_string()),
                                _ => None,
                            })
                            .collect(),
                    });
                }
            }
            _ => {}
        }
    }
    Some(summary)
}

fn field(field: &syn::Field) -> Option<Field> {
    let name = field.ident.as_ref()?.to_string();
    Some(Field {
        // `r#type` and `type` are the same field to everyone but the
        // lexer.
        name: name.trim_start_matches("r#").to_string(),
        ty: normalise(&field.ty),
    })
}

/// `Option < String >` and `Option<String>` are one type; token spacing
/// is not information.
fn normalise(ty: &syn::Type) -> String {
    ty.to_token_stream()
        .to_string()
        .replace(' ', "")
        .replace(',', ", ")
}

/// The bare name a block is implemented on: `TreeMapper` from
/// `impl<'a> TreeMapper<'a>`.
fn impl_target(item: &syn::ItemImpl) -> Option<String> {
    match &*item.self_ty {
        syn::Type::Path(path) => Some(path.path.segments.last()?.ident.to_string()),
        _ => None,
    }
}

impl Summary {
    /// Find a struct by name.
    pub fn struct_named(&self, name: &str) -> Option<&Struct> {
        self.structs.iter().find(|s| s.name == name)
    }

    /// Find an enum by name.
    pub fn enum_named(&self, name: &str) -> Option<&Enum> {
        self.enums.iter().find(|e| e.name == name)
    }

    /// The methods implemented on a type, across every impl block.
    pub fn methods_on(&self, type_name: &str) -> Vec<&str> {
        self.impls
            .iter()
            .filter(|i| i.type_name == type_name)
            .flat_map(|i| i.methods.iter().map(String::as_str))
            .collect()
    }
}

impl Struct {
    /// Find a field by name.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// What changed between two versions of a generated file, structurally.
///
/// Both sides are parsed, so a difference here is a difference in what
/// the code *declares* — never in how it was spaced, ordered or
/// commented. Anything that does not parse yields nothing to say rather
/// than a false alarm.
pub fn differences(old: &str, new: &str) -> Vec<String> {
    let (Some(old), Some(new)) = (summarize(old), summarize(new)) else {
        return Vec::new();
    };

    let mut notes = Vec::new();
    let named = new.structs.len() + new.enums.len() > 1;

    for after in &new.structs {
        let Some(before) = old.struct_named(&after.name) else {
            notes.push(format!("{} is new", after.name));
            continue;
        };
        let mut changed = Vec::new();
        for field in &after.fields {
            match before.field(&field.name) {
                None => changed.push(format!("+{}: {}", field.name, field.ty)),
                Some(was) if was.ty != field.ty => {
                    changed.push(format!("~{}: {} -> {}", field.name, was.ty, field.ty));
                }
                Some(_) => {}
            }
        }
        for field in &before.fields {
            if after.field(&field.name).is_none() {
                changed.push(format!("-{}", field.name));
            }
        }
        if !changed.is_empty() {
            notes.push(label(named, &after.name, &changed));
        }
    }
    for before in &old.structs {
        if new.struct_named(&before.name).is_none() {
            notes.push(format!("{} is gone", before.name));
        }
    }

    for after in &new.enums {
        let Some(before) = old.enum_named(&after.name) else {
            notes.push(format!("{} is new", after.name));
            continue;
        };
        let mut changed = Vec::new();
        for v in &after.variants {
            if !before.variants.contains(v) {
                changed.push(format!("+{v}"));
            }
        }
        for v in &before.variants {
            if !after.variants.contains(v) {
                changed.push(format!("-{v}"));
            }
        }
        if !changed.is_empty() {
            notes.push(label(named, &after.name, &changed));
        }
    }

    // By type, not by block. A type may be implemented across several
    // blocks — one proto generated and one somebody wrote in a keep
    // region — and comparing block to block reports each as having lost
    // the other's methods.
    let mut types: Vec<&str> = new.impls.iter().map(|i| i.type_name.as_str()).collect();
    types.extend(old.impls.iter().map(|i| i.type_name.as_str()));
    types.sort_unstable();
    types.dedup();

    for type_name in types {
        let (before, after) = (old.methods_on(type_name), new.methods_on(type_name));
        let mut changed = Vec::new();
        for method in &after {
            if !before.contains(method) {
                changed.push(format!("+{method}"));
            }
        }
        for method in &before {
            if !after.contains(method) {
                changed.push(format!("-{method}"));
            }
        }
        if !changed.is_empty() {
            notes.push(label(named, type_name, &changed));
        }
    }

    notes
}

/// Name the type only when a file holds more than one; in a file with a
/// single struct the name is noise.
fn label(named: bool, name: &str, changed: &[String]) -> String {
    if named {
        format!("{name}: {}", changed.join(", "))
    } else {
        changed.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
        use uuid::Uuid;

        /// A doc comment, which is not structure.
        #[derive(sqlx::FromRow, Debug)]
        pub struct Tree {
            pub id: Uuid,
            pub width: i32,
            pub notes: Option<String>,
            pub r#type: String,
        }

        pub enum Stage { Planted, Removed }

        impl<'a> TreeMapper<'a> {
            pub fn new(pool: &'a PgPool) -> Self { Self { pool } }
            pub async fn find_by_id(&self, id: Uuid) -> Result<Option<Tree>, sqlx::Error> { todo!() }
        }
    "#;

    #[test]
    fn a_file_reduces_to_its_types() {
        let summary = summarize(SOURCE).expect("parses");

        let tree = summary.struct_named("Tree").expect("Tree is there");
        assert_eq!(tree.field("width").unwrap().ty, "i32");
        assert_eq!(tree.field("notes").unwrap().ty, "Option<String>");
        // A raw identifier is the same field as the word it escapes.
        assert_eq!(tree.field("type").unwrap().ty, "String");

        assert_eq!(
            summary.enum_named("Stage").unwrap().variants,
            ["Planted", "Removed"]
        );
        assert_eq!(summary.methods_on("TreeMapper"), ["new", "find_by_id"]);
    }

    #[test]
    fn formatting_is_not_structure() {
        let reformatted = SOURCE
            .replace("pub width: i32,", "pub width:i32,")
            .replace("Option<String>", "Option < String >")
            .replace(
                "/// A doc comment, which is not structure.",
                "// changed comment",
            );
        assert_eq!(summarize(SOURCE), summarize(&reformatted));
    }

    #[test]
    fn a_changed_type_is_structure() {
        let edited = SOURCE.replace("pub width: i32,", "pub width: f64,");
        assert_ne!(summarize(SOURCE), summarize(&edited));
        assert_eq!(
            summarize(&edited)
                .unwrap()
                .struct_named("Tree")
                .unwrap()
                .field("width")
                .unwrap()
                .ty,
            "f64"
        );
    }

    #[test]
    fn source_that_does_not_parse_is_an_absence_not_a_difference() {
        assert!(summarize("pub struct Broken {").is_none());
        // And nothing is claimed about it either.
        assert!(differences("pub struct Broken {", SOURCE).is_empty());
    }

    #[test]
    fn differences_are_about_declarations_not_bytes() {
        let reformatted = SOURCE
            .replace("pub width: i32,", "pub width:i32,")
            .replace("/// A doc comment, which is not structure.", "// changed");
        assert!(
            differences(SOURCE, &reformatted).is_empty(),
            "reformatting is not a change"
        );

        let edited = SOURCE.replace("pub width: i32,", "pub width: f64,");
        assert_eq!(differences(SOURCE, &edited), ["Tree: ~width: i32 -> f64"]);
    }

    #[test]
    fn a_dropped_column_and_a_new_one_read_as_such() {
        let after = SOURCE
            .replace("    pub notes: Option<String>,\n", "")
            .replace(
                "pub width: i32,",
                "pub width: i32,\n    pub colour: Option<String>,",
            );
        let notes = differences(SOURCE, &after);
        assert_eq!(notes, ["Tree: +colour: Option<String>, -notes"]);
    }

    /// A type implemented across two blocks — one generated, one
    /// hand-written in a keep region — is one type, not two.
    #[test]
    fn methods_are_compared_by_type_not_by_block() {
        let split = format!("{SOURCE}\nimpl<'a> TreeMapper<'a> {{ pub fn mine(&self) {{}} }}");
        // Adding a second block adds a method and removes none.
        assert_eq!(differences(SOURCE, &split), ["TreeMapper: +mine"]);
        // And going back the other way removes it and nothing else.
        assert_eq!(differences(&split, SOURCE), ["TreeMapper: -mine"]);
    }

    #[test]
    fn an_added_method_shows_against_its_type() {
        let after = SOURCE.replace(
            "pub async fn find_by_id",
            "pub async fn list(&self) -> Result<Vec<Tree>, sqlx::Error> { todo!() }\n            pub async fn find_by_id",
        );
        assert_eq!(differences(SOURCE, &after), ["TreeMapper: +list"]);
    }
}
