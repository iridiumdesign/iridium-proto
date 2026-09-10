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
        psql -q -c "DROP SCHEMA IF EXISTS ${SCHEMA}_2 CASCADE;" >/dev/null 2>&1 || true
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
CREATE TYPE $SCHEMA.dimensions AS (width numeric, unit text);
CREATE TABLE $SCHEMA.item (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug       text NOT NULL,
    status     $SCHEMA.item_status NOT NULL DEFAULT 'draft',
    price      numeric(10,2),
    tags       text[],
    count      integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    parent_id  uuid REFERENCES $SCHEMA.item(id),
    size       $SCHEMA.dimensions
);
-- A second schema with a table of the same name, for the database run
-- below: two ItemMappers that must land in different submodules.
DROP SCHEMA IF EXISTS ${SCHEMA}_2 CASCADE;
CREATE SCHEMA ${SCHEMA}_2;
CREATE TABLE ${SCHEMA}_2.item (
    id   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug text NOT NULL
);
SQL

say "generating models and mappers with --pyo3 and a #[pymodule]"
mkdir -p "$WORK/src/model" "$WORK/src/mapper" "$WORK/.cargo"
# An extension crate declares pyo3 itself, not optional, because the
# whole crate is the extension. proto sees that and writes a feature
# that lists the conversions only, with no `dep:pyo3`. Everything else
# the generated code needs, it adds — tokio included, since the mappers'
# Python classes run on a runtime the generated Database holds.
cat > "$WORK/Cargo.toml" <<'TOML'
[package]
name = "protopy"
version = "0.0.0"
edition = "2024"

[lib]
name = "protopy_test"
crate-type = ["cdylib"]

[dependencies]
pyo3 = { version = "0.26", features = ["extension-module"] }
TOML
"$PROTO" --db "$TARGET" schema "$SCHEMA" --pyo3 --pymodule protopy \
    --mappers --out-dir "$WORK/src/model" --mapper-dir "$WORK/src/mapper"
test -f "$WORK/src/mapper/python.rs" || {
    echo "  no bridge written beside the mappers" >&2
    exit 1
}
for needed in 'python = ["pyo3/chrono", "pyo3/uuid", "pyo3/rust_decimal"]' \
    'tokio = {'; do
    grep -qF "$needed" "$WORK/Cargo.toml" || {
        echo "  Cargo.toml is missing: $needed" >&2
        cat "$WORK/Cargo.toml" >&2
        exit 1
    }
done

say "building an extension module from them"

# An extension module leaves libpython to the interpreter that loads it.
# Linux resolves that by default; macOS has to be told.
cat > "$WORK/.cargo/config.toml" <<'TOML'
[target.aarch64-apple-darwin]
rustflags = ["-C", "link-arg=-undefined", "-C", "link-arg=dynamic_lookup"]

[target.x86_64-apple-darwin]
rustflags = ["-C", "link-arg=-undefined", "-C", "link-arg=dynamic_lookup"]
TOML

# What a consumer writes when the extension needs functions of its own:
# their own module, calling the generated `register`s rather than listing
# the classes by hand — the models' first, since the mappers hand those
# back. `sample` builds a row in Rust, since row structs have no
# constructor: they come from the database.
#
# The generated `#[pymodule] protopy` is compiled too, and is what a
# consumer who needs nothing else would import directly.
cat > "$WORK/src/lib.rs" <<'RUST'
use pyo3::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;

pub mod mapper;
pub mod model;
use model::item::{Dimensions, Item, ItemStatus};

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
        parent_id: None,
        children: Vec::new(),
        size: Some(Dimensions {
            width: Some(Decimal::from_str("2.5").unwrap()),
            unit: Some("cm".to_string()),
        }),
    }
}

