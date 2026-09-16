# Changelog

Every release of `proto`, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
versions follow [Semantic Versioning](https://semver.org/) once 1.0
lands; before that a minor bump may change a public shape, and this
file says so when it does.

## [Unreleased]

Planned for 0.1.2: a Rust-native `Data` operation, with the Python
class as a thin wrapper over it ([#17]). See the
[0.1.2 milestone](https://github.com/iridiumdesign/iridium-proto/milestone/1).

### Added

- A `…Patch` type beside every `New…`, with `--input`: every column an
  update may write as `Option`, `None` to leave it and `Some(None)` to
  set a nullable column null, the key and the database's own columns
  absent, and `apply`, which sets what the patch carries on a row. The
  other half of what a write takes; the `Data` operation's `update` is
  written with it. ([#17])
- `Data.find(table, pk, user_id, request_id)`, `Data.store(model, …)`
  and `Data.update(model, …)`: by key and by type, each writing one
  line to the `proto.data` logger with both ids before returning. With
  them the module exports `OperationError`, `DatabaseError` and
  `PermissionError`, the errors the original Python proto had; a
  mapper's `ProtoError` reaches an operation's caller as
  `DatabaseError`. ([#25])
- `set_id_on_children` on every row type: copies the row's key into
  every child row it holds and on down through theirs, in memory, so a
  parent and its children built by hand agree before a save, however
  deep. A nullable foreign key takes `Some`; a key that is not `Copy`
  is cloned, and a key whose type proto cannot clone — a generated
  enum or composite whose derives lack `Clone` — is skipped with a
  warning. A table with no children fields has it too, doing nothing.
  Under `--pyo3` it is a method of the Python class and changes the
  object in place. ([#21])

### Changed

- `Data` is Rust. `Data::new(&pool).execute(Request { table, op, query,
  values })` routes by table name to the mapper that serves it and
  returns an `Outcome`: `Rows` for `find` and `update`, `Row` for
  `create`, `Count`, `Deleted`, with one variant per table named by
  schema and table (`Rows::ShopProduct`). `Values` carries a table's
  `New…` for `create` or its `…Patch` for `update`. Errors are the
  operation's own `Error`. The Python class of
  the same name is now a wrapper: the dict is read as the table's
  types, runs through the Rust, and the outcome comes back as the
  table's classes, with the same exceptions as before. `--operations`
  no longer needs `--pyo3`, and `operation/mod.rs` declares `data`
  unconditionally. An `operation/data.rs` from 0.1.1 has the old shape
  and does not reconcile: delete it and regenerate. ([#17])
- The mappers' Python bridge gained `block_on`, which runs any future
  on the `Database`'s runtime with the GIL released, for a call whose
  error is its own; `run` stays for the mapper calls.
- Reconcile keys a trait impl by its trait as well as its type, so
  `impl Display for Error` and `impl From<E> for Error` are two blocks
  and a method of one cannot land in the other.
- Every statement a mapper writes names its columns; none says `*`.
  The finders, `list`, the children loaders, `find_where`, the
  `RETURNING` of `create` and `update`, and under `--sql server` the
  calls, all select what the struct holds, in its order, wrapped where
  a table is wide. The server functions themselves keep `*`: they
  return the table's row type and the caller names the columns. The
  catalog now records a child table's columns for its loader. ([#24])
- Children fields are named after their child table with
  `children_field` as the suffix: `variant_children`, and
  `link_children_by_from_id` when the same child refers to the parent
  twice. The loaders follow, `load_variant_children` and
  `find_by_id_with_variant_children`. The name no longer depends on how
  many child tables the parent has, so a second one arriving later
  cannot rename the first. A parent with one child table, which 0.1.1
  called `children`, is renamed on the next run; `[generate.relations]`
  pins the old name where that matters. ([#19])

## [0.1.1] — 2026-09-13

The layer above the mappers, and the Python side of all of it.

### Added

- The dependencies the output needs are added to the nearest
  `Cargo.toml` — `sqlx`, `serde`, the type crates the columns use,
  `pyo3` and `tokio` when asked for. What is already declared is left
  alone, and `--check` counts a missing one as drift. `--no-manifest`
  and `manifest = false` turn it off. ([#5])
- Parent/child relationships: a single-column foreign key gives the
  parent a `children` field and its mapper `load_children` and
  `find_by_id_with_children`. `children_field` and
  `[generate.relations]` name the field; self-referential tables work.
  ([#6]) From Python, `load_children` returns a filled copy. ([#13])
- `--pyo3` on the mappers: a Python class per mapper with the same
  methods, run on a runtime a generated `Database` holds with the GIL
  released; input types get a constructor with required columns
  positional and the rest keyword-only; errors arrive as `ProtoError`.
  ([#7])
- Composite types become generated structs, nested and as arrays,
  filed with the enums, with `composite_derives` in the config. An
  unmapped type's warning names the extension it came from and the
  `[generate.types]` line to add. ([#8])
- `find_where` and `count_where` on every mapper over a `Query` built
  at run time, generated into `query.rs` beside the mappers; from
  Python, a dict with `__lt`-style suffixes, `None` as `IS NULL`, a
  list as `= ANY`, and `order_by`, `limit`, `offset`. ([#15])
- The operation module, with `Data`: one request dict naming a table
  and an operation, routed through one generated function to the
  mapper that serves it. `--operations --operation-dir`, with
  `--mappers` and `--pyo3`. ([#16])

### Changed

- A unique index's `INCLUDE` columns are no longer part of the key, so
  the finder takes the key alone. ([#11])
- A regeneration takes back a method `proto` owns that the schema no
  longer calls for, an import nothing uses, and a `pub mod` for a
  pruned table; a new field arrives with its attributes and doc
  comment. ([#6])
- A table whose module or class would be one `proto` writes itself —
  `query`, `python`, `Data` — is refused before anything is written.
  ([#15], [#16])
- The generated code, Python side included, is held to
  `clippy -D warnings` in CI.

### Library API

- `Column` gained `extension`; `Model` gained `composites`; `Table`
  gained `children`; `render::Opts` gained `bridge_path`, `query_path`
  and `mapper_path`. Anything building these by hand adds the fields.

## [0.1.0] — 2026-09-08

First release. `proto` reads a live PostgreSQL schema and writes the
Rust models and mappers for it, then keeps them in step as the schema
moves, correcting files rather than overwriting them.

- Models and mappers written against `sqlx`, with the SQL embedded or,
  with `--sql server`, moved into Postgres functions written as a
  migration alongside your own.
- `--pyo3` makes the models Python-compatible behind a Cargo feature.
- Output to stdout by default; `--out-dir` for files; `--check` for
  drift; `--prune` to take back models for tables that are gone.
- Identifiers quoted and values bound, held by `tests/injection.rs` and
  the round trip in CI against PostgreSQL 16 and 17.

[Unreleased]: https://github.com/iridiumdesign/iridium-proto/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/iridiumdesign/iridium-proto/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/iridiumdesign/iridium-proto/releases/tag/v0.1.0
[#5]: https://github.com/iridiumdesign/iridium-proto/pull/5
[#6]: https://github.com/iridiumdesign/iridium-proto/pull/6
[#7]: https://github.com/iridiumdesign/iridium-proto/pull/7
[#8]: https://github.com/iridiumdesign/iridium-proto/pull/8
[#11]: https://github.com/iridiumdesign/iridium-proto/pull/11
[#13]: https://github.com/iridiumdesign/iridium-proto/pull/13
[#15]: https://github.com/iridiumdesign/iridium-proto/pull/15
[#16]: https://github.com/iridiumdesign/iridium-proto/pull/16
[#17]: https://github.com/iridiumdesign/iridium-proto/issues/17
[#19]: https://github.com/iridiumdesign/iridium-proto/issues/19
[#21]: https://github.com/iridiumdesign/iridium-proto/issues/21
[#24]: https://github.com/iridiumdesign/iridium-proto/issues/24
[#25]: https://github.com/iridiumdesign/iridium-proto/issues/25
