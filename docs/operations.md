# Operations

Part of the [iridium-proto](../README.md) reference: `Data`, one
request in and one outcome out through whichever mapper the request
names, in Rust and from Python.

Above the mappers sits the layer an integrating engineer would otherwise
write by hand: a layer that takes a request, works out which mapper it
is for, and runs it. `proto` generates it, because the routing — a
table name to a mapper, a request's values to that mapper's column
types — is what the type map already knows, and what drifts first when
the schema moves.

`Data` is the first operation: one request in, one outcome out, through
whichever mapper the request names, with the table's own types in and
out and no interpreter in the path.

```
proto schema shop --mappers --operations \
    --out-dir src/model --mapper-dir src/mapper --operation-dir src/operation
```

```rust
use model::product::{NewProduct, ProductPatch};
use operation::data::{Data, Op, Outcome, Request, Row, Rows, Values};

let data = Data::new(&pool);
let found = data
    .execute(Request {
        table: "shop.product",
        op: Op::Find,
        query: Query::new().eq("status", ProductStatus::Active).order_by_desc("created_at").limit(20),
        values: None,
    })
    .await?;
let Outcome::Rows(Rows::ShopProduct(rows)) = found else { unreachable!() };

let made = data
    .execute(Request {
        table: "product",
        op: Op::Create,
        query: Query::new(),
        values: Some(Values::ShopProductInput(NewProduct { slug: "dovetail-saw".into(), .. })),
    })
    .await?;                                         // Outcome::Row(Row::ShopProduct(..))
data.execute(Request {
        table: "product",
        op: Op::Update,
        query: Query::new().eq("id", id),
        values: Some(Values::ShopProductPatch(ProductPatch {
            name: Some("Dovetail saw, 10in".into()),
            ..Default::default()
        })),
    })
    .await?;                                         // Outcome::Rows(..), as written
data.execute(Request { table: "product", op: Op::Delete, query: Query::new().eq("id", id), values: None })
    .await?;                                         // Outcome::Deleted(1)
```

`table` is `schema.table`, or the bare name where only one table has
it. `op` is `Find`, `Count`, `Create`, `Update` or `Delete`. `query` is
what the mapper's `find_where` takes — the conditions, the order, the
limit and the offset; `count` uses its conditions only, and `create`
does not read it. `values` is typed for the table: `ShopProductInput`
carries the `New…` that `create` inserts, and `ShopProductPatch` the
model's `…Patch` — what `update` sets on every row the query finds.
What comes back is an
`Outcome`: `Rows` for `find` and `update`, `Row` for `create`, `Count`,
or `Deleted`; `Rows` and `Row` have one variant per table, named by
schema and table, so two tables of one name in two schemas stay apart.
What can go wrong is the operation's own `Error`: a table `Data` does
not route to, an operation the table cannot do — `create` on a view,
`delete` on a table without a key — a write without its values or with
another table's, or the database's error.

The routing table is one generated method, `execute`, and it is not
yours to edit: `proto` rewrites it whenever a table arrives or leaves,
so a route added by hand is lost on the next run. The enums are
replaced whole, as the generated enums are. Everything else follows
the mappers' rule — each method `proto` owns says so, and an operation
of your own beside `Data` stays where you put it. A table whose class
would be called `Data` is refused before anything is written.

## From Python

With `--pyo3` a Python class of the same name wraps it: the request
dict is read as the table's types, runs through the Rust `Data`, and
the outcome comes back as the table's classes.

```python
data = shop.Data(db)
rows = data.execute({"table": "shop.product", "op": "find",
                     "where": {"status": "active"}, "order_by": "-created_at",
                     "limit": 20})
n = data.execute({"table": "product", "op": "count", "where": {"price__lt": 10}})
made = data.execute({"table": "product", "op": "create",
                     "values": {"slug": "dovetail-saw", "name": "Dovetail saw"}})
data.execute({"table": "product", "op": "update",
              "where": {"id": made.id}, "values": {"name": "Dovetail saw, 10in"}})
data.execute({"table": "product", "op": "delete", "where": {"id": made.id}})
```

`op` is `find`, `count`, `create`, `update` or `delete`. `where`,
`order_by`, `limit` and `offset` are what the mapper's `find_where`
takes, each value read as its column's own type. `values` is what
`create` inserts, read through the `New…` constructor so the same
columns are required, or what `update` sets, each value read as its
column's type into the table's patch: the key and a column the
database owns are refused with a `ValueError`, a value of the wrong
type is a `TypeError`, and a name that is not a column is a
`KeyError`. `find` and `update` return the rows, `create` the row,
`count` and `delete` a number. A table that is not routed is a
`KeyError`; an operation a table cannot do is a `ValueError` naming
both; the database's error is a `ProtoError`.

`Data` lands as `data.rs` in `--operation-dir`. The Rust is there
without any feature; the Python class inside it is behind the pyo3
feature, with its own `register` — call it after the mappers':

```rust
#[pymodule]
fn shop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    model::python::register(m)?;
    mapper::python::register(m)?;
    operation::data::register(m)
}
```

`proto database` writes one `Data` over every schema at the root of
`--operation-dir`, with every table qualified; the bare name is a
schema run's shorthand.

## From Python, by key and by type

Beside `execute`, three methods for the common case, each taking the
ids that trace a request through a service:

```python
row = data.find("shop.product", pk, user_id, request_id)   # the row, or None
row = data.store(NewProduct("dovetail-saw", "Dovetail saw"), user_id, request_id)
row.name = "Dovetail saw, 10in"
row = data.update(row, user_id, request_id)                # the row as written
```

`find` addresses a row by the table's key, read as the key's own type
— a tuple in column order for a composite key. One value per key
column: a list is several, and is refused rather than found by `ANY`.
`store` takes an input, a `New…`, and routes by its type; `update`
takes a row and writes it back in full, as the mapper's `update` does.
Each writes one line to the `proto.data` logger before returning,
`INFO` when it went through and `ERROR` with the error when it did
not, carrying `request_id=` and `user_id=`, the operation, the table
and the key — for `store`, the key as the database filled it in:

```
request_id=8f3a… user_id=brad find shop.product pk=…: ok
request_id=8f3a… user_id=brad store NewProduct shop.product pk=…: ok
request_id=8f3a… user_id=brad update Product: failed: DatabaseError: …
```

It is one line whatever the ids carry: a control character in an id,
a key or an error is written as its escape, so a caller's input cannot
end the record or start another.

Three exceptions come with the module. `OperationError` is what an
operation raises unless it has a more specific one: a table `Data`
does not route to, a model of a type it does not know, a table with no
key. `DatabaseError`, an `OperationError`, is the database refusing or
failing, in the driver's words — the mapper's `ProtoError`, as the
operation reports it. `PermissionError`, also an `OperationError`, is
raised by nothing generated: it is there for an operation of your own.
A value that is not the key's type is a `TypeError`, as it is in a
`where`.
