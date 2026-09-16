# Mappers

Part of the [iridium-proto](../README.md) reference: the repository
struct per table, `find_where` over a `Query`, and where the SQL
lives — in the Rust, or in Postgres functions with a migration to
match.

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

Between the finders and a hand-written query sits `find_where`, which
takes a `Query` built at run time:

```rust
use crate::mapper::query::Query;

let cheap = products
    .find_where(
        Query::new()
            .eq("status", ProductStatus::Active)
            .lt("price", Decimal::new(1000, 2))
            .not_null("org_id")
            .order_by("name")
            .limit(20),
    )
    .await?;
let n = products.count_where(Query::new().any("id", ids)).await?;
```

Column names are Postgres names. Every value is bound, never written
into the statement, and a name that is not a column of the table is an
error before anything is sent. `Query` lives in a `query.rs` beside the
mappers, generated once and depending on `sqlx` alone; a lone
`proto mapper` assumes it at `super::query`. This is the one method
whose SQL is assembled at run time under either strategy: a function
cannot take a clause the caller composes, so `--sql server` embeds it
too, and the method's doc comment says so.

## Where the SQL lives

`--sql embedded` (the default) puts the statements in the Rust:

```rust
pub async fn create(&self, new: &NewProduct) -> Result<Product, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO shop.product
             (slug, name, status, price)
         VALUES ($1, $2, COALESCE($3, 'draft'::shop.product_status), $4)
         RETURNING id, slug, name, status, price, org_id, created_at",
    )
    .bind(&new.slug)
    ...
}
```

Every statement names its columns; none says `*`. The statement then
says what the struct expects, so a column the table gained or lost is
drift the file shows and `--check` reports, rather than a surprise at
run time, and the columns come back in the order the struct holds
them. A wide table's list wraps under its first column.

`--sql server` puts them in Postgres and calls them:

```rust
sqlx::query_as(
    "SELECT id, slug, name, status, price, org_id, created_at
       FROM shop.product_insert($1, $2, $3, $4)",
)
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
rather than a row of nulls. The functions keep `*`: they return the
table's row type, whatever it holds, and it is the caller that names
the columns — so a column added later shows up in the mapper on the
next run, not as a function that no longer matches its table.

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
