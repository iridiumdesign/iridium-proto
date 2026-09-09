//! Quoting for generated SQL.
//!
//! Identifiers come out of the catalogs as whatever someone typed inside
//! a `CREATE TABLE`. Most are plain lowercase words that need nothing
//! done to them, and quoting those anyway would leave every generated
//! statement full of `"shop"."product"` noise for no gain — the output is
//! meant to be read and edited. So quoting is applied where it changes
//! the meaning and skipped where it does not.
//!
//! An identifier is left bare when it is a plain lowercase word that
//! Postgres will not read as a keyword. Everything else is
//! double-quoted, with internal quotes doubled: a mixed-case name (which
//! Postgres would otherwise fold), a name with a space or punctuation, a
//! name starting with a digit, and any of the reserved words.

/// Quote an identifier if it needs it.
///
/// ```
/// use iridium_proto::quoting::ident;
///
/// assert_eq!(ident("product"), "product");
/// assert_eq!(ident("order"), r#""order""#);      // reserved
/// assert_eq!(ident("Product"), r#""Product""#);  // would fold
/// assert_eq!(ident("my col"), r#""my col""#);
/// assert_eq!(ident(r#"we"rd"#), r#""we""rd""#);  // doubled
/// ```
pub fn ident(name: &str) -> String {
    if bare(name) {
        return name.to_string();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a schema-qualified name, each part on its own merits.
///
/// ```
/// use iridium_proto::quoting::qualified;
///
/// assert_eq!(qualified("shop", "product"), "shop.product");
/// assert_eq!(qualified("shop", "order"), r#"shop."order""#);
/// ```
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", ident(schema), ident(name))
}

/// A single-quoted SQL string literal, for the places a name is compared
/// as a value rather than used as an identifier.
///
/// ```
/// use iridium_proto::quoting::literal;
///
/// assert_eq!(literal("shop"), "'shop'");
/// assert_eq!(literal("it's"), "'it''s'");
/// ```
pub fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Whether Postgres will read this back unchanged without quotes: a
/// lowercase letter or underscore, then lowercase letters, digits,
/// underscores or dollar signs, and not a reserved word.
fn bare(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$') {
        return false;
    }
    RESERVED.binary_search(&name).is_err()
}

/// PostgreSQL's reserved key words, both the plain reserved set and the
/// set that is reserved but may still name a function or type. The
/// second group is legal unquoted in some positions and not others;
/// quoting it everywhere costs nothing and removes the question.
///
/// Sorted, because [`bare`] binary searches it.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "binary",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "lateral",
    "leading",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "natural",
    "not",
    "notnull",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "overlaps",
    "placing",
    "primary",
    "references",
    "returning",
    "right",
    "select",
    "session_user",
    "similar",
    "some",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserved_list_is_sorted() {
        // bare() binary searches it, so an out-of-order entry would be
        // silently invisible.
        let mut sorted = RESERVED.to_vec();
        sorted.sort_unstable();
        assert_eq!(RESERVED, sorted.as_slice());
    }

    #[test]
    fn ordinary_names_are_left_alone() {
        for name in ["product", "order_item", "x1", "_private", "a$b"] {
            assert_eq!(ident(name), name, "{name} should not be quoted");
        }
    }

    #[test]
    fn names_that_would_change_meaning_are_quoted() {
        for name in [
            "order",
            "user",
            "table",
            "select",
            "Product",
            "PRODUCT",
            "my col",
            "1st",
            "",
            "with-dash",
            "café",
        ] {
            assert!(
                ident(name).starts_with('"'),
                "{name} should have been quoted, got {}",
                ident(name)
            );
        }
    }

    #[test]
    fn embedded_quotes_are_doubled() {
        assert_eq!(ident(r#"a"b"#), r#""a""b""#);
        assert_eq!(literal("it's"), "'it''s'");
        // The pair a quoting bug would let through.
        assert_eq!(
            ident(r#"a"; DROP TABLE t; --"#),
            r#""a""; DROP TABLE t; --""#
        );
    }
}
