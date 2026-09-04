//! Identifier conversions. Deliberately dumb and predictable: a table name
//! is not singularized, it is only re-cased. `member_units` becomes
//! `MemberUnits`, and `--name` exists for the times that reads wrong.

/// Rust keywords that cannot appear as a bare identifier. Struct fields hit
/// this often enough (`type`, `ref`, `match`) to be worth handling; they are
/// emitted as raw identifiers, which sqlx and serde both match by name.
const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "async", "await", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "try", "typeof", "unsized", "virtual", "yield", "gen",
];

/// `member_units` -> `MemberUnits`, `tree status` -> `TreeStatus`.
pub fn pascal_case(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut upper_next = true;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            if upper_next {
                out.extend(c.to_uppercase());
                upper_next = false;
            } else {
                out.push(c);
            }
        } else {
            upper_next = true;
        }
    }
    if out.is_empty() {
        out.push('X');
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'V');
    }
    out
}

/// `MemberUnits` -> `member_units`. Used to test whether an enum label
/// round-trips through `rename_all = "snake_case"`.
pub fn snake_case(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    let mut prev_lower_or_digit = false;
    for c in input.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower_or_digit {
                out.push('_');
            }
            out.extend(c.to_lowercase());
            prev_lower_or_digit = false;
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_lower_or_digit = true;
        } else {
            out.push('_');
            prev_lower_or_digit = false;
        }
    }
    out
}

/// A field or module name that is safe to emit. Keywords come back as raw
/// identifiers (`r#type`) so the name still matches the column.
pub fn ident(input: &str) -> String {
    let mut out: String = input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if KEYWORDS.contains(&out.as_str()) {
        // `self`, `super`, `crate` and `Self` cannot be raw identifiers.
        if matches!(out.as_str(), "self" | "super" | "crate") {
            out.push('_');
        } else {
            out.insert_str(0, "r#");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal() {
        assert_eq!(pascal_case("member_units"), "MemberUnits");
        assert_eq!(pascal_case("species"), "Species");
        assert_eq!(pascal_case("2fa_token"), "V2faToken");
        assert_eq!(pascal_case("tree-status"), "TreeStatus");
    }

    #[test]
    fn snake() {
        assert_eq!(snake_case("MemberUnits"), "member_units");
        assert_eq!(snake_case("Species"), "species");
        assert_eq!(snake_case("NeedsWork"), "needs_work");
    }

    #[test]
    fn idents() {
        assert_eq!(ident("type"), "r#type");
        assert_eq!(ident("self"), "self_");
        assert_eq!(ident("common name"), "common_name");
        assert_eq!(ident("1st"), "_1st");
    }
}
