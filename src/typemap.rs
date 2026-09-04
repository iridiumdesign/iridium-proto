//! Postgres type -> Rust type.
//!
//! The built-in map covers what sqlx can decode out of the box. Anything
//! else comes back as `String` with a TODO and a warning on stderr; the
//! `[generate.types]` table in the config is the fix, and it also overrides
//! the built-ins when a project wants a different backend.

use std::collections::HashMap;

use crate::introspect::PgType;
use crate::naming;

#[derive(Debug, Clone)]
pub struct Mapped {
    /// The Rust type as written in the struct, using short names.
    pub text: String,
    /// Full paths to `use`, e.g. `chrono::DateTime`.
    pub imports: Vec<String>,
    /// The Postgres type name proto did not recognise, if any.
    pub unmapped: Option<String>,
}

impl Mapped {
    fn new(text: impl Into<String>, imports: &[&str]) -> Self {
        Self {
            text: text.into(),
            imports: imports.iter().map(|s| (*s).to_string()).collect(),
            unmapped: None,
        }
    }
}

/// Map a column type, wrapping arrays in `Vec` and applying overrides.
pub fn map(ty: &PgType, overrides: &HashMap<String, String>) -> Mapped {
    match ty {
        PgType::Array(inner) => {
            let mut mapped = map(inner, overrides);
            mapped.text = format!("Vec<{}>", mapped.text);
            mapped
        }
        PgType::Enum { name, .. } => {
            if let Some(mapped) = override_for(name, overrides) {
                return mapped;
            }
            Mapped::new(naming::pascal_case(name), &[])
        }
        PgType::Scalar(name) => {
            if let Some(mapped) = override_for(name, overrides) {
                return mapped;
            }
            scalar(name)
        }
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
fn from_path(path: &str) -> Mapped {
    if path.contains('<') || !path.contains("::") {
        return Mapped::new(path, &[]);
    }
    let short = path.rsplit("::").next().unwrap_or(path);
    Mapped {
        text: short.to_string(),
        imports: vec![path.to_string()],
        unmapped: None,
    }
}

fn scalar(name: &str) -> Mapped {
    match name {
        "bool" => Mapped::new("bool", &[]),
        "char" => Mapped::new("i8", &[]),
        "int2" => Mapped::new("i16", &[]),
        "int4" => Mapped::new("i32", &[]),
        "int8" => Mapped::new("i64", &[]),
        "oid" => Mapped::new("Oid", &["sqlx::postgres::types::Oid"]),
        "float4" => Mapped::new("f32", &[]),
        "float8" => Mapped::new("f64", &[]),
        "numeric" => Mapped::new("Decimal", &["rust_decimal::Decimal"]),
        "money" => Mapped::new("PgMoney", &["sqlx::postgres::types::PgMoney"]),

        "text" | "varchar" | "bpchar" | "name" | "citext" | "xml" | "ltree" | "unknown" => {
            Mapped::new("String", &[])
        }

        "uuid" => Mapped::new("Uuid", &["uuid::Uuid"]),
        "bytea" => Mapped::new("Vec<u8>", &[]),
        "json" | "jsonb" => Mapped::new("Value", &["serde_json::Value"]),

        "timestamptz" => Mapped::new("DateTime<Utc>", &["chrono::DateTime", "chrono::Utc"]),
        "timestamp" => Mapped::new("NaiveDateTime", &["chrono::NaiveDateTime"]),
        "date" => Mapped::new("NaiveDate", &["chrono::NaiveDate"]),
        "time" => Mapped::new("NaiveTime", &["chrono::NaiveTime"]),
        "timetz" => Mapped::new("PgTimeTz", &["sqlx::postgres::types::PgTimeTz"]),
        "interval" => Mapped::new("PgInterval", &["sqlx::postgres::types::PgInterval"]),

        "inet" | "cidr" => Mapped::new("IpAddr", &["std::net::IpAddr"]),

        "int4range" => Mapped::new("PgRange<i32>", &["sqlx::postgres::types::PgRange"]),
        "int8range" => Mapped::new("PgRange<i64>", &["sqlx::postgres::types::PgRange"]),
        "numrange" => Mapped::new(
            "PgRange<Decimal>",
            &["sqlx::postgres::types::PgRange", "rust_decimal::Decimal"],
        ),
        "daterange" => Mapped::new(
            "PgRange<NaiveDate>",
            &["sqlx::postgres::types::PgRange", "chrono::NaiveDate"],
        ),
        "tsrange" => Mapped::new(
            "PgRange<NaiveDateTime>",
            &["sqlx::postgres::types::PgRange", "chrono::NaiveDateTime"],
        ),
        "tstzrange" => Mapped::new(
            "PgRange<DateTime<Utc>>",
            &[
                "sqlx::postgres::types::PgRange",
                "chrono::DateTime",
                "chrono::Utc",
            ],
        ),

        other => Mapped {
            text: "String".to_string(),
            imports: Vec::new(),
            unmapped: Some(other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_overrides() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn arrays_wrap_their_element() {
        let ty = PgType::Array(Box::new(PgType::Scalar("text".into())));
        assert_eq!(map(&ty, &no_overrides()).text, "Vec<String>");
    }

    #[test]
    fn enum_columns_use_the_generated_name() {
        let ty = PgType::Enum {
            schema: "arboreal".into(),
            name: "tree_status".into(),
        };
        assert_eq!(map(&ty, &no_overrides()).text, "TreeStatus");
    }

    #[test]
    fn unknown_types_are_flagged() {
        let mapped = map(&PgType::Scalar("tsvector".into()), &no_overrides());
        assert_eq!(mapped.unmapped.as_deref(), Some("tsvector"));
    }

    #[test]
    fn overrides_win_and_import() {
        let mut overrides = no_overrides();
        overrides.insert("tsvector".into(), "my_crate::TsVector".into());
        let mapped = map(&PgType::Scalar("tsvector".into()), &overrides);
        assert_eq!(mapped.text, "TsVector");
        assert_eq!(mapped.imports, vec!["my_crate::TsVector".to_string()]);
        assert!(mapped.unmapped.is_none());
    }

    #[test]
    fn timestamptz_pulls_chrono() {
        let mapped = map(&PgType::Scalar("timestamptz".into()), &no_overrides());
        assert_eq!(mapped.text, "DateTime<Utc>");
        assert_eq!(mapped.imports.len(), 2);
    }
}
