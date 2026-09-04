//! Postgres type -> Rust type.
//!
//! The built-in map covers what sqlx can decode out of the box. Anything
//! else comes back as `String` with a TODO and a warning on stderr; the
//! `[generate.types]` table in the config is the fix, and it also overrides
//! the built-ins when a project wants a different backend.

use std::collections::HashMap;

use crate::config::Generate;
use crate::introspect::PgType;
use crate::naming;

/// A Rust type, ready to write into a struct.
#[derive(Debug, Clone)]
pub struct Mapped {
    /// The Rust type as written in the struct, using short names.
    pub text: String,
    /// Full paths to `use`, e.g. `chrono::DateTime`.
    pub imports: Vec<String>,
    /// The Postgres type name proto did not recognise, if any.
    pub unmapped: Option<String>,
    /// Whether the type is `Copy`, which decides how a mapper binds it:
    /// by value, or by reference. Getting this wrong in one direction is
    /// a clippy warning in the generated crate and in the other a
    /// compile error, so an unknown type is assumed not to be.
    pub copy: bool,
}

impl Mapped {
    /// A type a mapper has to borrow to bind: `String`, `Vec<_>`, and
    /// anything proto cannot vouch for.
    fn borrowed(text: impl Into<String>, imports: &[&str]) -> Self {
        Self {
            text: text.into(),
            imports: imports.iter().map(|s| (*s).to_string()).collect(),
            unmapped: None,
            copy: false,
        }
    }

    /// A `Copy` type, which a mapper binds by value.
    fn copied(text: impl Into<String>, imports: &[&str]) -> Self {
        Self {
            copy: true,
            ..Self::borrowed(text, imports)
        }
    }
}

/// Map a column type, wrapping arrays in `Vec` and applying overrides.
///
/// # Examples
///
/// ```
/// use iridium_proto::config::Generate;
/// use iridium_proto::introspect::PgType;
/// use iridium_proto::typemap::map;
///
/// let generate = Generate::default();
///
/// let ts = map(&PgType::Scalar("timestamptz".into()), &generate);
/// assert_eq!(ts.text, "DateTime<Utc>");
/// assert!(ts.copy); // so a mapper binds it by value
///
/// let tags = PgType::Array(Box::new(PgType::Scalar("text".into())));
/// assert_eq!(map(&tags, &generate).text, "Vec<String>");
///
/// // What proto does not know, it says so about rather than guessing.
/// let unknown = map(&PgType::Scalar("geometry".into()), &generate);
/// assert_eq!(unknown.unmapped.as_deref(), Some("geometry"));
/// ```
///
/// The escape hatch is `[generate.types]`, which also overrides the
/// built-in map:
///
/// ```
/// use iridium_proto::config::Generate;
/// use iridium_proto::introspect::PgType;
/// use iridium_proto::typemap::map;
///
/// let mut generate = Generate::default();
/// generate
///     .types
///     .insert("geometry".into(), "geo_types::Geometry<f64>".into());
///
/// let mapped = map(&PgType::Scalar("geometry".into()), &generate);
/// assert_eq!(mapped.text, "geo_types::Geometry<f64>");
/// assert!(mapped.unmapped.is_none());
/// ```
pub fn map(ty: &PgType, generate: &Generate) -> Mapped {
    match ty {
        PgType::Array(inner) => {
            let inner = map(inner, generate);
            Mapped {
                text: format!("Vec<{}>", inner.text),
                // A Vec is not Copy however Copy its elements are.
                copy: false,
                ..inner
            }
        }
        PgType::Enum { name, .. } => override_for(name, &generate.types).unwrap_or_else(|| {
            Mapped {
                // The enum is generated, so whether it is Copy is
                // whatever the configured derives say.
                copy: generate.enum_derives.iter().any(|d| d == "Copy"),
                ..Mapped::borrowed(naming::pascal_case(name), &[])
            }
        }),
        PgType::Scalar(name) => override_for(name, &generate.types).unwrap_or_else(|| scalar(name)),
    }
}

fn override_for(name: &str, overrides: &HashMap<String, String>) -> Option<Mapped> {
    // Both `text` and `_text` spellings are accepted as override keys; the
    // element name is what reaches here for arrays.
    let path = overrides
        .get(name)
        .or_else(|| overrides.get(name.trim_start_matches('_')))?;
    Some(from_path(path))
}

/// Turn a configured path into a short name plus its import. A value with
/// generics or no `::` is emitted verbatim and imports nothing.
///
/// An override is assumed not to be `Copy`: proto cannot see the type, and
/// a needless borrow is a lint where a wrong move is a build failure.
fn from_path(path: &str) -> Mapped {
    if path.contains('<') || !path.contains("::") {
        return Mapped::borrowed(path, &[]);
    }
    let short = path.rsplit("::").next().unwrap_or(path);
    Mapped::borrowed(short, &[path])
}

