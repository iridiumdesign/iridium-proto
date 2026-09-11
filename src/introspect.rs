//! Reads the Postgres catalogs. Everything proto knows about a table comes
//! from here: columns in ordinal order, nullability, comments, the primary
//! key, and the enum and composite types the columns reference.

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
    /// A user-defined composite type — `CREATE TYPE ... AS (...)` — which
    /// becomes a generated Rust struct. A table's own row type is not
    /// one of these; a column typed by one stays a [`PgType::Scalar`].
    /// proto does not know it, and says so.
    Composite {
        /// Schema the type lives in.
        schema: String,
        /// The type's name, which becomes the Rust struct's name.
        name: String,
    },
    /// An array of the type inside.
    Array(Box<PgType>),
}

impl PgType {
    /// The user-defined type this names, looking through arrays: an
    /// enum or a composite, as `(schema, name)`.
    fn user_type(&self) -> Option<(&str, &str)> {
        match self {
            PgType::Enum { schema, name } | PgType::Composite { schema, name } => {
                Some((schema, name))
            }
            PgType::Array(inner) => inner.user_type(),
            PgType::Scalar(_) => None,
        }
    }
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
    /// The extension that defines the column's type, if one does —
    /// `postgis` for a `geometry`. Looked through arrays and domains, so
    /// it names the extension behind whatever proto would have to map.
    pub extension: Option<String>,
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
    /// Tables whose single-column foreign keys point here, in constraint
    /// order. The parent's side of the relationship.
    pub children: Vec<Child>,
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

/// A table that refers to this one through a single-column foreign key.
/// One-to-one links — where the referring column is itself unique — are
/// not here: they are not a collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    /// Schema of the referring table.
    pub schema: String,
    /// The referring table.
    pub table: String,
    /// Its foreign key column.
    pub column: String,
    /// The column here that it refers to — the primary key, nearly always.
    pub ref_column: String,
    /// The child's own primary key, for naming the finder the parent's
    /// loader calls; see [`crate::render::plan::finder_call`].
    pub primary_key: Vec<String>,
    /// The child's unique keys, likewise.
    pub unique_keys: Vec<Vec<String>>,
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

/// A Postgres composite type and its attributes.
#[derive(Debug, Clone)]
pub struct PgComposite {
    /// Schema the type lives in.
    pub schema: String,
    /// The type's name.
    pub name: String,
    /// `COMMENT ON TYPE`, which becomes a doc comment.
    pub comment: Option<String>,
    /// Its attributes, in declaration order. The catalog describes them
    /// exactly as it describes a table's columns, so they are read the
    /// same way; `not_null` is always false, since an attribute cannot
    /// be declared `NOT NULL`.
    pub fields: Vec<Column>,
}

/// A table and every user-defined type its columns use — exactly what one
/// generated module needs.
#[derive(Debug, Clone)]
pub struct Model {
    /// The relation itself.
    pub table: Table,
    /// The enum types its columns reference, deduplicated. A type named
    /// only inside a composite's attributes is here too.
    pub enums: Vec<PgEnum>,
    /// The composite types its columns reference, deduplicated, with a
    /// composite nested inside another listed before the one that holds
    /// it.
    pub composites: Vec<PgComposite>,
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
       ebt.typname::text                      AS elem_base_name,
       ebt.typtype::text                      AS elem_base_kind,
       ebn.nspname::text                      AS elem_base_schema,
       tr.relkind::text                       AS type_relkind,
       er.relkind::text                       AS elem_relkind,
       br.relkind::text                       AS base_relkind,
       ebr.relkind::text                      AS elem_base_relkind,
       tx.extname::text                       AS type_extension,
       ex.extname::text                       AS elem_extension,
       bx.extname::text                       AS base_extension,
       ebx.extname::text                      AS elem_base_extension,
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
  LEFT JOIN pg_type ebt     ON ebt.oid = NULLIF(et.typbasetype, 0)
  LEFT JOIN pg_namespace ebn ON ebn.oid = ebt.typnamespace
  LEFT JOIN pg_class tr     ON tr.oid = NULLIF(t.typrelid, 0)
  LEFT JOIN pg_class er     ON er.oid = NULLIF(et.typrelid, 0)
  LEFT JOIN pg_class br     ON br.oid = NULLIF(bt.typrelid, 0)
  LEFT JOIN pg_class ebr    ON ebr.oid = NULLIF(ebt.typrelid, 0)
  LEFT JOIN LATERAL (
       SELECT x.extname
         FROM pg_depend dp
         JOIN pg_extension x ON x.oid = dp.refobjid
        WHERE dp.classid = 'pg_type'::regclass
          AND dp.objid = t.oid
          AND dp.refclassid = 'pg_extension'::regclass
          AND dp.deptype = 'e'
        LIMIT 1) tx ON TRUE
  LEFT JOIN LATERAL (
       SELECT x.extname
         FROM pg_depend dp
         JOIN pg_extension x ON x.oid = dp.refobjid
        WHERE dp.classid = 'pg_type'::regclass
          AND dp.objid = et.oid
          AND dp.refclassid = 'pg_extension'::regclass
          AND dp.deptype = 'e'
        LIMIT 1) ex ON TRUE
  LEFT JOIN LATERAL (
       SELECT x.extname
         FROM pg_depend dp
         JOIN pg_extension x ON x.oid = dp.refobjid
        WHERE dp.classid = 'pg_type'::regclass
          AND dp.objid = bt.oid
          AND dp.refclassid = 'pg_extension'::regclass
          AND dp.deptype = 'e'
        LIMIT 1) bx ON TRUE
  LEFT JOIN LATERAL (
       SELECT x.extname
         FROM pg_depend dp
         JOIN pg_extension x ON x.oid = dp.refobjid
        WHERE dp.classid = 'pg_type'::regclass
          AND dp.objid = ebt.oid
          AND dp.refclassid = 'pg_extension'::regclass
          AND dp.deptype = 'e'
        LIMIT 1) ebx ON TRUE
  LEFT JOIN pg_attrdef d    ON d.adrelid = c.oid AND d.adnum = a.attnum
 WHERE cn.nspname = $1
   AND c.relname = $2
   AND a.attnum > 0
   AND NOT a.attisdropped
 ORDER BY a.attnum";

const COMPOSITE_SQL: &str = "\
SELECT obj_description(t.oid, 'pg_type') AS comment
  FROM pg_type t
  JOIN pg_namespace n ON n.oid = t.typnamespace
  JOIN pg_class c     ON c.oid = t.typrelid
 WHERE n.nspname = $1
   AND t.typname = $2
   AND t.typtype = 'c'
   AND c.relkind = 'c'";

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

// The other side of FOREIGN_KEY_SQL: every single-column foreign key
// that points at this table. A referring column that is itself unique
// makes a one-to-one, which is not a collection, so it is left out. Only
// the key part of an index decides that: INCLUDE columns follow the key
// in indkey and do not make it any less unique on the column.
const CHILDREN_SQL: &str = "\
SELECT n.nspname::text  AS schema,
       t.relname::text  AS table,
       a.attname::text  AS column,
       ra.attname::text AS ref_column
  FROM pg_constraint c
  JOIN pg_class t       ON t.oid = c.conrelid
  JOIN pg_namespace n   ON n.oid = t.relnamespace
  JOIN pg_class rt      ON rt.oid = c.confrelid
  JOIN pg_namespace rn  ON rn.oid = rt.relnamespace
  JOIN pg_attribute a   ON a.attrelid = t.oid AND a.attnum = c.conkey[1]
  JOIN pg_attribute ra  ON ra.attrelid = rt.oid AND ra.attnum = c.confkey[1]
 WHERE rn.nspname = $1
   AND rt.relname = $2
   AND c.contype = 'f'
   AND array_length(c.conkey, 1) = 1
   AND NOT EXISTS (
       SELECT 1 FROM pg_index i
        WHERE i.indrelid = t.oid
          AND i.indisunique
          AND i.indpred IS NULL
          AND i.indexprs IS NULL
          AND i.indnkeyatts = 1
          AND i.indkey[0] = c.conkey[1])
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
/// keys, the tables whose foreign keys point at it, and the enum types
/// its columns use.
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

