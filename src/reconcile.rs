//! Bringing an existing file into line with the database, one line at a
//! time.
//!
//! The database is right. When a file disagrees with it — a migration
//! changed `numeric` to `integer`, or somebody changed a field by hand —
//! the file is wrong and proto fixes it. What it must not do is take the
//! rest of the file with it.
//!
//! So rather than rendering a new file over the old one, proto renders
//! what the file *should* say, parses both, and edits only where they
//! disagree. A field whose type is wrong has its type replaced and
//! nothing else: the comment a developer wrote above it last week is
//! still above it, the one below is still below, and an `impl` further
//! down is untouched. Nothing has to be marked to be spared, because
//! nothing is being rewritten in the first place.

use std::collections::BTreeMap;

use proc_macro2::LineColumn;
use quote::ToTokens;
use syn::spanned::Spanned;

/// One replacement in the source: a byte range and what goes there.
#[derive(Debug)]
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Edit `existing` until it says what `rendered` says, and no further.
///
/// Returns `None` when either side does not parse, which is not a
/// failure — it means proto has nothing reliable to say, and the caller
/// should fall back to writing the file whole.
pub fn reconcile(existing: &str, rendered: &str) -> Option<String> {
    let old = syn::parse_file(existing).ok()?;
    let new = syn::parse_file(rendered).ok()?;

    let offsets = Offsets::new(existing);
    let rendered_offsets = Offsets::new(rendered);
    let mut edits = Vec::new();

    // What the file already has, by what it is.
    let mut present: BTreeMap<String, &syn::Item> = BTreeMap::new();
    for item in &old.items {
        if let Some(key) = key(item) {
            present.insert(key, item);
        }
    }

    let mut appended = String::new();
    for want in &new.items {
        let Some(key) = key(want) else { continue };
        match present.get(&key) {
            // Already here, in some form: correct it in place.
            Some(have) => match (have, want) {
                (syn::Item::Struct(have), syn::Item::Struct(want)) => {
                    fields(have, want, &offsets, existing, &mut edits);
                }
                // An enum's variants or a mapper's methods are not
                // somewhere a person edits a line at a time, so those
                // are replaced as a whole — but only they are.
                _ if !same(have, want) => {
                    let (start, end) = span_of(have, &offsets);
                    edits.push(Edit {
                        start,
                        end,
                        text: text_of(want, rendered, &rendered_offsets),
                    });
                }
                _ => {}
            },
            // Not here at all: a new table's type, or an import a new
            // column needs.
            None => {
                appended.push('\n');
                appended.push_str(&text_of(want, rendered, &rendered_offsets));
                appended.push('\n');
            }
        }
    }

    let mut out = apply(existing, edits);
    if !appended.is_empty() {
        out.push_str(&appended);
    }
    Some(out)
}

