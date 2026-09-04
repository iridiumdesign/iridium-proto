# Contributing

Thanks for looking. I maintain this crate on my own time, so the ground
rules are short.

## Before you write code

**Open an issue first, or comment on an existing one.** Say what you
intend to change and roughly how. This costs you a few minutes and saves
us both the situation where I decline a patch you already finished.

Issues labeled *good first issue* are the easiest place to start, but
comment before you do — the label marks the size of the work, not an
open invitation. Anything already assigned to you needs no further
ceremony.

An open issue is not a claim. If a ticket has sat untouched for months,
that means I haven't gotten to it, not that it's waiting for a stranger
to close it.

## Unsolicited agent-generated pull requests

**I don't accept them.** If a coding agent wrote the patch and you are
opening the PR without having discussed the work with me first, I will
close it unreviewed.

This is not a position on the tools. It's a position on the cost. A
patch takes minutes to generate and an hour to review properly. This
crate writes code that someone else then compiles, so a plausible-looking
diff is worth very little: the output has to build, the SQL has to run
against a real server, and both mapper backends have to keep emitting the
same API. Review is the scarce thing here, and I'd rather spend it on
work someone has actually run.

Concretely, a PR gets closed on sight if it:

- arrives against an issue you never commented on
- was written by an agent working from the issue text alone
- says the tests couldn't be run in your environment

That last one is the tell. **If you didn't compile it, don't send it.**
For this crate that means twice over: your change has to compile, and so
does what it emits.

Use whatever tools you like on work we've agreed on. Bring the same
judgment to the output that you'd bring to your own.

## If you do send a patch

The repository has a `justfile`. `just check` runs formatting, clippy
with warnings as errors, the tests, and rustdoc with warnings promoted.
None of it needs a database. `just check-all` adds the MSRV build and
the round trip against a live server, which is what CI runs — so
`check-all`, not `check`, is what matches a green CI.

```sh
cargo install --locked just   # once
just                          # list every recipe
just check                    # the fast gate, no database
just check-all                # the gate CI actually runs
```

`check` leaves out MSRV, which downloads a toolchain, and the round
trip, which needs a server. That is a fine trade while you iterate and a
bad one right before you open a pull request.

`just fix` reformats in place.

There is a pre-commit hook that runs the two fast gates. Enable it once
per clone:

```sh
git config core.hooksPath .githooks
```

If you'd rather not install anything, the same gate by hand:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

## The round trip

`just smoke` is the gate that matters, and the only one that needs a
PostgreSQL server. It creates a scratch schema covering the cases that
decide output — a server-owned key, a literal default, a unique
constraint, a foreign key, an enum type, an array, a nullable column —
generates models, mappers and functions from it, compiles the result as
its own crate with and without the pyo3 feature, applies the functions,
exercises every one of them, and checks that a rerun writes no duplicate
migration. Then it rolls back and drops the schema.

```sh
./scripts/smoke.sh <target>          # a target from your proto config
PROTO_SMOKE_DB=<target> just smoke
```

CI runs it against PostgreSQL 16 and 17. If you touch the renderers or
the catalog queries, run it before you send the patch — the unit tests
prove proto emits the text it means to, and only this proves the text is
real.

The round trip also covers the update cycle: it changes the schema under
a generated tree, checks that `--check` notices and names the new column,
regenerates, and checks the tree goes quiet again. Then it drops a table
and checks `--prune` takes back the model without touching a
hand-written file beside it.

`just python` is the same argument for the `--pyo3` output: it builds an
extension module from generated models and drives it from an
interpreter. Run it if you touch anything pyo3.

## What a change needs

New behavior comes with a test. The renderers run off a fixture in
`src/render/fixture.rs`, so most of them need no database: extend the
fixture table rather than reaching for a connection.

A change to what gets emitted needs the round trip run, and a change to
the catalog queries needs it run against more than one server version if
you are relying on something recent.

The MSRV in `Cargo.toml` still has to hold. It is a promise to
consumers, and it is a CI gate.

Claims about what PostgreSQL does need a citation from the PostgreSQL
documentation — the system catalog chapter, or the reference page for
the statement in question. Claims about what sqlx decodes need a
citation from sqlx, or a test. "It seemed to work" is not one; a lot of
this behavior is version-dependent and quietly so.

The CI workflow runs all of this. First-time contributors need me to
approve the run manually, so there may be a wait.

## Reporting bugs

Generated code that doesn't compile, SQL that doesn't run, a Postgres
type that maps to the wrong Rust type, a schema shape proto gets wrong —
open an issue. Include the `CREATE TABLE` (or enough of it to
reproduce), your server version, what proto emitted, and what you
expected. A schema I can paste into psql is the most useful report I
get.

Security problems go through [SECURITY.md](SECURITY.md) instead, not the
public tracker.

## License

This crate is dual licensed under the MIT license and the Apache License,
Version 2.0, at the user's option. Unless you state otherwise, anything you
submit for inclusion is dual licensed the same way, with no additional
terms. There is no CLA to sign.
