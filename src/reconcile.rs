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
                    fields(
                        have,
                        want,
                        &offsets,
                        existing,
                        rendered,
                        &rendered_offsets,
                        &mut edits,
                    );
                }
                (syn::Item::Impl(have), syn::Item::Impl(want)) => {
                    methods(
                        have,
                        want,
                        &offsets,
                        existing,
                        rendered,
                        &rendered_offsets,
                        &mut edits,
                    );
                }
                // An enum's variants are not somewhere a line gets
                // edited, so they are replaced as a whole.
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

    // A `pub mod x;` line for a module the render no longer declares —
    // a table that was pruned — is taken back, or the crate fails to
    // find the file. Only the bare declaration: a module with a body is
    // somebody's code.
    for item in &old.items {
        let syn::Item::Mod(module) = item else {
            continue;
        };
        if module.content.is_some() || new.items.iter().any(|want| same(want, item)) {
            continue;
        }
        let (start, end) = span_of(item, &offsets);
        edits.push(Edit {
            start: line_start(existing, start),
            end: (line_end(existing, end) + 1).min(existing.len()),
            text: String::new(),
        });
    }

    let mut out = apply(existing, edits);
    if !appended.is_empty() {
        out.push_str(&appended);
    }

    // An import proto wrote for a sibling model's type, that the render
    // no longer has and nothing else in the file uses, is taken back:
    // left there it fails the consumer's `-D warnings`. One that is
    // still used is somebody's, whatever the render says. Decided on the
    // corrected text, once the field that used it is gone.
    let wanted: std::collections::BTreeSet<String> = new.items.iter().filter_map(key).collect();
    for item in &old.items {
        let syn::Item::Use(import) = item else {
            continue;
        };
        if wanted.contains(&key(item).unwrap_or_default()) {
            continue;
        }
        let Some(leaf) = sibling_import(import) else {
            continue;
        };
        if uses_of(&out, &leaf) > 1 {
            continue;
        }
        let (start, end) = span_of(item, &offsets);
        let from = line_start(existing, start);
        let to = (line_end(existing, end) + 1).min(existing.len());
        out = out.replacen(&existing[from..to], "", 1);
    }
    Some(out)
}

/// The type a `use super::<module>::<Type>;` brings in — the shape of
/// an import of a sibling model, which is the only kind proto writes
/// with a `super` path — or `None` for any other import.
fn sibling_import(item: &syn::ItemUse) -> Option<String> {
    if !matches!(item.vis, syn::Visibility::Inherited) {
        return None;
    }
    let syn::UseTree::Path(first) = &item.tree else {
        return None;
    };
    if first.ident != "super" {
        return None;
    }
    let syn::UseTree::Path(module) = &*first.tree else {
        return None;
    };
    if module.ident == "enums" {
        return None;
    }
    match &*module.tree {
        syn::UseTree::Name(name) => Some(name.ident.to_string()),
        _ => None,
    }
}

/// How many times `word` appears in `source` as a whole identifier —
/// the import line itself counts for one.
fn uses_of(source: &str, word: &str) -> usize {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    source
        .match_indices(word)
        .filter(|(at, _)| !source[..*at].chars().next_back().is_some_and(is_ident))
        .filter(|(at, _)| {
            !source[at + word.len()..]
                .chars()
                .next()
                .is_some_and(is_ident)
        })
        .count()
}