/// Reconcile one struct's fields: fix a type, add a column, drop one.
fn fields(
    have: &syn::ItemStruct,
    want: &syn::ItemStruct,
    offsets: &Offsets,
    source: &str,
    edits: &mut Vec<Edit>,
) {
    let name_of = |f: &syn::Field| {
        f.ident
            .as_ref()
            .map(|i| i.to_string().trim_start_matches("r#").to_string())
    };

    // A type that disagrees with its column is replaced, and only it.
    for field in &have.fields {
        let Some(name) = name_of(field) else { continue };
        let Some(target) = want
            .fields
            .iter()
            .find(|f| name_of(f).as_deref() == Some(&name))
        else {
            continue;
        };
        let (is, should) = (type_text(&field.ty), type_text(&target.ty));
        if is != should {
            let (start, end) = range(field.ty.span(), offsets);
            edits.push(Edit {
                start,
                end,
                text: should,
            });
        }
    }

    // A column the file has never heard of goes in where the database
    // has it — after whichever of its neighbours the file already
    // knows. Nothing that is already there moves: a field carries the
    // comment above it, and no comment is worth a line's tidiness.
    let here: Vec<String> = have.fields.iter().filter_map(name_of).collect();
    let order: Vec<String> = want.fields.iter().filter_map(name_of).collect();

    let mut additions: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (index, name) in order.iter().enumerate() {
        if here.contains(name) {
            continue;
        }
        let Some(field) = want
            .fields
            .iter()
            .find(|f| name_of(f).as_deref() == Some(name))
        else {
            continue;
        };

        // The nearest earlier column the file already has.
        let anchor = order[..index]
            .iter()
            .rev()
            .find(|earlier| here.contains(earlier))
            .and_then(|earlier| {
                have.fields
                    .iter()
                    .find(|f| name_of(f).as_deref() == Some(earlier.as_str()))
            });

        let (at, indent) = match anchor {
            Some(field) => {
                let end = line_end(source, offsets.of(field.span().end()));
                (end, indent_of(source, offsets.of(field.span().start())))
            }
            // Nothing earlier survives, so it goes before the first
            // field the file does have.
            None => match have.fields.iter().next() {
                Some(first) => {
                    let at = line_start(source, offsets.of(first.span().start()));
                    (
                        at.saturating_sub(1),
                        indent_of(source, offsets.of(first.span().start())),
                    )
                }
                None => (
                    line_end(source, offsets.of(have.span().start())),
                    "    ".to_string(),
                ),
            },
        };

        additions.entry(at).or_default().push(format!(
            "\n{indent}pub {}: {},",
            field.ident.as_ref().unwrap(),
            type_text(&field.ty)
        ));
    }

    for (at, lines) in additions {
        edits.push(Edit {
            start: at,
            end: at,
            text: lines.concat(),
        });
    }

    // A column that is gone takes its field, and the doc comment that
    // came from its own COMMENT ON, with it.
    let wanted: Vec<String> = want.fields.iter().filter_map(name_of).collect();
    for field in &have.fields {
        let Some(name) = name_of(field) else { continue };
        if wanted.contains(&name) {
            continue;
        }
        let start = field
            .attrs
            .first()
            .map_or_else(|| field.span().start(), |a| a.span().start());
        let (from, to) = (offsets.of(start), offsets.of(field.span().end()));
        edits.push(Edit {
            start: line_start(source, from),
            end: line_end(source, to) + 1,
            text: String::new(),
        });
    }
}

// ── Text and positions ──────────────────────────────────────────────────────

/// What an item is, for matching one file's against another's.
fn key(item: &syn::Item) -> Option<String> {
    Some(match item {
        syn::Item::Struct(i) => format!("struct {}", i.ident),
        syn::Item::Enum(i) => format!("enum {}", i.ident),
        syn::Item::Impl(i) => format!("impl {}", impl_target(i)?),
        syn::Item::Use(i) => format!("use {}", normalise(i)),
        syn::Item::Mod(i) => format!("mod {}", i.ident),
        syn::Item::Fn(i) => format!("fn {}", i.sig.ident),
        _ => return None,
    })
}

fn impl_target(item: &syn::ItemImpl) -> Option<String> {
    match &*item.self_ty {
        syn::Type::Path(p) => Some(p.path.segments.last()?.ident.to_string()),
        _ => None,
    }
}

fn normalise(item: &impl ToTokens) -> String {
    item.to_token_stream().to_string().replace(' ', "")
}

fn same(a: &syn::Item, b: &syn::Item) -> bool {
    normalise(a) == normalise(b)
}

fn type_text(ty: &syn::Type) -> String {
    ty.to_token_stream().to_string().replace(' ', "")
}

fn span_of(item: &syn::Item, offsets: &Offsets) -> (usize, usize) {
    range(item.span(), offsets)
}

/// The source text of an item, taken from the file it came from so its
/// formatting survives.
fn text_of(item: &syn::Item, source: &str, offsets: &Offsets) -> String {
    let (start, end) = range(item.span(), offsets);
    source[start..end].to_string()
}

fn range(span: proc_macro2::Span, offsets: &Offsets) -> (usize, usize) {
    (offsets.of(span.start()), offsets.of(span.end()))
}

