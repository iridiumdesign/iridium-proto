# Type mapping and dependencies

Part of the [iridium-proto](../README.md) reference: Postgres types
to Rust types, the overrides, and what `proto` adds to `Cargo.toml`.

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
`bigdecimal::BigDecimal` for `numeric` says so there. A type named there
is used wherever the column's type goes, so it has to carry what the
generated code asks of it: the `sqlx` traits the mapper binds and
decodes with, and `Clone` when the column is a key that
`set_id_on_children` copies into child rows.

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
[pyo3](pyo3.md)). A crate that inherits from `[workspace.dependencies]`
gets a `workspace = true` entry instead of a version.

`--check` counts a missing dependency as drift, the same as a missing
column. Output to stdout touches no manifest. `--no-manifest`, or
`manifest = false` under `[generate]`, turns the whole thing off. A
type from `[generate.types]` names a crate `proto` knows no version
for, so a missing one is reported, not added.