#[pymodule]
fn protopy_test(m: &Bound<'_, PyModule>) -> PyResult<()> {
    model::python::register(m)?;
    mapper::python::register(m)?;
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
# The same defaults libpq uses, so this works wherever psql already does.
DATABASE_URL="postgres://${PGUSER:-$(id -un)}${PGPASSWORD:+:$PGPASSWORD}@$PSQL_HOST/$PSQL_DB"
(cd "$WORK" && PROTO_TEST_URL="$DATABASE_URL" python3 - <<'PY'
import datetime
import decimal
import os
import uuid

import protopy_test as protopy

# Registered by the generated `register`s, not by hand — the composite
# type included.
assert hasattr(protopy, "Item") and hasattr(protopy, "ItemStatus")
assert hasattr(protopy, "NewItem") and hasattr(protopy, "ItemMapper")
assert hasattr(protopy, "Database") and hasattr(protopy, "ProtoError")
assert hasattr(protopy, "Dimensions")
print("  classes registered by the generated modules")

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
# The parent's side of the tree crosses as a list of the same class.
assert it.parent_id is None
assert it.children == [], it.children
# A composite crosses as an object of its own class, fields and all.
assert isinstance(it.size, protopy.Dimensions), type(it.size)
assert it.size.width == decimal.Decimal("2.5")
assert it.size.unit == "cm"
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
child = protopy.sample()
child.parent_id = it.id
it.children = [child]
assert len(it.children) == 1 and it.children[0].slug == "widget"
assert it.children[0].parent_id == it.id
print("  writes  ok")

# A nullable column takes None; the enum compares by identity and by int,
# which is what pyclass(eq, eq_int) is for.
it.price = None
it.tags = None
assert it.price is None and it.tags is None
# A composite has no constructor on the Python side, but one read from a
# row can be edited and put back, and the column can be cleared.
size = it.size
size.unit = "mm"
it.size = size
assert it.size.unit == "mm"
it.size = None
assert it.size is None
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

# The mapper, from Python: the whole round trip through the generated
# class, on a real connection, with the rows coming back as the classes
# registered above.
db = protopy.Database(os.environ["PROTO_TEST_URL"])
items = protopy.ItemMapper(db)

# Required columns are positional; the defaulted and nullable ones are
# keyword-only, and leaving them out leaves them to the database.
made = items.create(protopy.NewItem("widget", price=decimal.Decimal("9.99")))
assert isinstance(made, protopy.Item), type(made)
assert made.slug == "widget"
assert made.status == protopy.ItemStatus.Draft, "the column default"
assert made.count == 0, "the column default"
assert made.price == decimal.Decimal("9.99")
assert made.created_at.tzinfo is not None, "the server filled this in"
print("  create  ok")

found = items.find_by_id(made.id)
assert found is not None and found.id == made.id
assert items.find_by_id(uuid.uuid4()) is None, "a miss is None, not an error"
assert any(i.id == made.id for i in items.list())
print("  find    ok")

found.slug = "changed"
found.status = protopy.ItemStatus.Active
found.tags = ["x", "y"]
updated = items.update(found)
assert updated.slug == "changed"
assert updated.status == protopy.ItemStatus.Active
assert updated.tags == ["x", "y"]
print("  update  ok")

items.delete(made.id)
assert items.find_by_id(made.id) is None
print("  delete  ok")

# A statement that fails is an exception, with the driver's message.
try:
    items.update(protopy.sample())  # an id that is not in the table
except protopy.ProtoError as e:
    assert "no rows" in str(e), str(e)
else:
    raise AssertionError("update of a missing row did not raise")
try:
    protopy.Database("postgres://nobody@localhost:1/nowhere")
except protopy.ProtoError:
    pass
else:
    raise AssertionError("a bad connection did not raise")
print("  errors  arrive as ProtoError")
PY
)

say "a database run: one bridge at the root, mappers by schema"
# Not compiled here — that would build every schema in the database —
# but the layout is what the compile would stand or fall on: exactly
# one bridge, declared once, and each schema's mapper classes on its
# own submodule so two `item` tables do not collide.
mkdir -p "$WORK/db/model" "$WORK/db/mapper"
"$PROTO" --db "$TARGET" database --pyo3 --pymodule store --mappers \
    --no-manifest --out-dir "$WORK/db/model" --mapper-dir "$WORK/db/mapper" \
    >/dev/null 2>&1
test -f "$WORK/db/mapper/python.rs" || {
    echo "  no bridge at the mapper root" >&2
    exit 1
}
for schema in "$SCHEMA" "${SCHEMA}_2"; do
    if [ -f "$WORK/db/mapper/$schema/python.rs" ]; then
        echo "  a per-schema bridge was written for $schema" >&2
        exit 1
    fi
    if grep -q "pub mod python" "$WORK/db/mapper/$schema/mod.rs"; then
        echo "  $schema's mapper mod.rs declares a bridge it does not have" >&2
        exit 1
    fi
    grep -q "fn $schema(parent: &Bound<'_, PyModule>)" "$WORK/db/mapper/python.rs" || {
        echo "  the bridge has no submodule for $schema" >&2
        exit 1
    }
    grep -q "child.add_class::<super::$schema::item::PyItemMapper>()?;" \
        "$WORK/db/mapper/python.rs" || {
        echo "  $schema's ItemMapper is not registered on its submodule" >&2
        exit 1
    }
done
grep -q "pub mod python" "$WORK/db/mapper/mod.rs" || {
    echo "  the root mapper mod.rs does not declare the bridge" >&2
    exit 1
}
echo "  one bridge, two submodules, two ItemMappers"
# The composite belongs to the first schema alone: filed in its own
# enums.rs, registered on its own submodule, and absent from the other.
grep -q "pub struct Dimensions" "$WORK/db/model/$SCHEMA/enums.rs" || {
    echo "  $SCHEMA's enums.rs does not hold the composite" >&2
    exit 1
}
grep -q "child.add_class::<super::$SCHEMA::enums::Dimensions>()?;" \
    "$WORK/db/model/python.rs" || {
    echo "  Dimensions is not registered on $SCHEMA's submodule" >&2
    exit 1
}
if [ -f "$WORK/db/model/${SCHEMA}_2/enums.rs" ]; then
    echo "  ${SCHEMA}_2 got an enums.rs with nothing to put in it" >&2
    exit 1
fi
echo "  the composite is filed and registered under its own schema"

say "ok"
