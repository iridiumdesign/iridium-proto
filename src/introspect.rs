//! Reads the Postgres catalogs. Everything proto knows about a table comes
//! from here: columns in ordinal order, nullability, comments, the primary
//! key, and the enum types the columns reference.

use sqlx::{PgPool, Row};

use crate::error::{Error, Result};

// ── Shapes ──────────────────────────────────────────────────────────────────

/// What kind of relation a name refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    /// An ordinary table.
    Table,
    /// A partitioned table.
    Partitioned,
    /// A view.
    View,
    /// A materialized view.
    MaterializedView,
    /// A foreign table.
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

    /// The name to use in prose, e.g. `materialized view`.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Partitioned => "partitioned table",
            Self::View => "view",
            Self::MaterializedView => "materialized view",
            Self::Foreign => "foreign table",
        }
    }

    /// Whether `NOT NULL` means anything here. A view reports every
    /// column as nullable no matter what feeds it, which is worth saying
    /// in the generated header.
    pub const fn nullability_is_known(&self) -> bool {
        matches!(self, Self::Table | Self::Partitioned | Self::Foreign)
    }
}

/// A column's type, resolved far enough to pick a Rust type for it.
#[derive(Debug, Clone)]
pub enum PgType {
    /// A base type, by its `pg_type.typname` (`int4`, `timestamptz`, ...).
    Scalar(String),
    /// A user-defined enum type, which becomes a generated Rust enum.
    Enum {
        /// Schema the type lives in.
        schema: String,
        /// The type's name, which becomes the Rust enum's name.
        name: String,
    },
    /// An array of the type inside.
    Array(Box<PgType>),
}

/// One column, as the catalogs describe it.
#[derive(Debug, Clone)]
pub struct Column {
    /// The column name, exactly as Postgres spells it.
    pub name: String,
    /// The resolved type: domains unwrapped, arrays and enums named.
    pub ty: PgType,
    /// The type as Postgres spells it, e.g. `character varying(64)`. Used
    /// for the parameter list of a generated function.
    pub sql_type: String,
    /// Declared `NOT NULL`. Always false on a view.
    pub not_null: bool,
    /// `COMMENT ON COLUMN`, which becomes a doc comment.
    pub comment: Option<String>,
    /// Has a default of any kind, identity included.
    pub has_default: bool,
    /// The default as SQL, e.g. `gen_random_uuid()` or `'draft'::text`.
    pub default_expr: Option<String>,
    /// An identity column.
    pub identity: bool,
    /// A `GENERATED ALWAYS AS` column.
    pub generated: bool,
}

impl Column {
    /// A column the database fills in on its own: generated, identity, or
    /// defaulted to a function call (`gen_random_uuid()`, `now()`,
    /// `nextval(...)`). These stay out of both insert and update — the
    /// server owns them.
    pub fn server_owned(&self) -> bool {
        self.generated
            || self.identity
            || self
                .default_expr
                .as_deref()
                .is_some_and(|d| d.contains('('))
    }

    /// A default that is a plain literal, e.g. `'draft'::text`. The column
    /// is settable, and omitting it falls back to this expression.
    pub fn literal_default(&self) -> Option<&str> {
        match &self.default_expr {
            Some(d) if !d.contains('(') && !self.generated && !self.identity => Some(d),
            _ => None,
        }
    }
}

/// One relation and everything about it that shapes generated code.
#[derive(Debug, Clone)]
pub struct Table {
    /// Schema the relation lives in.
    pub schema: String,
    /// The relation's own name.
    pub name: String,
    /// Table, view, or one of the rest.
    pub kind: RelKind,
    /// `COMMENT ON TABLE`, which becomes a doc comment.
    pub comment: Option<String>,
    /// Columns in ordinal order, dropped ones excluded.
    pub columns: Vec<Column>,
    /// Primary key columns, in index order. Empty when there is none.
    pub primary_key: Vec<String>,
    /// Unique constraints and unique indexes, excluding the primary key and
    /// anything partial or expression-based.
    pub unique_keys: Vec<Vec<String>>,
    /// Foreign key constraints, in constraint order.
    pub foreign_keys: Vec<ForeignKey>,
}

/// A foreign key constraint. Single-column ones become finders.
#[derive(Debug, Clone)]
pub struct ForeignKey {
    /// The referencing columns, in constraint order.
    pub columns: Vec<String>,
    /// Schema of the referenced relation.
    pub ref_schema: String,
    /// The referenced relation.
    pub ref_table: String,
}

impl Table {
    /// Rows can be written back. Views and materialized views cannot,
    /// without rules or triggers proto cannot see.
    pub const fn writable(&self) -> bool {
        matches!(
            self.kind,
            RelKind::Table | RelKind::Partitioned | RelKind::Foreign
        )
    }