/// Reconcile one struct's fields: fix a type, add a column, drop one.
#[allow(clippy::too_many_arguments)]
fn fields(
    have: &syn::ItemStruct,
    want: &syn::ItemStruct,
    offsets: &Offsets,
    source: &str,
    rendered: &str,
    rendered_offsets: &Offsets,
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

        // The field as rendered, attributes and doc comment included: a
        // `#[sqlx(rename)]` or `#[sqlx(skip)]` is part of what makes the
        // field decode, not decoration. Re-indented to where it lands.
        let text = &rendered
            [rendered_offsets.of(field.span().start())..rendered_offsets.of(field.span().end())];
        let lines: Vec<String> = text
            .lines()
            .map(|line| format!("\n{indent}{}", line.trim_start()))
            .collect();
        additions
            .entry(at)
            .or_default()
            .push(format!("{},", lines.concat()));
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

/// Reconcile one impl block a method at a time.
///
/// Proto owns the methods it derives from the catalog: a `create` whose
/// column list is out of date is put back, because the statement it
/// runs is wrong. It owns nothing else in the block. A method somebody
/// added — a query the schema does not imply and proto would never
/// derive — is not proto's to have an opinion about, and is left where
/// it is.
#[allow(clippy::too_many_arguments)]
fn methods(
    have: &syn::ItemImpl,
    want: &syn::ItemImpl,
    offsets: &Offsets,
    source: &str,
    rendered: &str,
    rendered_offsets: &Offsets,
    edits: &mut Vec<Edit>,
) {
    let name_of = |item: &syn::ImplItem| match item {
        syn::ImplItem::Fn(f) => Some(f.sig.ident.to_string()),
        _ => None,
    };
    let here: Vec<String> = have.items.iter().filter_map(name_of).collect();

    // A method proto generates, that says something else now, is put
    // back. Only that method: the ones around it, and whatever sits
    // between them, do not move.
    for item in &have.items {
        let Some(name) = name_of(item) else { continue };
        let Some(target) = want
            .items
            .iter()
            .find(|i| name_of(i).as_deref() == Some(&name))
        else {
            continue; // not proto's method
        };
        if normalise(item) != normalise(target) {
            edits.push(Edit {
                start: item_start(item, offsets),
                end: offsets.of(item.span().end()),
                text: rendered[item_start(target, rendered_offsets)
                    ..rendered_offsets.of(target.span().end())]
                    .to_string(),
            });
        }
    }

    // A method proto owned that the render no longer has — a finder for
    // a constraint that is gone, a loader for a field that was renamed —
    // is taken back, blank line and all. Only what carries the notice:
    // a method somebody wrote lacks it, and is not proto's to remove.
    let order: Vec<String> = want.items.iter().filter_map(name_of).collect();
    for item in &have.items {
        let Some(name) = name_of(item) else { continue };
        if order.contains(&name) || !owned(item) {
            continue;
        }
        let from = line_start(source, item_start(item, offsets));
        let from = if source[..from].ends_with("\n\n") {
            from - 1
        } else {
            from
        };
        let to = (line_end(source, offsets.of(item.span().end())) + 1).min(source.len());
        edits.push(Edit {
            start: from,
            end: to,
            text: String::new(),
        });
    }

    // A method the file does not have goes in after the last one it
    // does, so proto's stay together and nobody else's move.
    let mut additions: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (index, name) in order.iter().enumerate() {
        if here.contains(name) {
            continue;
        }
        let Some(item) = want
            .items
            .iter()
            .find(|i| name_of(i).as_deref() == Some(name))
        else {
            continue;
        };
        let anchor = order[..index]
            .iter()
            .rev()
            .find(|earlier| here.contains(earlier))
            .and_then(|earlier| {
                have.items
                    .iter()
                    .find(|i| name_of(i).as_deref() == Some(earlier.as_str()))
            });

        let at = match anchor {
            Some(previous) => line_end(source, offsets.of(previous.span().end())),
            None => line_end(source, offsets.of(have.brace_token.span.open().end())),
        };
        let text = rendered
            [item_start(item, rendered_offsets)..rendered_offsets.of(item.span().end())]
            .to_string();
        additions
            .entry(at)
            .or_default()
            .push(format!("\n\n    {text}"));
    }
    for (at, blocks) in additions {
        edits.push(Edit {
            start: at,
            end: at,
            text: blocks.concat(),
        });
    }
}

/// Whether a method's doc comment carries proto's notice — the one
/// [`crate::render::mapper`] closes every generated method with.
fn owned(item: &syn::ImplItem) -> bool {
    let syn::ImplItem::Fn(f) = item else {
        return false;
    };
    f.attrs.iter().any(|attr| {
        attr.path().is_ident("doc")
            && matches!(
                &attr.meta,
                syn::Meta::NameValue(syn::MetaNameValue {
                    value: syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(text),
                        ..
                    }),
                    ..
                }) if text.value().contains("proto owns this method")
            )
    })
}

// ── Text and positions ──────────────────────────────────────────────────────

/// Where an item really begins: at its first attribute, since a doc
/// comment is one and belongs to what it documents.
fn item_start(item: &impl Spanned, offsets: &Offsets) -> usize {
    offsets.of(item.span().start())
}

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

