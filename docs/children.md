# Children

Part of the [iridium-proto](../README.md) reference: the parent's
side of a foreign key — the field that holds the child rows, the
loaders that fill it, and `set_id_on_children`.

A single-column foreign key gives the child a finder. The parent gets
the other half: a field holding the child rows, and the methods that
fill it.

```rust
/// Rows of `shop.variant` whose `product_id` is this row's `id`. Not a
/// column: `ProductMapper::load_variant_children` fills it, and it is
/// empty until then.
#[sqlx(skip)]
#[serde(default)]
pub variant_children: Vec<Variant>,
```

```rust
let products = ProductMapper::new(&pool);
let mut one = products.find_by_id(id).await?.unwrap();
products.load_variant_children(&mut one).await?;    // fills one.variant_children
let same = products.find_by_id_with_variant_children(id).await?;
```

One level: the children's own children are not loaded. A table that
refers to itself — `category.parent_id` to `category.id` — is the
common tree, and holds `category_children: Vec<Category>` like any
other parent.

The other direction is in memory only, and it is the row's own. Every
row type has `set_id_on_children`, which copies its key into every
child row it holds — each `variant_children` row's `product_id` becomes
this row's `id` — and then asks each child to do the same, all the way
down. A parent and its children built by hand agree before anything is
saved, however deep the tree. A nullable foreign key, the tree's
`parent_id`, takes `Some(id)`. A table with no children fields still
has the method, doing nothing, so a caller can rely on it. The key is
copied per child, so a key of a generated enum or composite type needs
`Clone` among its derives; without it that field is left alone, with a
warning that says so.

```rust
one.variant_children.push(Variant { product_id: Uuid::nil(), ..variant });
one.set_id_on_children();                         // product_id is one.id now
```

Every field is named after its child table with `children_field` as the
suffix — `variant_children`, `review_children` — or after the table and
column when the same child refers to the parent twice
(`link_children_by_from_id`). The count of child tables plays no part,
so a second one arriving later cannot rename the first. The loaders
follow the field: `load_variant_children`,
`find_by_id_with_variant_children`. Both can be overridden per parent:

```toml
[generate]
children_field = "children"

[generate.relations."shop.product"]
children = "variants"                      # one child table

[generate.relations."shop.category"]
children = { category = "subcategories", product = "products" }
```

`children_field = ""` generates no children fields at all.

Under `--sql server` the parent's mapper calls the child's own finder
function, so nothing new is needed on the server. That function is
written by the *child's* migration, when the child's mapper is
generated: a schema or database run writes both, and a parent generated
on its own with `proto mapper` expects the child's to exist. A
one-to-one link, where the referring column is itself unique, is not a
collection and gets no field. A field that would collide — with a
column called `variant_children`, say, or a child table named `date_time` whose
`DateTime` shadows `chrono`'s — is skipped with a warning that says how
to rename it. When a field goes away or is renamed, the methods that
filled it and the import it needed go with it; regenerating leaves no
stale loader behind. Neither does a child in another schema,
or one excluded by `exclude_tables`: its type would not be there to
name. A single `proto model` run imports the child's type from the
sibling module `super::<child>`, the layout `--out-dir` writes.