    let columns = read_columns(pool, schema, table).await?;

    let (primary_key, unique_keys) = read_keys(pool, schema, table).await?;

    let fks = sqlx::query(FOREIGN_KEY_SQL)
        .bind(schema)
        .bind(table)
        .fetch_all(pool)
        .await?;

    let mut children = Vec::new();
    for row in sqlx::query(CHILDREN_SQL)
        .bind(schema)
        .bind(table)
        .fetch_all(pool)
        .await?
    {
        let (child_schema, child_table): (String, String) = (row.get("schema"), row.get("table"));
        // The child's keys name the finder the parent's loader calls.
        let (primary_key, unique_keys) = read_keys(pool, &child_schema, &child_table).await?;
        children.push(Child {
            schema: child_schema,
            table: child_table,
            column: row.get("column"),
            ref_column: row.get("ref_column"),
            primary_key,
            unique_keys,
        });
    }

    let table = Table {
        schema: schema.to_string(),
        name: table.to_string(),
        kind,
        comment: relation.get("comment"),
        columns,
        primary_key,
        unique_keys,
        foreign_keys: fks
            .iter()
            .map(|r| ForeignKey {
                columns: r.get("cols"),
                ref_schema: r.get("ref_schema"),
                ref_table: r.get("ref_table"),
            })
            .collect(),
        children,
    };