fn scalar(name: &str) -> Mapped {
    match name {
        "bool" => Mapped::copied("bool", &[]),
        "char" => Mapped::copied("i8", &[]),
        "int2" => Mapped::copied("i16", &[]),
        "int4" => Mapped::copied("i32", &[]),
        "int8" => Mapped::copied("i64", &[]),
        "oid" => Mapped::copied("Oid", &["sqlx::postgres::types::Oid"]),
        "float4" => Mapped::copied("f32", &[]),
        "float8" => Mapped::copied("f64", &[]),
        "numeric" => Mapped::copied("Decimal", &["rust_decimal::Decimal"]),
        "money" => Mapped::copied("PgMoney", &["sqlx::postgres::types::PgMoney"]),

        "text" | "varchar" | "bpchar" | "name" | "citext" | "xml" | "ltree" | "unknown" => {
            Mapped::borrowed("String", &[])
        }

        "uuid" => Mapped::copied("Uuid", &["uuid::Uuid"]),
        "bytea" => Mapped::borrowed("Vec<u8>", &[]),
        "json" | "jsonb" => Mapped::borrowed("Value", &["serde_json::Value"]),

        "timestamptz" => Mapped::copied("DateTime<Utc>", &["chrono::DateTime", "chrono::Utc"]),
        "timestamp" => Mapped::copied("NaiveDateTime", &["chrono::NaiveDateTime"]),
        "date" => Mapped::copied("NaiveDate", &["chrono::NaiveDate"]),
        "time" => Mapped::copied("NaiveTime", &["chrono::NaiveTime"]),
        "timetz" => Mapped::copied("PgTimeTz", &["sqlx::postgres::types::PgTimeTz"]),
        "interval" => Mapped::copied("PgInterval", &["sqlx::postgres::types::PgInterval"]),

        "inet" | "cidr" => Mapped::copied("IpAddr", &["std::net::IpAddr"]),

        "int4range" => Mapped::copied("PgRange<i32>", &["sqlx::postgres::types::PgRange"]),
        "int8range" => Mapped::copied("PgRange<i64>", &["sqlx::postgres::types::PgRange"]),
        "numrange" => Mapped::copied(
            "PgRange<Decimal>",
            &["sqlx::postgres::types::PgRange", "rust_decimal::Decimal"],
        ),
        "daterange" => Mapped::copied(
            "PgRange<NaiveDate>",
            &["sqlx::postgres::types::PgRange", "chrono::NaiveDate"],
        ),
        "tsrange" => Mapped::copied(
            "PgRange<NaiveDateTime>",
            &["sqlx::postgres::types::PgRange", "chrono::NaiveDateTime"],
        ),
        "tstzrange" => Mapped::copied(
            "PgRange<DateTime<Utc>>",
            &[
                "sqlx::postgres::types::PgRange",
                "chrono::DateTime",
                "chrono::Utc",
            ],
        ),

        other => Mapped {
            unmapped: Some(other.to_string()),
            ..Mapped::borrowed("String", &[])
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Generate {
        Generate::default()
    }

    #[test]
    fn arrays_wrap_their_element() {
        let ty = PgType::Array(Box::new(PgType::Scalar("text".into())));
        assert_eq!(map(&ty, &plain()).text, "Vec<String>");
    }

    #[test]
    fn enum_columns_use_the_generated_name() {
        let ty = PgType::Enum {
            schema: "shop".into(),
            name: "product_status".into(),
        };
        assert_eq!(map(&ty, &plain()).text, "ProductStatus");
    }

    #[test]
    fn unknown_types_are_flagged() {
        let mapped = map(&PgType::Scalar("tsvector".into()), &plain());
        assert_eq!(mapped.unmapped.as_deref(), Some("tsvector"));
    }

    #[test]
    fn overrides_win_and_import() {
        let mut generate = plain();
        generate
            .types
            .insert("tsvector".into(), "my_crate::TsVector".into());
        let mapped = map(&PgType::Scalar("tsvector".into()), &generate);
        assert_eq!(mapped.text, "TsVector");
        assert_eq!(mapped.imports, vec!["my_crate::TsVector".to_string()]);
        assert!(mapped.unmapped.is_none());
    }

    #[test]
    fn timestamptz_pulls_chrono() {
        let mapped = map(&PgType::Scalar("timestamptz".into()), &plain());
        assert_eq!(mapped.text, "DateTime<Utc>");
        assert_eq!(mapped.imports.len(), 2);
    }
}
