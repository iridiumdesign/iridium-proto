<p align="center">
  <img
    src="https://raw.githubusercontent.com/iridiumdesign/iridium-proto/main/branding/iridiumdesign-proto-banner.png"
    alt="iridium-proto"
    width="880">
</p>

A common pattern in application development is to create models and mappers to
interact with a database, and then keep the models and mappers in sync with
changes to the database. Once those models are built, you extend and modify
them in other ways to support the needs of the project, which makes it harder
for any tool to synchronize those changes.

I've built versions of `proto` for several projects in various languages.
They've grown stale and been left behind as I've moved between teams. This time
I've decided to share it. Honestly, because I'm done recreating it.

This project is opinionated. When I create or modernize a project I choose
PostgreSQL. I start a project with the database design, as the data often
outlives any individual software component. Then come the model classes that
represent the data, with a layer of old-school mapping objects that create the
seam between the application's representation and the current state of the
database. `proto` builds the layer after you have your database.

`proto` is written in Rust and creates that layer in Rust, with the option to
make it Python-compatible. This is another opinion, based on how my teams are
currently writing software. There is a lot of Python remaining at the edges,
with the core moving to the performance and safety of Rust.

The generated code is written against `sqlx`, which is one more opinion.
By default the SQL is embedded in the mappers. Choose `--sql server` and
`proto` moves the CRUD into Postgres functions instead, written as a
migration file alongside your own, and the mappers call those functions.
Either way the intent is the same: migrations move the database forward,
and `proto` keeps the model and mapper layer in step with wherever they
have taken it. It never runs migrations and never touches yours.

`proto` is designed to be idempotent. You can safely rerun the tool on your
project to confirm that it is in sync. If there are changes, only the changes
are applied. If you modify one column, only that column is modified.

The tool uses abstract syntax trees to understand your models and mappers. It
can update individual fields in your models, but it updates entire functions
within the mappers. For example, if you modify a mapper's `create` function
with custom functionality, you will lose those modifications. Each function
`proto` owns says so at the end of its doc comment. Anything in the mapper
without that notice is yours to modify.

Point it at a table and it writes the model:

```
proto model shop.product --pyo3
```

Output goes to stdout by default, which is the point: from an editor,
`:%!proto model shop.product --pyo3` replaces the buffer with the
generated model, and `:r !proto ...` drops it in below the cursor. Pass
`-o` or `--out-dir` when you want files instead.

## The workflow

The database moves first and the code follows. A change to the schema
goes through the same loop every time:

1. Write a migration and apply it to your local development database.
2. Run `proto`. It corrects the models and mappers to match what the
   database now says, and leaves everything else in those files alone.
3. If the mappers use `--sql server` and the change created or altered a
   table's CRUD, `proto` writes a migration with the new or updated
   functions, next to the one you wrote.
4. Apply that migration, then run `proto --check`. It exits clean when
   there is nothing left to write, which means the two sides are in sync.

Step 4 is the same question CI asks: has someone added a migration
without regenerating, or regenerated without applying?

## Generated code you can live in

Most generators make you choose. Either you never touch the output, or
you take it over and it stops following the database. `proto` does
neither: **a regeneration corrects a file rather than replacing it.**

It renders what the file *should* say, parses both that and what is on
disk, and edits only where they disagree. Here is a model six months
in — the struct `proto` wrote, with a developer's comments woven through
it, their own `impl` below, and a field somebody has quietly got wrong:

```rust
pub struct Item {
    pub id: Uuid,
    // Prices are ex-VAT — checked with finance 2026-08-30.
    pub price: Option<f64>,          // wrong: the column is numeric
    // The tags come from the importer, not from us.
    pub tags: Option<Vec<String>>,
}

impl Item {
    /// Written last week.
    pub fn dear(&self) -> bool { self.price.is_some() }
}
```

A migration adds a `color` column. Regenerate:

```
$ proto schema shop --out-dir src/model
  updated src/model/item.rs
      ~price: Option<f64> -> Option<Decimal>, +color: Option<String>
1 updated, 11 unchanged
```

One type replaced, one field inserted. Both comments still bracket the
field they were written about. The `impl` is untouched. **Nothing had to
be marked to be spared** — no regions, no `keep` tags, nothing a
formatter or a careless merge could delete — because nothing was
rewritten.

**The database is right**, and it is right about the parts it owns:
column names, types, nullability. Whether a field disagrees because a
migration changed it or because somebody edited the line, `proto` fixes it
and says which field and what it was. Everything else in the file is
yours, including the order — fields are matched by name, so rearrange
them however your team reads best and a later migration will add its
column to the arrangement you chose.

