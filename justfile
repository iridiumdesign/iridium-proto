# iridium-proto — the build.
#
# Everything in the `gates` group is what CI runs. If they pass here, they
# pass there; if the two ever disagree, the workflow is the authority and
# this file is the bug.

# The floor in Cargo.toml. Named once so `just msrv` and the rust-version
# field cannot drift apart silently. Let chains put it at 1.88.
MSRV := "1.88"

# Database the live gates run against. Any target in the proto config;
# empty means whichever one proto would pick on its own.
DB := env("PROTO_SMOKE_DB", "")

# List the recipes. Hidden: `just` runs it, nobody types it.
[private]
default:
    @just --list --unsorted

# ── build ──────────────────────────────────────────────────────────

[doc('Build the library and the proto binary.')]
[group('build')]
build:
    cargo build

[doc('Build the rustdoc and open it.')]
[group('build')]
doc:
    cargo doc --no-deps --open

[doc('Install proto into ~/.cargo/bin.')]
[group('build')]
install:
    cargo install --path . --locked

# ── inspect ────────────────────────────────────────────────────────

[doc('What ships in the published crate.')]
[group('inspect')]
manifest:
    cargo package --list --allow-dirty

[doc('The command-line surface, as a reader sees it.')]
[group('inspect')]
help:
    cargo run --quiet -- --help

[doc('Resolved config and its targets.')]
[group('inspect')]
targets:
    cargo run --quiet -- config

# ── gates ──────────────────────────────────────────────────────────

[doc('Formatting.')]
[group('gates')]
fmt:
    cargo fmt --all -- --check

[doc('Reformat in place.')]
[group('gates')]
fix:
    cargo fmt --all

[doc('Lints. Warnings are errors, same as CI.')]
[group('gates')]
clippy:
    cargo clippy --all-targets -- -D warnings

[doc('Tests. No database needed — the renderers run off a fixture.')]
[group('gates')]
test:
    cargo test

# A broken intra-doc link is only a warning by default, so docs.rs
# renders it and nobody notices. This promotes it, as CI does.

[doc('Rustdoc with warnings promoted to errors.')]
[group('gates')]
doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

[doc('Build on the MSRV floor.')]
[group('gates')]
msrv:
    rustup toolchain install {{ MSRV }} --profile minimal
    cargo +{{ MSRV }} build

[doc('The fast gate: fmt, clippy, tests, docs. No database.')]
[group('gates')]
check: fmt clippy test doc-check

[doc('Everything: MSRV, the round trip, and Python interop.')]
[group('gates')]
check-all: check msrv smoke python

# ── live ───────────────────────────────────────────────────────────

# The unit tests prove proto emits the text it means to. Only a database
# proves that text compiles and runs, so this generates a scratch schema,
# builds the output as a real crate, applies the functions, exercises
# them, and rolls the whole thing back.

[doc('End-to-end round trip against a live database.')]
[group('live')]
smoke:
    ./scripts/smoke.sh {{ DB }}

# `cargo check --features python` proves the pyo3 output compiles. Only
# an interpreter proves anyone can use it, so this builds a real
# extension module and reads and writes the fields from Python.

[doc('Python interop for the --pyo3 output.')]
[group('live')]
python:
    ./scripts/python.sh {{ DB }}

# ── tidy ───────────────────────────────────────────────────────────

[doc('Remove build artifacts.')]
[group('tidy')]
clean:
    cargo clean
