# pyo3

Part of the [iridium-proto](../README.md) reference: Python-compatible
models, mappers and operations behind a Cargo feature.

`--pyo3` gates every Python attribute behind a Cargo feature, so the same
file compiles in a pure-Rust crate and in a Python extension crate:

```rust
#[derive(sqlx::FromRow, Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "python", pyo3::pyclass(get_all, set_all))]
pub struct Product { ... }
```

Getters and setters come from `get_all, set_all` on the class rather than
a `#[pyo3(get, set)]` on each field. That is deliberate: `pyclass`
expands before `cfg_attr` does, so a field-level `cfg_attr` leaves an
orphaned `pyo3` attribute and the crate will not compile. Enums get
`pyclass(eq, eq_int)`, and composite types `get_all, set_all` like a row
struct.

Input types are classes too, with a constructor. A column that is
`NOT NULL` without a default is a required argument; everything else is
keyword-only and defaults to `None`, which leaves a defaulted column to
the database:

```python
NewProduct("dovetail-saw", "Dovetail saw", org_id)
NewProduct("dovetail-saw", "Dovetail saw", org_id, status=ProductStatus.Active)
```

The consuming crate declares the feature and the pyo3 conversions its
column types need. `proto` writes both into `Cargo.toml` when the
output lands in a crate (see [Dependencies](type-mapping.md#dependencies)); by hand,
it is:

```toml
[features]
python = ["dep:pyo3", "pyo3/chrono", "pyo3/uuid", "pyo3/rust_decimal"]

[dependencies]
pyo3 = { version = "0.26", optional = true }
```

Declare the feature even when it is off — otherwise every generated file
draws an `unexpected cfg condition` warning. Rename it with
`pyo3_feature` in the config.

Where a schema run puts enum and composite types in their own module,
each model re-exports the ones its struct names, so
`model::item::ItemStatus` resolves for whoever holds an `Item` and
nobody has to know where `proto` filed it.

`--pymodule <name>` writes the registration too, so nothing about the
crossing is hand-maintained:

```
proto schema shop --pyo3 --pymodule shop --out-dir src/model
```

```rust
/// Register every generated class in `shop` on a module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<super::enums::ProductStatus>()?;
    m.add_class::<super::product::Product>()?;
    Ok(())
}

/// The classes as an extension module, for when that is all you need.
#[pymodule]
pub fn shop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    register(m)
}
```

It lands as `python.rs` beside the models, and the generated `mod.rs`
declares it behind the pyo3 feature. An extension has room for exactly
one module initializer, so when yours needs functions of its own — a
mapper-backed query, say — write your own `#[pymodule]` and call
`register` from it rather than listing classes by hand.

`proto database --pymodule <name>` does the same across schemas, one
Python submodule each, registered in `sys.modules` so both
`import store.shop` and `from store.shop import Product` work. Two
schemas may hold a table of the same name without colliding, which is
why the classes are named by full path rather than imported.

The `#[pymodule]` does not have to sit at the crate root — pyo3 exports
its initializer from a nested module just as well — so a consumer needs
no glue at all beyond declaring the module.

## Mappers from Python

`--pyo3` on a mapper — `proto mapper shop.product --pyo3`, or `--mappers
--pyo3` on a schema or database run — adds a Python class beside the
Rust one, with the same methods:

```python
db = shop.Database("postgres://shop@localhost/shop")
products = shop.ProductMapper(db)

made = products.create(NewProduct("dovetail-saw", "Dovetail saw", org_id))
one = products.find_by_id(made.id)
one.name = "Dovetail saw, 10in"
products.update(one)

one = products.load_variant_children(one)       # a filled copy
one = products.find_by_id_with_variant_children(made.id)
one.set_id_on_children()                        # the row's own, in place
products.delete(one.id)
```

`find_where` takes a dict:

```python
active = products.find_where({"status": ProductStatus.Active, "price__lt": Decimal("10")})
some = products.find_where({"id": [a, b, c]}, order_by="-created_at", limit=20)
none = products.find_where({"org_id": None})           # IS NULL
n = products.count_where({"slug__like": "dove%"})
```

Keys are column names. An operator follows a double underscore —
`lt`, `lte`, `gt`, `gte`, `ne`, `like`, `in` — and a bare key is `=`.
`None` is `IS NULL`, or `IS NOT NULL` under `__ne`; a list is `= ANY`.
`order_by` is a column name or a list of them, `-name` for descending,
with `limit` and `offset` beside it. Each value is read as its column's
own type, so a `uuid` column takes a `uuid.UUID` and refuses a `str`
with a `TypeError`; a key that is not a column is a `KeyError` before
anything is sent. A column whose type does not cross — an array, a
composite, a type from `[generate.types]` — cannot be queried from
Python and says so by name.

The [children](children.md) loaders come too. Python has no `&mut`, so
`load_variant_children` hands back the row with the field filled rather
than changing the one it was given; the `_with_variant_children` finder
is the same as the Rust one. A row that arrives any other way has an
empty list until one of them runs. Handing back a copy needs `Clone`
among the row derives, which the default has; without it the class
gets the finder and no loader, and the run says so.
`set_id_on_children` is the row's own method and changes the object in
place, as it does in Rust.

The Rust mappers are `async` and Python, from here, is not: each call
runs to completion on a tokio runtime the `Database` holds, with the GIL
released meanwhile. An `asyncio`-native form is a follow-up. Every
database error arrives as `ProtoError`, a plain `Exception` carrying the
driver's message, so a lookup that finds nothing is `None` and a failed
statement is an exception, the same shape as the Rust.

`Database`, `ProtoError`, and a `register` for the mapper classes live
in a `python.rs` beside the mappers, which a schema or database run
writes and the mappers' `mod.rs` declares behind the feature. A lone
`proto mapper --pyo3` assumes that file at `super::python`. The bridge
has no `#[pymodule]` of its own — an extension has room for one, and the
model side may already have written it — so register both from yours,
models first:

```rust
#[pymodule]
fn shop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    model::python::register(m)?;
    mapper::python::register(m)
}
```

The consuming crate needs `tokio` for the runtime, alongside what the
models already asked for. Both strategies expose the same Python
surface, since the class wraps whichever mapper is beside it.

Everything crosses as the native Python type, and keeps the schema's
guarantees on the far side:

| Column | Python |
|---|---|
| `uuid` | `uuid.UUID` |
| `text` | `str` |
| `numeric` | `decimal.Decimal` |
| `timestamptz` | aware `datetime.datetime` |
| `text[]` | `list[str]` |
| an enum type | a Python enum, comparable and `int()`-able |
| a composite type | its own class, with the same fields |
| a nullable column | the value, or `None` |

Setters are type-checked rather than coercing, and a `NOT NULL` column
refuses `None`. `just python` proves all of that against a real
interpreter rather than taking it on trust; see
[CONTRIBUTING.md](../CONTRIBUTING.md).