    let composites = read_composites(pool, &table).await?;
    let enums = read_enums(pool, &table, &composites).await?;
    Ok(Model {
        table,
        enums,
        composites,
    })
}

/// The columns of a relation — or the attributes of a composite type,
/// which the catalog keeps in exactly the same place.
async fn read_columns(pool: &PgPool, schema: &str, relation: &str) -> Result<Vec<Column>> {
    let rows = sqlx::query(COLUMNS_SQL)
        .bind(schema)
        .bind(relation)
        .fetch_all(pool)
        .await?;

    let mut columns = Vec::with_capacity(rows.len());
    for row in &rows {
        let (ty, extension) = column_type(row);
        columns.push(Column {
            name: row.get("name"),
            ty,
            sql_type: row.get("sql_type"),
            not_null: row.get("not_null"),
            comment: row.get("comment"),
            has_default: row.get("has_default"),
            default_expr: row.get("default_expr"),
            identity: row.get("identity"),
            generated: row.get("generated"),
            extension,
        });
    }
    Ok(columns)
}

/// One type as the column query describes it, before proto has decided
/// what it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RawType {
    /// `pg_type.typname`.
    name: String,
    /// `pg_type.typtype`: `b`ase, `e`num, `c`omposite, `d`omain, ...
    kind: Option<String>,
    /// The type's schema.
    schema: Option<String>,
    /// For a composite, the `relkind` of the class behind it: `c` for a
    /// standalone type, `r` and the rest for a relation's row type.
    relkind: Option<String>,
    /// The extension the type belongs to, if any.
    extension: Option<String>,
}

/// What the column query says about a column's type: the type itself,
/// its element when it is an array, and the base behind whichever of
/// those is a domain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RawColumnType {
    ty: RawType,
    elem: Option<RawType>,
    elem_base: Option<RawType>,
    base: Option<RawType>,
}

/// Resolve one column's type: unwrap the domain, unwrap the array, and
/// note whether what is left is an enum or a composite. Alongside it, the
/// extension that owns whatever type proto will have to map.
fn column_type(row: &sqlx::postgres::PgRow) -> (PgType, Option<String>) {
    let raw = |prefix: &str| -> Option<RawType> {
        let name: Option<String> = row.get(format!("{prefix}_name").as_str());
        let field = |what: &str| row.get::<Option<String>, _>(format!("{prefix}_{what}").as_str());
        Some(RawType {
            name: name?,
            kind: field("kind"),
            schema: field("schema"),
            relkind: field("relkind"),
            extension: field("extension"),
        })
    };
    resolve_type(RawColumnType {
        ty: raw("type").expect("a column has a type"),
        elem: raw("elem"),
        elem_base: raw("elem_base"),
        base: raw("base"),
    })
}