How it works: the file is parsed to a syntax tree, and each node carries
its source range, so `proto` uses the tree only to *locate* — this type,
these bytes — and patches the text in place. It never prints code back
out of the tree, which is what would eat the comments. The same approach
`cargo fix` uses to apply a suggestion without reformatting your file.

Mappers get the same treatment, a method at a time: a migration updates
the statements it invalidates and leaves the query you wrote by hand
alone. [More on what is and is not reconciled](#editing-generated-code).

## Install

```
cargo install iridium-proto
```

The binary is `proto`. The library is `iridium-proto`, so the same work
can be done from a build script or a test.

## Configuration

`proto` reads a TOML config from `$PROTO_CONFIG`, else
`$XDG_CONFIG_HOME/proto/proto.toml`, else `~/.config/proto/proto.toml`.
See `proto.example.toml` for the full file.

```toml
[databases.dev]
host = "localhost"
name = "shop"
user = "shop"
password = "devpass"
```

Select a target with `--db dev`. When the file defines **exactly one**
database, that one is the default and `--db` is unnecessary; with more
than one, set `default_db` or pass `--db`.

Unknown keys in a `[databases.*]` table are ignored, so a config written
for another tool can be pointed at directly rather than copied:

```
PROTO_CONFIG=~/.config/other-tool/other.toml proto --db dev list
```

Passwords resolve in this order: `password`, `password_file`,
`password_env`, `$PGPASSWORD`, `~/.pgpass` (or `$PGPASSFILE`). None of
them is required — a server set to `trust`, which a local development
one often is, wants no password and a target can simply omit it. A
target may also carry a single `url`, and `--url` or `$DATABASE_URL`
skips the config file entirely.

`proto config` prints the resolved file, its targets, and the generation
defaults.

## Commands

```
proto model <schema.table>   Generate one model
proto mapper <schema.table>  Generate one mapper
proto schema <schema>        Generate every model in a schema
proto database               Generate every schema, one directory each
proto list [schema]          List schemas, or the tables in one schema
proto config                 Show the resolved config
```

| Flag | Applies to | Effect |
|---|---|---|
| `--db <target>` | all | Database target from the config file |
| `--url <url>` | all | Connection string, bypassing the config |
| `--config <path>` | all | Config file to read |
| `--sql <where>` | mappers | `embedded` (default) or `server` |
| `--model-path <path>` | mappers | Module the mappers import models from |
| `--migrations-dir <dir>` | `--sql server` | Where the migrations go, overriding the config |
| `--migration-tag <tag>` | `--sql server` | Value for `{tag}` in `migration_name` |
| `--pyo3` | models, mappers | Emit feature-gated pyo3 attributes |
| `--input` | `model` | Also emit the `New…` insert type |
| `-o, --out <file>` | single | Write to a file instead of stdout |
| `--out-dir <dir>` | bulk | Write one model file per table |
| `--mappers` | bulk | Also generate mappers |
| `--mapper-dir <dir>` | bulk | Write one mapper file per table |
| `--name <name>` | single | Struct name, overriding the derived one |
| `--no-mod` | bulk | Skip the generated `mod.rs` |
| `--force` | writers | Overwrite a file `proto` did not generate |
| `--no-manifest` | writers | Leave `Cargo.toml` alone |

`proto schema` without `--out-dir` writes one flat stream: the schema's
enum and composite types once, then a struct per table. With `--out-dir`
it writes `<table>.rs` per table, an `enums.rs` when the schema uses
enum or composite types, and a `mod.rs`. `proto database` does the same
one directory per schema, plus a top-level `mod.rs`.

A full tree, models and mappers side by side:

```
proto database --mappers --out-dir src/model --mapper-dir src/mapper
```

Generated files carry an `@generated by proto` header. Writing over a
file that lacks it fails unless you pass `--force`, so a hand-written
model is never clobbered by a stray `--out-dir`.

## Models

A table becomes one struct, named by re-casing the table name —
`order_items` becomes `OrderItems`. No singularization; use `--name` when
that reads wrong. `NOT NULL` columns get a bare type, everything else
gets `Option`. Column and table comments become doc comments, and the
primary key is noted above the struct.

```rust
// @generated by proto 0.1.0 — regenerate rather than rewrite.
// source: dev · shop.product (table)
// regenerate: proto model shop.product --db dev

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Primary key: `id`.
#[derive(sqlx::FromRow, Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    pub id: Uuid,
    /// Stable external identifier.
    pub slug: String,
    pub name: String,
    pub status: ProductStatus,
    pub price: Option<Decimal>,
    pub created_at: DateTime<Utc>,
}
```

Views and materialized views report every column as nullable, so every
field comes back `Option`. The header says so; tighten by hand where you
know better.

A Postgres enum type becomes a Rust enum beside the struct:

```rust
/// The `shop.product_status` enum type.
#[derive(sqlx::Type, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[sqlx(type_name = "product_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ProductStatus {
    Draft,
    Active,
    Retired,
}
```

Labels that do not round-trip through `snake_case` get an explicit
`#[sqlx(rename = "...")]` per variant instead.

A composite type — `CREATE TYPE shop.dimensions AS (...)` — becomes a
struct the same way, filed with the enums:

```rust
/// The `shop.dimensions` composite type. Every field is `Option`: an
/// attribute cannot be `NOT NULL`, so any of them may come back null.
#[derive(sqlx::Type, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[sqlx(type_name = "shop.dimensions")]
pub struct Dimensions {
    pub width: Option<Decimal>,
    pub height: Option<Decimal>,
    pub unit: Option<String>,
}
```

A column of that type is `Option<Dimensions>`, an array of it
`Option<Vec<Dimensions>>`, and a composite nested inside another is
generated first. `composite_derives` in the config sets the derives. A
table's own row type is not a composite for this purpose: a column typed
by one is reported as unmapped, like any other type `proto` does not
know.

`--input` adds the insert type:

```rust
/// Insert input for `shop.product`. Columns the database fills in on its
/// own are absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProduct {
    pub slug: String,
    pub name: String,
    /// `None` leaves this to the column default, `'draft'::shop.product_status`.
    pub status: Option<ProductStatus>,
    pub price: Option<Decimal>,
}
```

Columns the database owns — generated, identity, or defaulted to a
function call like `gen_random_uuid()` or `now()` — are left out; an
insert never supplies them. A column with a *literal* default is
`Option`, and `None` means "use the default". That distinction is the one
piece of judgment `proto` makes about your schema, and it is what keeps
the generated SQL static.

## Mappers

One repository struct per table, holding a pool and owning every
statement that touches it:

```rust
let products = ProductMapper::new(&pool);

let created = products.create(&NewProduct { .. }).await?;
let one     = products.find_by_id(id).await?;          // primary key
let by_slug = products.find_by_slug("dovetail").await?; // unique
let in_org  = products.find_by_org_id(org).await?;      // foreign key
let all     = products.list().await?;
let saved   = products.update(&row).await?;
products.delete(id).await?;
```

Finders come from the catalog: the primary key and every unique
constraint yield an `Option`, every single-column foreign key yields a
`Vec`. Read-only relations get finders and `list` but no writers, and a
table with no primary key gets no `find_by_id`, `update`, or `delete` —
there is nothing to address a row by.

`update` is a full replace, not a patch: it writes every column the
database does not own, addressed by the key. That matches what the
functions can express, so both strategies behave identically.

### Where the SQL lives

`--sql embedded` (the default) puts the statements in the Rust:

```rust
pub async fn create(&self, new: &NewProduct) -> Result<Product, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO shop.product
             (slug, name, status, price)
         VALUES ($1, $2, COALESCE($3, 'draft'::shop.product_status), $4)
         RETURNING *",
    )
    .bind(&new.slug)
    ...
}
```

`--sql server` puts them in Postgres and calls them:

```rust
sqlx::query_as("SELECT * FROM shop.product_insert($1, $2, $3, $4)")
```

with a migration to match, written into `--migrations-dir`:

```sql
CREATE OR REPLACE FUNCTION shop.product_insert(
    p_slug text,
    p_name text,
    p_status shop.product_status,
    p_price numeric(10,2)
)
RETURNS shop.product
LANGUAGE sql
AS $$
    INSERT INTO shop.product
        (slug, name, status, price)
    VALUES
        (p_slug, p_name, COALESCE(p_status, 'draft'::shop.product_status), p_price)
    RETURNING *;
$$;
```

Functions are named `<table>_<operation>` in the table's own schema:
`product_insert`, `product_get`, `product_by_slug`, `product_by_org_id`,
`product_list`, `product_update`, `product_delete`. Readers are `STABLE`;
writers are left `VOLATILE`. Readers return `SETOF`, so a miss is no rows
rather than a row of nulls.

Both strategies generate the same Rust API — same methods, same
signatures. Switching is a regeneration, not a rewrite of the callers.

Every function argument is required. Postgres only allows `DEFAULT` on
trailing parameters, and a defaulted column can sit anywhere in a table,
so giving some parameters defaults would mean reordering them away from
the column order. The mapper passes all of them regardless; add `DEFAULT`
clauses by hand if you want to call these from `psql` with fewer.

Migrations go where `--migrations-dir`, or `migrations_dir` under
`[generate]`, points. They are named `YYYYMMDDNNN_<schema>_<table>_crud.sql`
by default, taking the next free sequence for the day, which is the
shape `sqlx migrate` reads. A project with its own convention sets
`migration_name` in the config:

```toml
[generate]
migrations_dir = "migrations"
migration_name = "{version}_{tag}_{slug}.sql"
migration_tag = "v1.4"
```

`{version}` is the eleven-digit stamp, `{date}` and `{seq}` are its two
halves, `{slug}` is `<schema>_<table>_crud`, and `{schema}` and `{table}`
are there on their own. `{tag}` is whatever `migration_tag` says, or
`--migration-tag` for one run, which is how a target version gets onto
a file without editing the config each release. Anything else in the
pattern is written as given. `proto` reads the pattern back to find a
table's newest migration and the day's last sequence, so the files it
writes fit alongside the ones you write. The tag is not part of which
table a migration belongs to, so a tag bump over unchanged CRUD writes
nothing.
A pattern with `{seq}` but no `{date}` or `{version}` has no day to scope
the sequence to, so it counts up across every file and never restarts.

Regenerating an unchanged table leaves its migration alone rather than
writing a second one that says the same thing — migrations are
append-only, and a checksummed one that has already been applied must
not be edited. Each file opens by dropping exactly the functions it
defines, by name and whatever signature, so a regenerated set replaces
the old one instead of overloading it.

## Children

A single-column foreign key gives the child a finder. The parent gets
the other half: a field holding the child rows, and the methods that
fill it.

```rust
/// Rows of `shop.variant` whose `product_id` is this row's `id`. Not a
/// column: `ProductMapper::load_children` fills it, and it is empty
/// until then.
#[sqlx(skip)]
#[serde(default)]
pub children: Vec<Variant>,
```

```rust
let products = ProductMapper::new(&pool);
let mut one = products.find_by_id(id).await?.unwrap();
products.load_children(&mut one).await?;            // fills one.children
let same = products.find_by_id_with_children(id).await?;
```

One level: the children's own children are not loaded. A table that
refers to itself — `category.parent_id` to `category.id` — is the
common tree, and holds `children: Vec<Category>` like any other parent.

The field is `children` when exactly one table refers to the parent,
which is what most schemas look like; `children_field` in the config
renames it. A parent with several child tables names each field after
its child table (`variant`, `review`), or after the table and column
when the same child refers to it twice (`link_by_from_id`). Both can be
overridden per parent, which also pins a name so a second child table
arriving later cannot rename the first:

```toml
[generate]
children_field = "children"

[generate.relations."shop.product"]
children = "variants"                      # one child table

[generate.relations."shop.category"]
children = { category = "subcategories", product = "products" }
```

`children_field = ""` generates no children fields at all.

Under `--sql server` the parent's mapper calls the child's own finder
function, so nothing new is needed on the server. That function is
written by the *child's* migration, when the child's mapper is
generated: a schema or database run writes both, and a parent generated
on its own with `proto mapper` expects the child's to exist. A
one-to-one link, where the referring column is itself unique, is not a
collection and gets no field. A field that would collide — with a
column called `children`, say, or a child table named `date_time` whose
`DateTime` shadows `chrono`'s — is skipped with a warning that says how
to rename it. When a field goes away or is renamed, the methods that
filled it and the import it needed go with it; regenerating leaves no
stale loader behind. Neither does a child in another schema,
or one excluded by `exclude_tables`: its type would not be there to
name. A single `proto model` run imports the child's type from the
sibling module `super::<child>`, the layout `--out-dir` writes.

## Type mapping

| Postgres | Rust |
|---|---|
| `bool` | `bool` |
| `int2` / `int4` / `int8` | `i16` / `i32` / `i64` |
| `float4` / `float8` | `f32` / `f64` |
| `numeric` | `rust_decimal::Decimal` |
| `text`, `varchar`, `bpchar`, `name`, `citext`, `xml`, `ltree` | `String` |
| `uuid` | `uuid::Uuid` |
| `bytea` | `Vec<u8>` |
| `json`, `jsonb` | `serde_json::Value` |
| `timestamptz` | `chrono::DateTime<Utc>` |
| `timestamp` / `date` / `time` | `NaiveDateTime` / `NaiveDate` / `NaiveTime` |
| `interval` / `timetz` | `PgInterval` / `PgTimeTz` |
| `inet`, `cidr` | `std::net::IpAddr` |
| `int4range` and friends | `PgRange<T>` |
| `<type>[]` | `Vec<T>` |
| a domain | its base type |
| an enum type | a generated Rust enum |
| a composite type | a generated Rust struct |

Anything else lands as `String` with a `// TODO:` above it and a warning
on stderr. Where the type belongs to an extension, the warning says which
one and prints the config line that fixes it:

```
warning: shop.place.geom: unmapped Postgres type 'geometry' from the postgis extension, using String
  add to [generate.types]: geometry = "<rust path>"
```

Fix it once in the config:

```toml
[generate.types]
geometry = "geo_types::Geometry<f64>"
```

`[generate.types]` overrides the built-ins too, so a project that wants
`bigdecimal::BigDecimal` for `numeric` says so there.

Introspection runs with an empty `search_path`, so every type in the
generated SQL is written out with its schema. A cast like
`'draft'::shop.product_status` does not depend on the `search_path` of
whoever runs it later.

Identifiers are quoted where quoting changes the meaning — a reserved
word like `order`, a name Postgres would fold such as `Mixed Case`, a
name carrying punctuation — and left bare where it does not, so an
ordinary schema still produces ordinary-looking SQL.

Rust names go the other way. A field or module name is folded to
lowercase with punctuation collapsed, and the `#[sqlx(rename = "...")]`
carries the real column name, so `"Mixed Case"` becomes `mixed_case`
rather than something rustc lints on. Generated code compiles without
warnings whatever the schema looks like. Two columns that reduce to one
field name is the exception: that would not compile, so it is reported
on stderr rather than emitted quietly.

## Dependencies

The generated code names `sqlx`, `serde`, `uuid`, `chrono` and the rest
by full path, so the crate it lands in has to declare them. When the
output is written into a Cargo project — `--out` or `--out-dir` —
`proto` finds the nearest `Cargo.toml` above it and adds what is
missing, with the version and features the output is written against:

```
$ proto schema shop --out-dir src/model
  created src/model/product.rs
  updated Cargo.toml
      +chrono, +rust_decimal, +serde, +sqlx, +uuid
1 created, 1 updated
```

Only what a run actually used is added: a schema with no `numeric`
column pulls in no `rust_decimal`. A dependency already declared is
left exactly as it is, whatever its version or features. Where it lacks
a feature the output needs — `sqlx` without `uuid`, say — `proto` says
so on stderr rather than editing your line. Comments and ordering
elsewhere in the file survive; the manifest is patched, not reprinted.

`--pyo3` adds `pyo3` as an optional dependency and the feature that
turns it on, listing the conversions the columns need (see
[pyo3](#pyo3)). A crate that inherits from `[workspace.dependencies]`
gets a `workspace = true` entry instead of a version.

`--check` counts a missing dependency as drift, the same as a missing
column. Output to stdout touches no manifest. `--no-manifest`, or
`manifest = false` under `[generate]`, turns the whole thing off. A
type from `[generate.types]` names a crate `proto` knows no version
for, so a missing one is reported, not added.

## Keeping in step with the schema

A database moves. Regenerating is safe to do on a habit rather than a
decision: a run over an unchanged schema writes nothing and says so.

```
$ proto schema shop --out-dir src/model
3 unchanged
```

When something did move, the run says what — and for a model, which
columns:

```
$ proto schema shop --out-dir src/model
  created src/model/invoice.rs
  updated src/model/product.rs
      ~notes: String -> Option<String>, +cultivar: Option<String>, -slug
  updated src/model/mod.rs
1 created, 2 updated
```

`--check` writes nothing and exits non-zero if regenerating would change
anything. That is the question CI wants to ask — has someone added a
migration without regenerating the models? — and it reads the same as
`cargo fmt --check`:

```
$ proto schema shop --out-dir src/model --check
  would update src/model/product.rs
      +cultivar: Option<String>
1 would update
error: the generated tree is not in step with the database
$ echo $?
1
```

`--prune` deletes files `proto` generated for relations that no longer
exist, which is the one case regenerating cannot fix on its own — a
dropped table leaves a model behind that still compiles. Only files
carrying the `@generated by proto` header are removed, so a directory
you also keep hand-written code in survives intact.

```
$ proto schema shop --out-dir src/model --prune
  removed src/model/legacy_price.rs
1 removed
```

Migrations are left alone by all of this: they are append-only, and an
applied one is checksummed. A regenerated table whose CRUD has not
changed keeps the migration it already has.

## Editing generated code

Generated files ask you to *regenerate rather than rewrite*, and for
the most part you should — the database decides what is in them. But
they are code in your tree, and code in your tree gets read, annotated
and extended. So a regeneration corrects a file rather than replacing
it.

Proto renders what the file *should* say, parses both that and what is
on disk, and edits only where they disagree. Take a file that has been
lived in for a few months:

```rust
pub struct Item {
    pub id: Uuid,
    // Prices are ex-VAT — checked with finance 2026-08-30.
    pub price: Option<f64>,          // somebody got this wrong
    // The tags come from the importer, not from us.
    pub tags: Option<Vec<String>>,
}

impl Item {
    /// Written last week.
    pub fn dear(&self) -> bool { self.price.is_some() }
}
```

A migration adds a `color` column. Regenerate:

```
  updated src/model/item.rs
      ~price: Option<f64> -> Option<Decimal>, +color: Option<String>
```

The wrong type is corrected and the new column arrives. Both comments
still bracket the field they were written about, and the `impl` below is
untouched. Nothing had to be marked to be spared, because nothing was
rewritten — one type was replaced and one field was inserted.

**The database is right.** Whether a field disagrees because a migration
changed the column or because somebody edited the line, the file is
wrong and `proto` fixes it. It says which field and what it was, so the
correction is never silent.

**Order is not.** Column order in Postgres is a storage artifact — drop
and re-add a column and it moves to the end — and it means nothing to
`FromRow`, which matches by name. So an existing field never moves: it
carries the comment above it, and no comment is worth a line's
tidiness. A new field still lands beside the neighbors the database
gives it.

This cuts both ways, which is the useful part. Fields are matched by
name, so how a struct is arranged is your business: group the keys, put
the interesting columns first, and `proto` sees a struct with all the
right fields and nothing to do. A later migration adds its column to
the arrangement you chose rather than undoing it.

**Mappers work the same way, per method.** `proto` owns the methods it
derives from the catalog, so a `create` whose column list is out of date
is put back. Each of those methods says so at the end of its doc
comment, and the file says once, under its header, that everything
without that notice is yours. It owns nothing else in the block: a
query you added because the schema does not imply it is not something
`proto` has an opinion about. Adding a column to a table touches
`create` and `update` and leaves every other method — including yours —
byte for byte as it was.

```
$ diff before.rs after.rs
<                  (id, slug, price)
>                  (id, slug, price, color)
<              VALUES ($1, $2, $3)
>              VALUES ($1, $2, $3, $4)
>         .bind(&new.color)
```

**What is not reconciled.** A file that does not parse is left alone and
written whole, since half an edit is nothing to reason about. Enum
variants are replaced as a unit rather than one at a time — a variant
list is not somewhere a line gets edited — but only they are, and
anything else in that file stays.

**Taking a file over: delete the `@generated` line.** Without the
marker, `proto` stops managing that file. It says so on every run and
leaves it where it is:

```
$ proto database --mappers --out-dir src/model --mapper-dir src/mapper
  left alone (not generated by proto) src/model/canopy/tree_measurement.rs
1 left alone (not generated by proto), 91 unchanged
```

The file stops following the database, and a column added tomorrow will
not appear in it. Rarely what you want now that edits survive on their
own. `--force` puts it back under the management of `proto`.

The same guard protects you from a mistake: point `--out-dir` at a
directory of hand-written code and `proto` refuses to overwrite any of it,
listing what it left alone rather than failing on the first file.

## Safety

`proto` handles three things worth being careful with: a database and
the credentials to reach it, files in your tree, and SQL that you will
run later without reading all of it.

### The SQL it writes

**Values are bound, never interpolated.** Every value a mapper sends
reaches the database as a `$N` parameter. Nothing is assembled at run
time — no `format!`, no concatenation — so a query is a plain string
literal you can read in full at the call site. An application passing
user input to a generated finder cannot be injected through it.

**Identifiers are quoted.** Table and column names are copied out of the
catalogs into SQL text at generation time, and they are quoted wherever
quoting changes the meaning, with internal quotes doubled. A name
carrying a statement terminator stays one identifier rather than
becoming a second statement.

**The statement is escaped into the Rust literal that holds it.** This
is the one that is easy to miss: a quoted identifier carries `"`, which
is exactly the character that ends a Rust string literal. Escaping it is
what keeps a hostile name inside the data rather than loose in your
source. A table named ``x"; DROP TABLE audit; --`` comes out as:

```rust
sqlx::query_as("SELECT * FROM public.\"x\"\"; DROP TABLE audit; --\" WHERE id = $1")
    .bind(id)
```

One quoted identifier, one bound parameter, and nothing executable that
did not come from the schema.

### Credentials

They never reach generated output. Headers name the *target* — `dev`, or
the literal string `--url` — never a connection string, because those
headers get committed:

```rust
// @generated by proto 0.1.0 — regenerate rather than rewrite.
// source: dev · shop.product (table)
```

Passwords resolve from `password`, `password_file`, `password_env`,
`$PGPASSWORD`, then `~/.pgpass`, so a config file that gets committed
need not carry one. `.github/proto.ci.toml` in this repository is an
example: it names a variable rather than a secret.

### Files

**Names are sanitized before they become paths.** A module or file name
is derived from the relation name with everything but alphanumerics
folded to underscores, so a relation named `../../etc/passwd` yields
`_etc_passwd` and stays inside the directory you named.

**Hand-written files are not overwritten.** Everything `proto` emits
carries an `@generated by proto` header. Writing over a file that lacks
one fails unless you pass `--force`, so pointing `--out-dir` at the
wrong directory costs you an error rather than your work.

**Migrations are append-only.** A migration that has been applied is
checksummed and must not be edited, so regenerating a table whose CRUD
has not changed leaves its migration alone instead of writing a second
one that says the same thing.

**The manifest is only added to.** `Cargo.toml` gets the dependencies
the output needs and nothing it already has is edited — not a version,
not a feature list. The file is patched in place, so comments and
ordering stay where they are. `--no-manifest` leaves it alone entirely.

### What it does not protect you from

**The database you point it at is trusted.** `proto` reads identifiers,
comments, enum labels and default expressions and writes them into code
you then compile. It is a transcription tool. Pointing it at a database
someone else controls and building the result is equivalent to running
their code, and no amount of escaping inside `proto` changes that — the
doc comments alone are enough. This is why the default is stdout: read
what it emitted.

**A mapper you have edited is your code.** The guarantees above describe
what `proto` generates, not what a file becomes afterwards.

### How this is checked

`tests/injection.rs` covers the three surfaces above, using the public
API and a table whose name and one of whose columns are injection
payloads. The tests are mutation-checked: disabling identifier quoting
fails two of them, and dropping a single `.bind` call fails another.

The round trip in `scripts/smoke.sh` carries it to a real server. It
creates a table whose name is an injection payload alongside a table for
that payload to destroy, generates and applies the functions, and fails
if the neighbor is gone. CI runs it on PostgreSQL 16 and 17 on every
push.

Reports go through [SECURITY.md](SECURITY.md), which sets out the same
boundary in the terms a security reviewer will want.

## pyo3

`--pyo3` gates every Python attribute behind a Cargo feature, so the same
file compiles in a pure-Rust crate and in a Python extension crate:

```rust
#[derive(sqlx::FromRow, Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "python", pyo3::pyclass(get_all, set_all))]
pub struct Product { ... }
```

Getters and setters come from `get_all, set_all` on the class rather than
a `#[pyo3(get, set)]` on each field. That is deliberate: `pyclass`
expands before `cfg_attr` does, so a field-level `cfg_attr` leaves an
orphaned `pyo3` attribute and the crate will not compile. Enums get
`pyclass(eq, eq_int)`, and composite types `get_all, set_all` like a row
struct.

Input types are classes too, with a constructor. A column that is
`NOT NULL` without a default is a required argument; everything else is
keyword-only and defaults to `None`, which leaves a defaulted column to
the database:

```python
NewProduct("dovetail-saw", "Dovetail saw", org_id)
NewProduct("dovetail-saw", "Dovetail saw", org_id, status=ProductStatus.Active)
```

The consuming crate declares the feature and the pyo3 conversions its
column types need. `proto` writes both into `Cargo.toml` when the
output lands in a crate (see [Dependencies](#dependencies)); by hand,
it is:

```toml
[features]
python = ["dep:pyo3", "pyo3/chrono", "pyo3/uuid", "pyo3/rust_decimal"]

[dependencies]
pyo3 = { version = "0.26", optional = true }
```

Declare the feature even when it is off — otherwise every generated file
draws an `unexpected cfg condition` warning. Rename it with
`pyo3_feature` in the config.

Where a schema run puts enum and composite types in their own module,
each model re-exports the ones its struct names, so
`model::item::ItemStatus` resolves for whoever holds an `Item` and
nobody has to know where `proto` filed it.

`--pymodule <name>` writes the registration too, so nothing about the
crossing is hand-maintained:

```
proto schema shop --pyo3 --pymodule shop --out-dir src/model
```

```rust
/// Register every generated class in `shop` on a module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<super::enums::ProductStatus>()?;
    m.add_class::<super::product::Product>()?;
    Ok(())
}

/// The classes as an extension module, for when that is all you need.
#[pymodule]
pub fn shop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    register(m)
}
```

It lands as `python.rs` beside the models, and the generated `mod.rs`
declares it behind the pyo3 feature. An extension has room for exactly
one module initializer, so when yours needs functions of its own — a
mapper-backed query, say — write your own `#[pymodule]` and call
`register` from it rather than listing classes by hand.

`proto database --pymodule <name>` does the same across schemas, one
Python submodule each, registered in `sys.modules` so both
`import store.shop` and `from store.shop import Product` work. Two
schemas may hold a table of the same name without colliding, which is
why the classes are named by full path rather than imported.

The `#[pymodule]` does not have to sit at the crate root — pyo3 exports
its initializer from a nested module just as well — so a consumer needs
no glue at all beyond declaring the module.

### Mappers from Python

`--pyo3` on a mapper — `proto mapper shop.product --pyo3`, or `--mappers
--pyo3` on a schema or database run — adds a Python class beside the
Rust one, with the same methods:

```python
db = shop.Database("postgres://shop@localhost/shop")
products = shop.ProductMapper(db)

made = products.create(NewProduct("dovetail-saw", "Dovetail saw", org_id))
one = products.find_by_id(made.id)
one.name = "Dovetail saw, 10in"
products.update(one)
products.delete(one.id)
```

The Rust mappers are `async` and Python, from here, is not: each call
runs to completion on a tokio runtime the `Database` holds, with the GIL
released meanwhile. An `asyncio`-native form is a follow-up. Every
database error arrives as `ProtoError`, a plain `Exception` carrying the
driver's message, so a lookup that finds nothing is `None` and a failed
statement is an exception, the same shape as the Rust.

`Database`, `ProtoError`, and a `register` for the mapper classes live
in a `python.rs` beside the mappers, which a schema or database run
writes and the mappers' `mod.rs` declares behind the feature. A lone
`proto mapper --pyo3` assumes that file at `super::python`. The bridge
has no `#[pymodule]` of its own — an extension has room for one, and the
model side may already have written it — so register both from yours,
models first:

```rust
#[pymodule]
fn shop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    model::python::register(m)?;
    mapper::python::register(m)
}
```

The consuming crate needs `tokio` for the runtime, alongside what the
models already asked for. Both strategies expose the same Python
surface, since the class wraps whichever mapper is beside it.

Everything crosses as the native Python type, and keeps the schema's
guarantees on the far side:

| Column | Python |
|---|---|
| `uuid` | `uuid.UUID` |
| `text` | `str` |
| `numeric` | `decimal.Decimal` |
| `timestamptz` | aware `datetime.datetime` |
| `text[]` | `list[str]` |
| an enum type | a Python enum, comparable and `int()`-able |
| a nullable column | the value, or `None` |

Setters are type-checked rather than coercing, and a `NOT NULL` column
refuses `None`. `just python` proves all of that against a real
interpreter rather than taking it on trust — see below.

## Development

`just` lists the recipes. `just check` is the fast gate — formatting,
lints, tests, and rustdoc with warnings promoted to errors. None of it
needs a database.

`just smoke` does need one. It creates a scratch schema covering the
cases that decide output, generates models, mappers and functions from
it, compiles the result as its own crate, applies the functions and
exercises them, then rolls back and drops the schema. Point it at a
target with `PROTO_SMOKE_DB`, or run `./scripts/smoke.sh <target>`;
given neither, they use whatever `proto config` reports as the default.

`just python` needs a database and an interpreter. `cargo check
--features python` only proves the pyo3 output compiles; this builds a
real extension module out of generated models, imports it, and reads and
writes every field from Python — including that a nullable column takes
`None`, that the enum compares, and that a `NOT NULL` column still
refuses `None` on the Python side.

CI runs the same gates: formatting, lints, tests on stable and beta,
rustdoc with warnings as errors, a build on the MSRV floor, the round
trip against a real server — PostgreSQL 16 and 17, each as a service
container — and the Python interop. The config it uses is
`.github/proto.ci.toml`, which carries no password: `password_env`
points at the variable the workflow sets for both `proto` and `psql`.

Patches are welcome on terms set out in
[CONTRIBUTING.md](CONTRIBUTING.md) — read it before writing code, not
after. Security problems go through [SECURITY.md](SECURITY.md) rather
than the public tracker.

## License

MIT OR Apache-2.0.
