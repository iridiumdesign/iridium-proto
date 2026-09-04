#!/usr/bin/env sh
#
# End-to-end round trip for proto, against a live PostgreSQL database.
#
# Unit tests prove proto emits the text it means to. This proves the text
# is real: it creates a scratch schema covering the cases that decide
# output, generates models, mappers and functions from it, compiles the
# result as its own crate, applies the functions and exercises them, then
# rolls back and drops the schema.
#
#   ./scripts/smoke.sh [target]
#
# `target` is a database target from the proto config; default `dev`.
# Set PROTO_CONFIG to point at a config other than the default.

set -eu

TARGET="${1:-dev}"
SCHEMA="proto_smoke"
WORK="$(mktemp -d)"
PROTO="${CARGO_TARGET_DIR:-target}/debug/proto"

cleanup() {
    if [ -n "${PSQL_DB:-}" ]; then
        psql -q -c "DROP SCHEMA IF EXISTS $SCHEMA CASCADE;" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

# proto reads its own config; psql has to be told separately. Ask proto
# where the target points rather than duplicating the config here.
resolve() {
    "$PROTO" config | awk -v t="$TARGET" '$1 == t && $2 == "->" { print $3 }'
}

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

say "building proto"
cargo build --quiet

WHERE="$(resolve)"
if [ -z "$WHERE" ]; then
    echo "  no target '$TARGET' in the proto config" >&2
    echo "  set PROTO_CONFIG, or pass a target: ./scripts/smoke.sh <target>" >&2
    exit 1
fi
PSQL_HOST="${WHERE%%/*}"
PSQL_DB="${WHERE#*/}"
psql() { command psql -h "$PSQL_HOST" -d "$PSQL_DB" "$@"; }

say "creating $SCHEMA in $PSQL_HOST/$PSQL_DB"
psql -q -v ON_ERROR_STOP=1 <<SQL
DROP SCHEMA IF EXISTS $SCHEMA CASCADE;
CREATE SCHEMA $SCHEMA;
CREATE TYPE $SCHEMA.item_status AS ENUM ('draft', 'active', 'retired');
CREATE TABLE $SCHEMA.bin (
    id   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code text NOT NULL UNIQUE
);
CREATE TABLE $SCHEMA.item (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug       text NOT NULL UNIQUE,
    name       text NOT NULL,
    status     $SCHEMA.item_status NOT NULL DEFAULT 'draft',
    price      numeric(10,2),
    tags       text[],
    bin_id     uuid NOT NULL REFERENCES $SCHEMA.bin(id),
    created_at timestamptz NOT NULL DEFAULT now()
);
COMMENT ON TABLE $SCHEMA.item IS 'A thing on a shelf.';
COMMENT ON COLUMN $SCHEMA.item.slug IS 'Stable external identifier.';

-- A table whose name is a SQL injection payload, and a neighbour for it
-- to destroy if the quoting ever stops holding.
CREATE TABLE $SCHEMA.keep_me (id integer PRIMARY KEY);
INSERT INTO $SCHEMA.keep_me VALUES (1);
CREATE TABLE $SCHEMA."evil""; DROP TABLE keep_me; --" (
    id   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    note text NOT NULL
);

-- Nothing here is expressible in unquoted SQL: the relation is a
-- reserved word, so are two columns, and one would fold if written bare.
CREATE TABLE $SCHEMA."order" (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    "select"     text NOT NULL UNIQUE,
    "Mixed Case" integer,
    "desc"       text,
    bin_id       uuid NOT NULL REFERENCES $SCHEMA.bin(id)
);
SQL

mkdir -p "$WORK/src/model" "$WORK/src/mapper" "$WORK/migrations"

say "generating models, mappers and functions"
"$PROTO" --db "$TARGET" --sql server --migrations-dir "$WORK/migrations" \
    schema "$SCHEMA" --pyo3 --mappers \
    --out-dir "$WORK/src/model" --mapper-dir "$WORK/src/mapper"

say "compiling the generated crate, and linting it as hard as this one"
cat > "$WORK/Cargo.toml" <<'TOML'
[package]
name = "proto-smoke"
version = "0.0.0"
edition = "2024"

[features]
python = ["dep:pyo3", "pyo3/chrono", "pyo3/uuid", "pyo3/rust_decimal"]

[dependencies]
chrono = { version = "0.4", features = ["serde"] }
pyo3 = { version = "0.26", optional = true }
rust_decimal = { version = "1", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", features = [
    "runtime-tokio", "postgres", "chrono", "uuid", "rust_decimal", "json",
] }
uuid = { version = "1", features = ["serde"] }
TOML
printf 'pub mod mapper;\npub mod model;\n' > "$WORK/src/lib.rs"
# Generated code is held to the same bar as the code that writes it:
# whatever proto emits has to survive `-D warnings` in someone else's
# crate, or it is proto handing them a lint to clean up.
(
    cd "$WORK"
    cargo check --quiet
    cargo check --quiet --features python
    cargo clippy --quiet --all-targets -- -D warnings
)