fn line_start(source: &str, at: usize) -> usize {
    source[..at].rfind('\n').map_or(0, |n| n + 1)
}

fn line_end(source: &str, at: usize) -> usize {
    source[at..].find('\n').map_or(source.len(), |n| at + n)
}

fn indent_of(source: &str, at: usize) -> String {
    let line = &source[line_start(source, at)..at];
    line.chars().take_while(|c| c.is_whitespace()).collect()
}

/// Line and column, as syn reports them, to a byte offset.
struct Offsets {
    lines: Vec<usize>,
    source: String,
}

impl Offsets {
    fn new(source: &str) -> Self {
        let mut lines = vec![0];
        for (i, c) in source.char_indices() {
            if c == '\n' {
                lines.push(i + 1);
            }
        }
        Self {
            lines,
            source: source.to_string(),
        }
    }

    /// syn counts columns in characters, not bytes.
    fn of(&self, at: LineColumn) -> usize {
        let start = self
            .lines
            .get(at.line.saturating_sub(1))
            .copied()
            .unwrap_or(0);
        self.source[start..]
            .char_indices()
            .nth(at.column)
            .map_or(self.source.len(), |(i, _)| start + i)
    }
}

/// Apply edits from the back, so earlier offsets stay valid.
fn apply(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = source.to_string();
    for edit in edits {
        if edit.start <= edit.end && edit.end <= out.len() {
            out.replace_range(edit.start..edit.end, &edit.text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file as a developer would have it: proto's struct, with their
    /// own comments woven through it and their own code below.
    const LIVED_IN: &str = r#"// @generated by proto 0.1.0 — do not edit by hand.

use rust_decimal::Decimal;
use uuid::Uuid;

pub struct Item {
    pub id: Uuid,
    // Prices are ex-VAT — checked with finance 2026-08-30.
    pub price: Option<f64>,
    // The tags come from the importer, not from us.
    pub tags: Option<Vec<String>>,
}

impl Item {
    /// Written last week.
    pub fn dear(&self) -> bool {
        self.price.is_some()
    }
}
"#;

    const CORRECT: &str = r#"// @generated by proto 0.1.0 — do not edit by hand.

use rust_decimal::Decimal;
use uuid::Uuid;

pub struct Item {
    pub id: Uuid,
    pub price: Option<Decimal>,
    pub tags: Option<Vec<String>>,
}
"#;

    #[test]
    fn a_wrong_type_is_fixed_and_nothing_else_moves() {
        let out = reconcile(LIVED_IN, CORRECT).expect("both parse");

        // The database is right, so the type is now the database's.
        assert!(out.contains("pub price: Option<Decimal>,"), "{out}");
        assert!(!out.contains("Option<f64>"), "{out}");

        // And everything a person put there is exactly where it was.
        assert!(
            out.contains("// Prices are ex-VAT — checked with finance 2026-08-30."),
            "{out}"
        );
        assert!(
            out.contains("// The tags come from the importer, not from us."),
            "{out}"
        );
        assert!(out.contains("/// Written last week."), "{out}");
        assert!(out.contains("pub fn dear(&self) -> bool {"), "{out}");

        // The comment above the corrected line is still above it, and
        // the one below still below.
        let comment = out.find("// Prices are ex-VAT").unwrap();
        let price = out.find("pub price:").unwrap();
        let after = out.find("// The tags come from").unwrap();
        assert!(comment < price && price < after, "{out}");
    }

    #[test]
    fn a_new_column_arrives_without_disturbing_the_rest() {
        let with_colour = CORRECT.replace(
            "    pub tags: Option<Vec<String>>,",
            "    pub tags: Option<Vec<String>>,\n    pub colour: Option<String>,",
        );
        let out = reconcile(LIVED_IN, &with_colour).expect("both parse");

        assert!(out.contains("pub colour: Option<String>,"), "{out}");
        assert!(out.contains("// Prices are ex-VAT"), "{out}");
        assert!(out.contains("pub fn dear(&self) -> bool {"), "{out}");
        // Still parses as Rust, which is the only test that counts.
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    /// Column order in Postgres is a storage artifact — dropping and
    /// re-adding a column moves it to the end — and it means nothing to
    /// `FromRow`, which matches by name. So an existing field never
    /// moves. A new one still lands where the database has it.
    #[test]
    fn a_new_column_lands_beside_its_neighbours_and_moves_nothing() {
        let reordered = r#"// @generated by proto 0.1.0 — do not edit by hand.

use rust_decimal::Decimal;
use uuid::Uuid;

pub struct Item {
    pub id: Uuid,
    pub colour: Option<String>,
    pub price: Option<Decimal>,
    pub tags: Option<Vec<String>>,
}
"#;
        let out = reconcile(LIVED_IN, reordered).expect("both parse");

        // The new column sits after `id`, where the database has it.
        let id = out.find("pub id:").unwrap();
        let colour = out.find("pub colour:").unwrap();
        let price = out.find("pub price:").unwrap();
        assert!(id < colour && colour < price, "{out}");

        // And the fields that were already there are in the order they
        // were, still carrying their comments.
        let comment = out.find("// Prices are ex-VAT").unwrap();
        assert!(comment < price, "{out}");
        assert!(out.find("pub tags:").unwrap() > price, "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    #[test]
    fn a_dropped_column_takes_its_field_and_leaves_the_rest() {
        let without_tags = CORRECT.replace("    pub tags: Option<Vec<String>>,\n", "");
        let out = reconcile(LIVED_IN, &without_tags).expect("both parse");

        assert!(!out.contains("pub tags:"), "{out}");
        assert!(out.contains("pub price: Option<Decimal>,"), "{out}");
        assert!(out.contains("pub fn dear(&self) -> bool {"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    /// Fields are matched by name, so how they are arranged is the
    /// file's business. An engineer who reorders a struct — grouping
    /// the keys, say, or putting the interesting columns first — has
    /// changed nothing proto looks at.
    #[test]
    fn a_hand_reordered_struct_is_left_exactly_as_it_is() {
        let reordered = r#"// @generated by proto 0.1.0 — do not edit by hand.

use rust_decimal::Decimal;
use uuid::Uuid;

pub struct Item {
    // Grouped the way this team reads them.
    pub tags: Option<Vec<String>>,
    pub price: Option<Decimal>,
    pub id: Uuid,
}
"#;
        // Same fields, same types, different order: nothing to do.
        assert_eq!(reconcile(reordered, CORRECT).unwrap(), reordered);
    }

    #[test]
    fn a_new_column_joins_a_reordered_struct_without_rearranging_it() {
        let reordered = r#"// @generated by proto 0.1.0 — do not edit by hand.

use rust_decimal::Decimal;
use uuid::Uuid;

pub struct Item {
    pub tags: Option<Vec<String>>,
    pub price: Option<Decimal>,
    pub id: Uuid,
}
"#;
        let with_colour = CORRECT.replace(
            "    pub id: Uuid,",
            "    pub id: Uuid,\n    pub colour: Option<String>,",
        );
        let out = reconcile(reordered, &with_colour).expect("both parse");

        // The order the engineer chose is still the order.
        let tags = out.find("pub tags:").unwrap();
        let price = out.find("pub price:").unwrap();
        let id = out.find("pub id:").unwrap();
        assert!(tags < price && price < id, "{out}");

        // And the new column arrived, after the neighbour it follows in
        // the database.
        assert!(out.contains("pub colour: Option<String>,"), "{out}");
        assert!(out.find("pub colour:").unwrap() > id, "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    #[test]
    fn a_file_that_already_agrees_is_returned_untouched() {
        let out = reconcile(CORRECT, CORRECT).expect("both parse");
        assert_eq!(out, CORRECT);
    }

    #[test]
    fn source_that_does_not_parse_hands_the_decision_back() {
        assert!(reconcile("pub struct Broken {", CORRECT).is_none());
        assert!(reconcile(CORRECT, "pub struct Broken {").is_none());
    }
}