/// The decision behind [`column_type`], on what the query returned.
fn resolve_type(raw: RawColumnType) -> (PgType, Option<String>) {
    // Arrays first: `_text` carries its element in typelem. An element
    // that is a domain stands in for its base, the way a domain column
    // does, and it is the base that an extension owns.
    if let Some(elem) = raw.elem {
        let elem = match (elem.kind.as_deref(), raw.elem_base) {
            (Some("d"), Some(base)) => base,
            _ => elem,
        };
        let extension = elem.extension.clone();
        return (PgType::Array(Box::new(user_type(elem))), extension);
    }

    // A domain stands in for its base type.
    if raw.ty.kind.as_deref() == Some("d")
        && let Some(base) = raw.base
    {
        let extension = base.extension.clone();
        return (user_type(base), extension);
    }

    let extension = raw.ty.extension.clone();
    (user_type(raw.ty), extension)
}

/// A relation's primary key and its unique keys, in index order.
async fn read_keys(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<(Vec<String>, Vec<Vec<String>>)> {
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
    Ok((
        pk.iter().map(|r| r.get::<String, _>("name")).collect(),
        uniques
            .iter()
            .map(|r| r.get::<Vec<String>, _>("cols"))
            .collect(),
    ))
}

/// An enum, a standalone composite, or — for everything else, a table's
/// row type included — the bare name.
fn user_type(raw: RawType) -> PgType {
    let schema = raw.schema.unwrap_or_else(|| "public".to_string());
    match (raw.kind.as_deref(), raw.relkind.as_deref()) {
        (Some("e"), _) => PgType::Enum {
            schema,
            name: raw.name,
        },
        (Some("c"), Some("c")) => PgType::Composite {
            schema,
            name: raw.name,
        },
        _ => PgType::Scalar(raw.name),
    }
}

/// Every composite type the table's columns use, and every composite
/// those use in turn, each read once. A nested type is listed before the
/// type that holds it.
async fn read_composites(pool: &PgPool, table: &Table) -> Result<Vec<PgComposite>> {
    let is_composite = |ty: &PgType| matches!(ty, PgType::Composite { .. });
    let mut wanted: Vec<(String, String)> = Vec::new();
    let mut composites: Vec<PgComposite> = Vec::new();
    collect(&table.columns, &mut wanted, is_composite);

    let mut at = 0;
    while at < wanted.len() {
        let (schema, name) = wanted[at].clone();
        at += 1;
        let comment = sqlx::query(COMPOSITE_SQL)
            .bind(&schema)
            .bind(&name)
            .fetch_optional(pool)
            .await?
            .and_then(|row| row.get::<Option<String>, _>("comment"));
        let fields = read_columns(pool, &schema, &name).await?;
        collect(&fields, &mut wanted, is_composite);
        composites.push(PgComposite {
            schema,
            name,
            comment,
            fields,
        });
    }

    Ok(dependencies_first(composites))
}

/// `composites` with every type ahead of the types that hold it, so the
/// generated definitions read dependencies first.
///
/// Discovery is breadth-first from the columns, and that is not the
/// same order: a type both named by a column and nested in a type named
/// later is found before its holder, so reversing the list would put
/// the holder first. A depth-first postorder over the fields gets it
/// right whatever the columns say. Postgres does not let a composite
/// contain itself, so there is no cycle to guard against; the visited
/// set is for a type reached by two routes.
fn dependencies_first(composites: Vec<PgComposite>) -> Vec<PgComposite> {
    fn visit(at: usize, composites: &[PgComposite], seen: &mut [bool], order: &mut Vec<usize>) {
        if seen[at] {
            return;
        }
        seen[at] = true;
        for field in &composites[at].fields {
            let ty = match &field.ty {
                PgType::Array(inner) => inner.as_ref(),
                other => other,
            };
            if let PgType::Composite { schema, name } = ty
                && let Some(dep) = composites
                    .iter()
                    .position(|c| &c.schema == schema && &c.name == name)
            {
                visit(dep, composites, seen, order);
            }
        }
        order.push(at);
    }

    let mut seen = vec![false; composites.len()];
    let mut order = Vec::with_capacity(composites.len());
    for at in 0..composites.len() {
        visit(at, &composites, &mut seen, &mut order);
    }
    let mut slots: Vec<Option<PgComposite>> = composites.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|at| slots[at].take().expect("each index is taken once"))
        .collect()
}