fn normalise<T: ToTokens + ?Sized>(item: &T) -> String {
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
    const LIVED_IN: &str = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

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

    const CORRECT: &str = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

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

    /// A field that is not a column only decodes because of what is
    /// written above it, so a new one arrives with its attributes and
    /// its doc comment, not just its declaration.
    #[test]
    fn a_new_field_arrives_with_its_attributes() {
        let with_children = CORRECT.replace(
            "    pub tags: Option<Vec<String>>,",
            "    pub tags: Option<Vec<String>>,\n    /// Rows of `shop.variant`.\n    \
             #[sqlx(skip)]\n    #[serde(default)]\n    pub children: Vec<Variant>,",
        );
        let out = reconcile(LIVED_IN, &with_children).expect("both parse");
        assert!(
            out.contains(
                "    /// Rows of `shop.variant`.\n    #[sqlx(skip)]\n    #[serde(default)]\n    \
                 pub children: Vec<Variant>,\n"
            ),
            "{out}"
        );
        assert!(out.contains("// The tags come from the importer"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    /// Column order in Postgres is a storage artifact — dropping and
    /// re-adding a column moves it to the end — and it means nothing to
    /// `FromRow`, which matches by name. So an existing field never
    /// moves. A new one still lands where the database has it.
    #[test]
    fn a_new_column_lands_beside_its_neighbours_and_moves_nothing() {
        let reordered = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

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
        let reordered = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

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
        let reordered = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

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

    /// A mapper as it looks once somebody has worked in it: proto's
    /// methods, and one of their own the schema does not imply.
    const MAPPER: &str = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

use sqlx::PgPool;

impl<'a> ItemMapper<'a> {
    /// Look up the row identified by `id`.
    pub async fn find_by_id(&self, id: Uuid) -> Result<Option<Item>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM mp.item WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool)
            .await
    }

    // Ours, not proto's — the schema does not imply this one.
    pub async fn cheapest(&self) -> Result<Option<Item>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM mp.item ORDER BY price LIMIT 1")
            .fetch_optional(self.pool)
            .await
    }
}
"#;

    #[test]
    fn a_method_proto_does_not_generate_is_not_proto_s_to_touch() {
        // Proto's own method has moved on; theirs is not in the render
        // at all.
        let rendered = MAPPER
            .replace(
                r#"sqlx::query_as("SELECT * FROM mp.item WHERE id = $1")"#,
                r#"sqlx::query_as("SELECT * FROM mp.item WHERE id = $1 AND live")"#,
            )
            .replace(
                "    // Ours, not proto's — the schema does not imply this one.\n",
                "",
            );
        let rendered = rendered[..rendered.find("pub async fn cheapest").unwrap()]
            .trim_end()
            .to_string()
            + "\n}\n";

        let out = reconcile(MAPPER, &rendered).expect("both parse");

        // Proto's method was put back to what proto says.
        assert!(out.contains("WHERE id = $1 AND live"), "{out}");
        // Theirs is untouched, comment and all.
        assert!(out.contains("// Ours, not proto's"), "{out}");
        assert!(out.contains("pub async fn cheapest"), "{out}");
        assert!(out.contains("ORDER BY price LIMIT 1"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    /// A mapper holding a method proto owned for a field that has since
    /// been renamed, beside one somebody wrote.
    const STALE: &str = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

use sqlx::PgPool;

impl<'a> BinMapper<'a> {
    /// Look up the row identified by `id`.
    ///
    /// proto owns this method and rewrites it when the schema changes, so
    /// edits here do not survive. Add a method of your own beside it.
    pub async fn find_by_id(&self, id: Uuid) -> Result<Option<Bin>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM mp.bin WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool)
            .await
    }

    /// Every `mp.item` row whose `bin_id` is this row's `id`, into `item`.
    ///
    /// proto owns this method and rewrites it when the schema changes, so
    /// edits here do not survive. Add a method of your own beside it.
    pub async fn load_item(&self, row: &mut Bin) -> Result<(), sqlx::Error> {
        row.item = sqlx::query_as("SELECT * FROM mp.item WHERE bin_id = $1")
            .bind(row.id)
            .fetch_all(self.pool)
            .await?;
        Ok(())
    }

    // Ours, not proto's — the schema does not imply this one.
    pub async fn emptiest(&self) -> Result<Option<Bin>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM mp.bin ORDER BY fill LIMIT 1")
            .fetch_optional(self.pool)
            .await
    }
}
"#;

    /// The field was renamed, so the render has `load_children` where
    /// the file has `load_item`. The old method referenced a field that
    /// is gone and would not compile; it carries proto's notice, so it
    /// is proto's to take back. The one without the notice is not.
    #[test]
    fn an_owned_method_the_render_no_longer_has_is_taken_back() {
        let rendered = STALE
            .replace("load_item", "load_children")
            .replace("row.item =", "row.children =")
            .replace("into `item`", "into `children`");
        let rendered = rendered[..rendered.find("    // Ours, not proto's").unwrap()]
            .trim_end()
            .to_string()
            + "\n}\n";

        let out = reconcile(STALE, &rendered).expect("both parse");

        assert!(!out.contains("load_item"), "{out}");
        assert!(!out.contains("row.item ="), "{out}");
        assert!(out.contains("pub async fn load_children"), "{out}");
        assert!(out.contains("pub async fn find_by_id"), "{out}");
        // Theirs stays, comment and all, and the block still parses.
        assert!(out.contains("// Ours, not proto's"), "{out}");
        assert!(out.contains("pub async fn emptiest"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
        // No doubled blank lines where the method used to be.
        assert!(!out.contains("\n\n\n"), "{out}");
    }

    /// A struct whose children field went away takes its import with
    /// it — but only when nothing else in the file names the type.
    #[test]
    fn a_sibling_import_nothing_uses_any_more_is_taken_back() {
        let with_children = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

use super::item::Item;
use uuid::Uuid;

pub struct Bin {
    pub id: Uuid,
    #[sqlx(skip)]
    pub item: Vec<Item>,
}
"#;
        let without = r#"// @generated by proto 0.1.0 — regenerate rather than rewrite.

use uuid::Uuid;

pub struct Bin {
    pub id: Uuid,
}
"#;
        let out = reconcile(with_children, without).expect("both parse");
        assert!(!out.contains("use super::item::Item;"), "{out}");
        assert!(!out.contains("pub item:"), "{out}");
        assert!(out.contains("use uuid::Uuid;"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");

        // Somebody's own code names the type: the import is theirs now.
        let in_use = with_children.to_string()
            + "\nimpl Bin {\n    pub fn first(&self) -> Option<&Item> {\n        None\n    }\n}\n";
        let out = reconcile(&in_use, without).expect("both parse");
        assert!(out.contains("use super::item::Item;"), "{out}");
        assert!(out.contains("Option<&Item>"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    /// A pruned table's module goes out of `mod.rs`, or the crate cannot
    /// find its file. A module with a body is not a declaration proto
    /// makes, and stays.
    #[test]
    fn a_module_the_render_no_longer_declares_is_taken_back() {
        let old = "// @generated by proto 0.1.0\npub mod bin;\npub mod item;\npub mod order;\n\n\
                   pub mod extra {\n    pub fn helper() {}\n}\n";
        let new = "// @generated by proto 0.1.0\npub mod bin;\npub mod item;\n";
        let out = reconcile(old, new).expect("both parse");
        assert!(!out.contains("pub mod order;"), "{out}");
        assert!(out.contains("pub mod bin;\npub mod item;\n"), "{out}");
        assert!(out.contains("pub mod extra {"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    #[test]
    fn a_new_method_joins_the_block_without_moving_anyone() {
        let rendered = MAPPER.replace(
            "    // Ours, not proto's — the schema does not imply this one.\n    pub async fn cheapest(&self) -> Result<Option<Item>, sqlx::Error> {\n        sqlx::query_as(\"SELECT * FROM mp.item ORDER BY price LIMIT 1\")\n            .fetch_optional(self.pool)\n            .await\n    }\n",
            "    pub async fn list(&self) -> Result<Vec<Item>, sqlx::Error> {\n        sqlx::query_as(\"SELECT * FROM mp.item\")\n            .fetch_all(self.pool)\n            .await\n    }\n",
        );
        let out = reconcile(MAPPER, &rendered).expect("both parse");

        assert!(
            out.contains("pub async fn list"),
            "the new method arrived: {out}"
        );
        assert!(
            out.contains("pub async fn cheapest"),
            "theirs survived: {out}"
        );
        assert!(out.contains("// Ours, not proto's"), "{out}");
        assert!(syn::parse_file(&out).is_ok(), "{out}");
    }

    #[test]
    fn an_untouched_mapper_is_returned_as_it_was() {
        assert_eq!(reconcile(MAPPER, MAPPER).unwrap(), MAPPER);
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
