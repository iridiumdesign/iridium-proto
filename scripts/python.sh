#!/usr/bin/env sh
#
# Python interop for the --pyo3 output.
#
# `cargo check --features python` proves the generated models compile
# against pyo3. It does not prove anyone can use them: that needs a real
# extension module, imported by a real interpreter, with the fields read
# and written from Python. This builds one and does that.
#
#   ./scripts/python.sh [target]
#
# `target` is a database target from the proto config. Omitted, proto's
# own default is used — `default_db`, or the only database defined. Set
# PROTO_CONFIG to point at a config other than the default.

set -eu

TARGET="${1:-}"
SCHEMA="proto_python"
WORK="$(mktemp -d)"
PROTO="${CARGO_TARGET_DIR:-target}/debug/proto"

cleanup() {
    if [ -n "${PSQL_DB:-}" ]; then
        psql -q -c "DROP SCHEMA IF EXISTS $SCHEMA CASCADE;" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

# Which target to use when the caller named none: whichever proto would
# have picked on its own, rather than a name invented here.
default_target() {
    if [ -n "$1" ]; then
        printf '%s' "$1"
        return
    fi
    chosen=$("$PROTO" config | awk '$1 == "default:" { print $2 }')
    if [ -z "$chosen" ] || [ "$chosen" = "(none)" ]; then
        echo "  no target given and no default in the proto config" >&2
        echo "  set default_db, define one database, or pass a target" >&2
        exit 1
    fi
    printf '%s' "$chosen"
}

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

say "building proto"
cargo build --quiet

TARGET=$(default_target "$TARGET")
WHERE=$("$PROTO" config | awk -v t="$TARGET" '$1 == t && $2 == "->" { print $3 }')
if [ -z "$WHERE" ]; then
    echo "  no target '$TARGET' in the proto config" >&2
    exit 1
fi
PSQL_HOST="${WHERE%%/*}"
PSQL_DB="${WHERE#*/}"
# NOTICE is what `DROP ... IF EXISTS` says about a first run, and what
# CASCADE says about doing its job. Neither is news; warnings still are.
psql() {
    PGOPTIONS='-c client_min_messages=warning' \
        command psql -h "$PSQL_HOST" -d "$PSQL_DB" "$@"
}

say "creating $SCHEMA in $PSQL_HOST/$PSQL_DB"
# One table, spread across the conversions that have to hold: a uuid, an
# enum, a decimal, an array, a timestamp, and a nullable of each shape.
psql -q -v ON_ERROR_STOP=1 <<SQL
DROP SCHEMA IF EXISTS $SCHEMA CASCADE;
CREATE SCHEMA $SCHEMA;
CREATE TYPE $SCHEMA.item_status AS ENUM ('draft', 'active', 'retired');
CREATE TABLE $SCHEMA.item (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug       text NOT NULL,
    status     $SCHEMA.item_status NOT NULL DEFAULT 'draft',
    price      numeric(10,2),
    tags       text[],
    count      integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now()
);
SQL

say "generating models with --pyo3 and a #[pymodule]"
mkdir -p "$WORK/src/model" "$WORK/.cargo"
"$PROTO" --db "$TARGET" schema "$SCHEMA" --pyo3 --pymodule protopy \
    --out-dir "$WORK/src/model"

say "building an extension module from them"
cat > "$WORK/Cargo.toml" <<'TOML'
[package]
name = "protopy"
version = "0.0.0"
edition = "2024"

[lib]
name = "protopy_test"
crate-type = ["cdylib"]

[features]
python = ["pyo3/chrono", "pyo3/uuid", "pyo3/rust_decimal"]

[dependencies]
chrono = { version = "0.4", features = ["serde"] }
pyo3 = { version = "0.26", features = ["extension-module"] }
rust_decimal = { version = "1", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
sqlx = { version = "0.8", features = [
    "runtime-tokio", "postgres", "chrono", "uuid", "rust_decimal",
] }
uuid = { version = "1", features = ["serde"] }
TOML

# An extension module leaves libpython to the interpreter that loads it.
# Linux resolves that by default; macOS has to be told.
cat > "$WORK/.cargo/config.toml" <<'TOML'
[target.aarch64-apple-darwin]
rustflags = ["-C", "link-arg=-undefined", "-C", "link-arg=dynamic_lookup"]

[target.x86_64-apple-darwin]
rustflags = ["-C", "link-arg=-undefined", "-C", "link-arg=dynamic_lookup"]
TOML

# What a consumer writes when the extension needs functions of its own:
# their own module, calling the generated `register` rather than listing
# the classes by hand. `sample` stands in for a mapper handing an object
# back after a query, since generated structs have no constructor.
#
# The generated `#[pymodule] protopy` is compiled too, and is what a
# consumer who needs nothing else would import directly.
cat > "$WORK/src/lib.rs" <<'RUST'
use pyo3::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;

pub mod model;
use model::item::{Item, ItemStatus};

#[pyfunction]
fn sample() -> Item {
    Item {
        id: uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000ff").unwrap(),
        slug: "widget".to_string(),
        status: ItemStatus::Active,
        price: Some(Decimal::from_str("19.99").unwrap()),
        tags: Some(vec!["a".to_string(), "b".to_string()]),
        count: 7,
        created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    }
}

#[pymodule]
fn protopy_test(m: &Bound<'_, PyModule>) -> PyResult<()> {
    model::python::register(m)?;
    m.add_function(wrap_pyfunction!(sample, m)?)?;
    Ok(())
}
RUST

(cd "$WORK" && cargo build --quiet --features python)

# Name the artifact what this interpreter will import. Where cargo put
# it depends on CARGO_TARGET_DIR, which CI sets so the build is cached;
# what it is called depends on the platform.
SUFFIX=$(python3 -c 'import sysconfig; print(sysconfig.get_config_var("EXT_SUFFIX"))')
BUILT=""
for candidate in \
    "${CARGO_TARGET_DIR:-$WORK/target}/debug/libprotopy_test.dylib" \
    "${CARGO_TARGET_DIR:-$WORK/target}/debug/libprotopy_test.so"
do
    [ -f "$candidate" ] && BUILT="$candidate" && break
done
if [ -z "$BUILT" ]; then
    echo "  built no extension module under ${CARGO_TARGET_DIR:-$WORK/target}" >&2
    exit 1
fi
cp "$BUILT" "$WORK/protopy_test$SUFFIX"

say "importing it and using the object"
(cd "$WORK" && python3 - <<'PY'
import datetime
import decimal
import uuid

import protopy_test as protopy

# Registered by the generated `register`, not by hand.
assert hasattr(protopy, "Item") and hasattr(protopy, "ItemStatus")
print("  classes registered by the generated module")

it = protopy.sample()

# Every column type has to arrive as the native Python thing, not a
# stringified stand-in.
assert isinstance(it.id, uuid.UUID), type(it.id)
assert isinstance(it.slug, str), type(it.slug)
assert isinstance(it.status, protopy.ItemStatus), type(it.status)
assert isinstance(it.price, decimal.Decimal), type(it.price)
assert isinstance(it.tags, list), type(it.tags)
assert isinstance(it.count, int), type(it.count)
assert isinstance(it.created_at, datetime.datetime), type(it.created_at)
assert it.created_at.tzinfo is not None, "timestamptz must stay aware"
print("  reads   ok")

# set_all has to work, and has to round trip.
it.slug = "changed"
it.count = 42
it.price = decimal.Decimal("1.50")
it.tags = ["x"]
it.status = protopy.ItemStatus.Retired
it.id = uuid.UUID("11111111-1111-1111-1111-111111111111")
assert it.slug == "changed"
assert it.count == 42
assert it.price == decimal.Decimal("1.50")
assert it.tags == ["x"]
assert it.status == protopy.ItemStatus.Retired
assert it.id == uuid.UUID("11111111-1111-1111-1111-111111111111")
print("  writes  ok")

# A nullable column takes None; the enum compares by identity and by int,
# which is what pyclass(eq, eq_int) is for.
it.price = None
it.tags = None
assert it.price is None and it.tags is None
assert protopy.ItemStatus.Draft == protopy.ItemStatus.Draft
assert protopy.ItemStatus.Draft != protopy.ItemStatus.Active
assert int(protopy.ItemStatus.Draft) == 0
assert int(protopy.ItemStatus.Retired) == 2
print("  nulls and enums ok")

# The types are enforced rather than coerced, and a NOT NULL column
# refuses None — the schema's guarantees survive the crossing.
for attr, bad in (("count", "not an int"), ("slug", None), ("tags", 3)):
    try:
        setattr(it, attr, bad)
    except TypeError:
        pass
    else:
        raise AssertionError(f"{attr} accepted {bad!r}")
print("  typing  enforced")
PY
)

say "ok"