/// Add every user type among `columns` that `keep` accepts to `wanted`,
/// once each, in column order. Arrays are looked through.
fn collect(columns: &[Column], wanted: &mut Vec<(String, String)>, keep: impl Fn(&PgType) -> bool) {
    for column in columns {
        let ty = match &column.ty {
            PgType::Array(inner) => inner.as_ref(),
            other => other,
        };
        if !keep(ty) {
            continue;
        }
        if let Some((schema, name)) = ty.user_type() {
            let key = (schema.to_string(), name.to_string());
            if !wanted.contains(&key) {
                wanted.push(key);
            }
        }
    }
}

/// Every enum the table's columns use, and every enum a composite among
/// `composites` uses in an attribute.
async fn read_enums(
    pool: &PgPool,
    table: &Table,
    composites: &[PgComposite],
) -> Result<Vec<PgEnum>> {
    let is_enum = |ty: &PgType| matches!(ty, PgType::Enum { .. });
    let mut wanted: Vec<(String, String)> = Vec::new();
    collect(&table.columns, &mut wanted, is_enum);
    for composite in composites {
        collect(&composite.fields, &mut wanted, is_enum);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, kind: &str, extension: Option<&str>) -> RawType {
        RawType {
            name: name.to_string(),
            kind: Some(kind.to_string()),
            schema: Some("public".to_string()),
            relkind: (kind == "c").then(|| "c".to_string()),
            extension: extension.map(str::to_string),
        }
    }

    #[test]
    fn an_array_of_a_domain_resolves_to_the_base_and_its_extension() {
        // local_geometry[] where local_geometry is a domain over PostGIS
        // geometry: the element unwraps to geometry, and the extension
        // hint is PostGIS's, not the domain's own (a domain has none).
        let (ty, extension) = resolve_type(RawColumnType {
            ty: raw("_local_geometry", "b", None),
            elem: Some(raw("local_geometry", "d", None)),
            elem_base: Some(raw("geometry", "b", Some("postgis"))),
            base: None,
        });
        match ty {
            PgType::Array(inner) => {
                assert!(matches!(*inner, PgType::Scalar(ref n) if n == "geometry"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(extension.as_deref(), Some("postgis"));

        // A plain array of an extension type keeps working as before.
        let (_, extension) = resolve_type(RawColumnType {
            ty: raw("_hstore", "b", None),
            elem: Some(raw("hstore", "b", Some("hstore"))),
            elem_base: None,
            base: None,
        });
        assert_eq!(extension.as_deref(), Some("hstore"));
    }

    #[test]
    fn only_a_standalone_composite_is_a_composite() {
        let (ty, _) = resolve_type(RawColumnType {
            ty: raw("dimensions", "c", None),
            ..Default::default()
        });
        assert!(matches!(ty, PgType::Composite { ref name, .. } if name == "dimensions"));

        // A column typed by a table's row type: relkind is the table's.
        let mut row_type = raw("item", "c", None);
        row_type.relkind = Some("r".to_string());
        let (ty, _) = resolve_type(RawColumnType {
            ty: row_type,
            ..Default::default()
        });
        assert!(matches!(ty, PgType::Scalar(ref name) if name == "item"));
    }

    fn composite(name: &str, fields: &[(&str, PgType)]) -> PgComposite {
        PgComposite {
            schema: "shop".into(),
            name: name.into(),
            comment: None,
            fields: fields
                .iter()
                .map(|(field, ty)| Column {
                    name: (*field).to_string(),
                    ty: ty.clone(),
                    sql_type: String::new(),
                    not_null: false,
                    comment: None,
                    has_default: false,
                    default_expr: None,
                    identity: false,
                    generated: false,
                    extension: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_nested_type_named_directly_still_comes_before_its_holder() {
        // Columns `a a, b b` where b holds a: discovery lists a first,
        // then b, and a reversal would put B's definition before A's.
        let a = || composite("a", &[("lo", PgType::Scalar("numeric".into()))]);
        let b = || {
            composite(
                "b",
                &[(
                    "inner",
                    PgType::Array(Box::new(PgType::Composite {
                        schema: "shop".into(),
                        name: "a".into(),
                    })),
                )],
            )
        };
        let names = |list: Vec<PgComposite>| -> Vec<String> {
            dependencies_first(list)
                .into_iter()
                .map(|c| c.name)
                .collect()
        };
        assert_eq!(names(vec![a(), b()]), ["a", "b"]);
        assert_eq!(names(vec![b(), a()]), ["a", "b"]);
    }
}
