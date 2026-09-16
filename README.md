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
make it Python-compatible: the models, the mappers, a query over them, and
an operation that routes a request to the right mapper. This is another
opinion, based on how my teams are currently writing software. There is a lot
of Python remaining at the edges, with the core moving to the performance and
safety of Rust.

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
alone. [More on what is and is not reconciled](docs/editing-generated-code.md).

## Install

```
cargo install iridium-proto
```

The binary is `proto`. The library is `iridium-proto`, so the same work
can be done from a build script or a test; the crate docs on docs.rs
show the library, and the pages below show the tool.

## Reading on

The reference is a page per subject under `docs/`:

- [Configuration and commands](docs/configuration.md) — the config
  file, targets and credentials, every command and flag.
- [Models](docs/models.md) — what a table, an enum and a composite
  become; the insert input and the patch.
- [Mappers](docs/mappers.md) — the repository per table, `find_where`,
  and where the SQL lives: embedded, or in Postgres functions with a
  migration to match.
- [Children](docs/children.md) — the parent's side of a foreign key:
  the field, the loaders, and `set_id_on_children`.
- [Operations](docs/operations.md) — `Data`: one request in, routed by
  table name, the table's own types out; and the same from Python.
- [Type mapping and dependencies](docs/type-mapping.md) — Postgres to
  Rust, overrides, and what `proto` adds to `Cargo.toml`.
- [Keeping in step](docs/keeping-in-step.md) — what a run reports,
  `--check`, and `--prune`.
- [Editing generated code](docs/editing-generated-code.md) — what a
  regeneration corrects, what it leaves alone, and how to take a file
  over.
- [Safety](docs/safety.md) — the SQL it writes, credentials, files, and
  what it does not protect you from.
- [pyo3](docs/pyo3.md) — Python-compatible models, mappers and
  operations behind a Cargo feature.

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
