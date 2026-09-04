//! Reads the Postgres catalogs. Everything proto knows about a table comes
//! from here: columns in ordinal order, nullability, comments, the primary
//! key, and the enum types the columns reference.

use sqlx::{PgPool, Row};

use crate::error::{Error, Result};

// ── Shapes ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    Table,
    Partitioned,
    View,
    MaterializedView,
    Foreign,
}

impl RelKind {
    fn parse(c: &str) -> Option<Self> {
        match c {
            "r" => Some(Self::Table),
            "p" => Some(Self::Partitioned),
            "v" => Some(Self::View),
            "m" => Some(Self::MaterializedView),
            "f" => Some(Self::Foreign),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Partitioned => "partitioned table",
            Self::View => "view",
            Self::MaterializedView => "materialized view",
            Self::Foreign => "foreign table",
        }
    }

    /// Views report every column as nullable; worth saying so in the header.
    pub fn nullability_is_known(&self) -> bool {
        matches!(self, Self::Table | Self::Partitioned | Self::Foreign)
    }
}

#[derive(Debug, Clone)]
pub enum PgType {
    /// A base type, by its `pg_type.typname` (`int4`, `timestamptz`, ...).
    Scalar(String),
    /// A user-defined enum type, which becomes a generated Rust enum.
    Enum {
        schema: String,
        name: String,
    },
    Array(Box<PgType>),
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: PgType,
    pub not_null: bool,
    pub comment: Option<String>,
    /// True for identity columns and columns with a default — the mapper
    /// phase needs this to decide what belongs in a Create type.
    pub has_default: bool,
    pub generated: bool,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    pub comment: Option<String>,
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PgEnum {
    pub schema: String,
    pub name: String,
    pub labels: Vec<String>,
}

/// A table and every enum type it references, which is exactly what one
/// generated module needs.
#[derive(Debug, Clone)]
pub struct Model {
    pub table: Table,
    pub enums: Vec<PgEnum>,
}

// ── Queries ─────────────────────────────────────────────────────────────────

const COLUMNS_SQL: &str = "\
SELECT a.attname::text                        AS name,
       a.attnotnull                           AS not_null,
       t.typname::text                        AS type_name,
       t.typtype::text                        AS type_kind,
       tn.nspname::text                       AS type_schema,
       et.typname::text                       AS elem_name,
       et.typtype::text                       AS elem_kind,
       en.nspname::text                       AS elem_schema,
       bt.typname::text                       AS base_name,
       bt.typtype::text                       AS base_kind,
       bn.nspname::text                       AS base_schema,
       (a.atthasdef OR a.attidentity <> '')   AS has_default,
       (a.attgenerated <> '')                 AS generated,
       col_description(c.oid, a.attnum)       AS comment
  FROM pg_attribute a
  JOIN pg_class c           ON c.oid = a.attrelid
  JOIN pg_namespace cn      ON cn.oid = c.relnamespace
  JOIN pg_type t            ON t.oid = a.atttypid
  JOIN pg_namespace tn      ON tn.oid = t.typnamespace
  LEFT JOIN pg_type et      ON et.oid = NULLIF(t.typelem, 0) AND t.typcategory = 'A'
  LEFT JOIN pg_namespace en ON en.oid = et.typnamespace
  LEFT JOIN pg_type bt      ON bt.oid = NULLIF(t.typbasetype, 0)
  LEFT JOIN pg_namespace bn ON bn.oid = bt.typnamespace
 WHERE cn.nspname = $1
   AND c.relname = $2
   AND a.attnum > 0
   AND NOT a.attisdropped
 ORDER BY a.attnum";

const RELATION_SQL: &str = "\
SELECT c.relkind::text AS kind, obj_description(c.oid) AS comment
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = $1
   AND c.relname = $2
   AND c.relkind IN ('r', 'p', 'v', 'm', 'f')";

const PRIMARY_KEY_SQL: &str = "\
SELECT a.attname::text AS name
  FROM pg_index i
  JOIN pg_class c      ON c.oid = i.indrelid
  JOIN pg_namespace n  ON n.oid = c.relnamespace
  JOIN pg_attribute a  ON a.attrelid = c.oid AND a.attnum = ANY(i.indkey)
 WHERE n.nspname = $1
   AND c.relname = $2
   AND i.indisprimary
 ORDER BY array_position(i.indkey, a.attnum)";

const ENUM_SQL: &str = "\
SELECT e.enumlabel::text AS label
  FROM pg_enum e
  JOIN pg_type t      ON t.oid = e.enumtypid
  JOIN pg_namespace n ON n.oid = t.typnamespace
 WHERE n.nspname = $1
   AND t.typname = $2
 ORDER BY e.enumsortorder";

const TABLES_SQL: &str = "\
SELECT c.relname::text AS name
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = $1
   AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
 ORDER BY c.relname";

const SCHEMAS_SQL: &str = "\
SELECT n.nspname::text AS name
  FROM pg_namespace n
 WHERE n.nspname NOT LIKE 'pg\\_%'
   AND n.nspname <> 'information_schema'
   AND EXISTS (
       SELECT 1 FROM pg_class c
        WHERE c.relnamespace = n.oid
          AND c.relkind IN ('r', 'p', 'v', 'm', 'f'))
 ORDER BY n.nspname";

// ── Reads ───────────────────────────────────────────────────────────────────

pub async fn schemas(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query(SCHEMAS_SQL).fetch_all(pool).await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
}

pub async fn tables(pool: &PgPool, schema: &str) -> Result<Vec<String>> {
    let rows = sqlx::query(TABLES_SQL).bind(schema).fetch_all(pool).await?;
    if rows.is_empty() && !schema_exists(pool, schema).await? {
        return Err(Error::NoSuchSchema(schema.to_string()));
    }
    Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
}

async fn schema_exists(pool: &PgPool, schema: &str) -> Result<bool> {
    let row = sqlx::query("SELECT 1 AS ok FROM pg_namespace WHERE nspname = $1")
        .bind(schema)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// Read one table plus the enum types its columns use.
pub async fn model(pool: &PgPool, schema: &str, table: &str) -> Result<Model> {
    let relation = sqlx::query(RELATION_SQL)
        .bind(schema)
        .bind(table)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NoSuchTable {
            schema: schema.to_string(),
            table: table.to_string(),
        })?;

    let kind =
        RelKind::parse(&relation.get::<String, _>("kind")).ok_or_else(|| Error::NoSuchTable {
            schema: schema.to_string(),
            table: table.to_string(),
        })?;

    let rows = sqlx::query(COLUMNS_SQL)
        .bind(schema)
        .bind(table)
        .fetch_all(pool)
        .await?;

    let mut columns = Vec::with_capacity(rows.len());
    for row in &rows {
        columns.push(Column {
            name: row.get("name"),
            ty: column_type(row),
            not_null: row.get("not_null"),
            comment: row.get("comment"),
            has_default: row.get("has_default"),
            generated: row.get("generated"),
        });
    }

    let pk = sqlx::query(PRIMARY_KEY_SQL)
        .bind(schema)
        .bind(table)
        .fetch_all(pool)
        .await?;

    let table = Table {
        schema: schema.to_string(),
        name: table.to_string(),
        kind,
        comment: relation.get("comment"),
        columns,
        primary_key: pk.iter().map(|r| r.get::<String, _>("name")).collect(),
    };

    let enums = read_enums(pool, &table).await?;
    Ok(Model { table, enums })
}

/// Resolve one column's type: unwrap the domain, unwrap the array, and note
/// whether what is left is an enum.
fn column_type(row: &sqlx::postgres::PgRow) -> PgType {
    let kind: String = row.get("type_kind");
    let elem_name: Option<String> = row.get("elem_name");

    // Arrays first: `_text` carries its element in typelem.
    if let Some(elem_name) = elem_name {
        let elem_kind: Option<String> = row.get("elem_kind");
        let elem_schema: Option<String> = row.get("elem_schema");
        let inner = if elem_kind.as_deref() == Some("e") {
            PgType::Enum {
                schema: elem_schema.unwrap_or_else(|| "public".to_string()),
                name: elem_name,
            }
        } else {
            PgType::Scalar(elem_name)
        };
        return PgType::Array(Box::new(inner));
    }

    // A domain stands in for its base type.
    if kind == "d"
        && let Some(base_name) = row.get::<Option<String>, _>("base_name")
    {
        let base_kind: Option<String> = row.get("base_kind");
        if base_kind.as_deref() == Some("e") {
            return PgType::Enum {
                schema: row
                    .get::<Option<String>, _>("base_schema")
                    .unwrap_or_else(|| "public".to_string()),
                name: base_name,
            };
        }
        return PgType::Scalar(base_name);
    }

    if kind == "e" {
        return PgType::Enum {
            schema: row.get("type_schema"),
            name: row.get("type_name"),
        };
    }

    PgType::Scalar(row.get("type_name"))
}

async fn read_enums(pool: &PgPool, table: &Table) -> Result<Vec<PgEnum>> {
    let mut wanted: Vec<(String, String)> = Vec::new();
    for column in &table.columns {
        if let Some((schema, name)) = enum_ref(&column.ty) {
            let key = (schema.to_string(), name.to_string());
            if !wanted.contains(&key) {
                wanted.push(key);
            }
        }
    }

    let mut enums = Vec::with_capacity(wanted.len());
    for (schema, name) in wanted {
        let rows = sqlx::query(ENUM_SQL)
            .bind(&schema)
            .bind(&name)
            .fetch_all(pool)
            .await?;
        enums.push(PgEnum {
            schema,
            name,
            labels: rows.iter().map(|r| r.get::<String, _>("label")).collect(),
        });
    }
    Ok(enums)
}

fn enum_ref(ty: &PgType) -> Option<(&str, &str)> {
    match ty {
        PgType::Enum { schema, name } => Some((schema, name)),
        PgType::Array(inner) => enum_ref(inner),
        PgType::Scalar(_) => None,
    }
}