    /// Find a column by name.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Columns an insert supplies. Server-owned columns are left out so the
    /// database fills them.
    pub fn insert_columns(&self) -> Vec<&Column> {
        self.columns.iter().filter(|c| !c.server_owned()).collect()
    }

    /// Columns an update writes: everything insertable except the key,
    /// which identifies the row rather than being written to it.
    pub fn update_columns(&self) -> Vec<&Column> {
        self.insert_columns()
            .into_iter()
            .filter(|c| !self.primary_key.contains(&c.name))
            .collect()
    }

    /// The primary key columns themselves, in index order.
    pub fn primary_key_columns(&self) -> Vec<&Column> {
        self.primary_key
            .iter()
            .filter_map(|name| self.column(name))
            .collect()
    }
}

/// A Postgres enum type and its labels.
#[derive(Debug, Clone)]
pub struct PgEnum {
    /// Schema the type lives in.
    pub schema: String,
    /// The type's name.
    pub name: String,
    /// Its labels, in sort order.
    pub labels: Vec<String>,
}

/// A table and every enum type it references, which is exactly what one
/// generated module needs.
/// A table and every enum type its columns use — exactly what one
/// generated module needs.
#[derive(Debug, Clone)]
pub struct Model {
    /// The relation itself.
    pub table: Table,
    /// The enum types its columns reference, deduplicated.
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
       format_type(a.atttypid, a.atttypmod)   AS sql_type,
       (a.atthasdef OR a.attidentity <> '')   AS has_default,
       pg_get_expr(d.adbin, d.adrelid)        AS default_expr,
       (a.attidentity <> '')                  AS identity,
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
  LEFT JOIN pg_attrdef d    ON d.adrelid = c.oid AND d.adnum = a.attnum
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

const UNIQUE_SQL: &str = "\
SELECT array_agg(a.attname::text ORDER BY k.ord) AS cols
  FROM pg_index i
  JOIN pg_class t      ON t.oid = i.indrelid
  JOIN pg_namespace n  ON n.oid = t.relnamespace
  JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) ON TRUE
  JOIN pg_attribute a  ON a.attrelid = t.oid AND a.attnum = k.attnum
 WHERE n.nspname = $1
   AND t.relname = $2
   AND i.indisunique
   AND NOT i.indisprimary
   AND i.indpred IS NULL
   AND i.indexprs IS NULL
 GROUP BY i.indexrelid
 ORDER BY i.indexrelid";

const FOREIGN_KEY_SQL: &str = "\
SELECT array_agg(a.attname::text ORDER BY k.ord) AS cols,
       rn.nspname::text AS ref_schema,
       rt.relname::text AS ref_table
  FROM pg_constraint c
  JOIN pg_class t      ON t.oid = c.conrelid
  JOIN pg_namespace n  ON n.oid = t.relnamespace
  JOIN pg_class rt     ON rt.oid = c.confrelid
  JOIN pg_namespace rn ON rn.oid = rt.relnamespace
  JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON TRUE
  JOIN pg_attribute a  ON a.attrelid = t.oid AND a.attnum = k.attnum
 WHERE n.nspname = $1
   AND t.relname = $2
   AND c.contype = 'f'
 GROUP BY c.oid, rn.nspname, rt.relname
 ORDER BY c.oid";

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

/// Every schema holding at least one relation, system schemas aside.
///
/// # Errors
///
/// Fails if the catalog query does.
pub async fn schemas(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query(SCHEMAS_SQL).fetch_all(pool).await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
}

/// Every relation in a schema: tables, views, materialized views,
/// partitioned and foreign tables.
///
/// # Errors
///
/// [`Error::NoSuchSchema`] if the
/// schema does not exist. An existing but empty schema returns no rows.
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

/// Read one relation: its columns, comments, key, unique keys, foreign
/// keys, and the enum types its columns use.
///
/// # Errors
///
/// [`Error::NoSuchTable`] if nothing of
/// that name exists in the schema.
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
            sql_type: row.get("sql_type"),
            not_null: row.get("not_null"),
            comment: row.get("comment"),
            has_default: row.get("has_default"),
            default_expr: row.get("default_expr"),
            identity: row.get("identity"),
            generated: row.get("generated"),
        });
    }

    let pk = sqlx::query(PRIMARY_KEY_SQL)
        .bind(schema)
        .bind(table)
        .fetch_all(pool)
        .await?;

    let uniques = sqlx::query(UNIQUE_SQL)
        .bind(schema)
        .bind(table)
        .fetch_all(pool)
        .await?;

    let fks = sqlx::query(FOREIGN_KEY_SQL)
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
        unique_keys: uniques
            .iter()
            .map(|r| r.get::<Vec<String>, _>("cols"))
            .collect(),
        foreign_keys: fks
            .iter()
            .map(|r| ForeignKey {
                columns: r.get("cols"),
                ref_schema: r.get("ref_schema"),
                ref_table: r.get("ref_table"),
            })
            .collect(),
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