say "applying the functions and exercising them"
# psql's \i takes a literal path, so the shell resolves the version
# numbers proto chose.
BIN_SQL=$(ls "$WORK"/migrations/*_"${SCHEMA}"_bin_crud.sql)
ITEM_SQL=$(ls "$WORK"/migrations/*_"${SCHEMA}"_item_crud.sql)
ORDER_SQL=$(ls "$WORK"/migrations/*_"${SCHEMA}"_order_crud.sql)
EVIL_SQL=$(ls "$WORK"/migrations/*_"${SCHEMA}"_evil*_crud.sql)
psql -q -v ON_ERROR_STOP=1 <<SQL
BEGIN;
\\i $BIN_SQL
\\i $ITEM_SQL
\\i $ORDER_SQL

SELECT id AS bin_id FROM $SCHEMA.bin_insert('B1') \\gset
-- A NULL status must fall through COALESCE to the column default.
SELECT id AS item_id
  FROM $SCHEMA.item_insert('widget', 'Widget', NULL, 9.99,
                           ARRAY['a','b'], :'bin_id') \\gset

\\echo '  read'
SELECT slug, status, price, tags FROM $SCHEMA.item_get(:'item_id');
SELECT count(*) AS by_slug FROM $SCHEMA.item_by_slug('widget');
SELECT count(*) AS by_bin  FROM $SCHEMA.item_by_bin_id(:'bin_id');
SELECT count(*) AS listed  FROM $SCHEMA.item_list();

\\echo '  update'
SELECT name, status, price
  FROM $SCHEMA.item_update(:'item_id', 'widget', 'Widget 2', 'active',
                           19.99, ARRAY['c'], :'bin_id');

\\echo '  reserved words and folding columns'
SELECT id AS order_id
  FROM $SCHEMA.order_insert('first', 42, 'a note', :'bin_id') \\gset
SELECT "select", "Mixed Case", "desc" FROM $SCHEMA.order_get(:'order_id');
SELECT count(*) AS by_select FROM $SCHEMA.order_by_select('first');
SELECT count(*) AS ordered FROM $SCHEMA.order_list();
SELECT "select" FROM $SCHEMA.order_update(:'order_id', 'second', 43,
                                          'another note', :'bin_id');
SELECT $SCHEMA.order_delete(:'order_id');

\\echo '  a table named like an injection payload'
-- Applying this at all is the test: if the name escaped its quoting,
-- the DDL would carry a DROP with it.
\\i $EVIL_SQL
SELECT count(*) AS functions
  FROM pg_proc p
  JOIN pg_namespace n ON n.oid = p.pronamespace
 WHERE n.nspname = '$SCHEMA' AND p.proname LIKE 'evil%';
DO \$\$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM $SCHEMA.keep_me) THEN
        RAISE EXCEPTION 'keep_me was dropped: the payload executed';
    END IF;
END \$\$;
\\echo '  keep_me survived'

\\echo '  delete'
SELECT $SCHEMA.item_delete(:'item_id');
SELECT count(*) AS remaining FROM $SCHEMA.item_list();
ROLLBACK;
SQL

say "regenerating: the migrations should be left alone"
"$PROTO" --db "$TARGET" --sql server --migrations-dir "$WORK/migrations" \
    schema "$SCHEMA" --mappers \
    --out-dir "$WORK/src/model" --mapper-dir "$WORK/src/mapper" 2>&1 \
    | grep -q 'unchanged, kept' \
    || { echo "  a rerun wrote a duplicate migration" >&2; exit 1; }

say "ok"
