//! The parent's side of a foreign key.
//!
//! A single-column foreign key gives the child a finder. This module
//! gives the parent the other half: a field holding the child rows, and
//! the names both the model and the mapper agree on for it. Everything
//! about naming lives here so the struct field and the method that fills
//! it cannot drift apart — including what gets left out, so a field the
//! model skips is one the mapper never tries to fill.

use std::collections::{BTreeMap, BTreeSet};

use super::plan;
use crate::config::{ChildrenName, Generate};
use crate::introspect::{Child, Table};
use crate::naming;
use crate::typemap;

/// One child relationship as the parent's model and mapper see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildField<'a> {
    /// The catalog's side of it.
    pub child: &'a Child,
    /// The field on the parent struct, as an identifier (`r#type` when
    /// it has to be).
    pub field: String,
    /// The same, bare, for building method names: `load_type`.
    pub stem: String,
    /// The child's Rust type name, `Variant`.
    pub ty: String,
    /// The child's module, `variant`, for the import.
    pub module: String,
    /// The `call` of the child's finder on its foreign key column — the
    /// suffix of the server-side function the loader calls. Comes from
    /// the same rule [`plan::operations`] uses, so the two cannot drift.
    pub call: String,
}

impl ChildField<'_> {
    /// Whether the child is the parent itself — a tree.
    pub fn is_self(&self, table: &Table) -> bool {
        self.child.schema == table.schema && self.child.table == table.name
    }
}

/// The children fields a table gets, and why any were left out.
#[derive(Debug, Default)]
pub struct Children<'a> {
    /// The fields, in catalog order.
    pub fields: Vec<ChildField<'a>>,
    /// A relationship that could not become a field without breaking
    /// the build, and what to do about it.
    pub warnings: Vec<String>,
}

/// The children fields a table gets, in catalog order.
///
/// A table with exactly one child table holds it in `children_field`.
/// With several, each is named after its child table, or after the
/// table and column when the same child refers to this table twice.
/// `[generate.relations]` overrides either. An empty `children_field`
/// means no fields at all. Children in another schema, and children
/// excluded by `exclude_tables`, are left out: their type would not be
/// there to name.
///
/// A field that would collide — with a column, with another child, or
/// whose type shares a name with one a column imports — is skipped with
/// a warning rather than written into a struct that will not compile.
pub fn of<'a>(table: &'a Table, generate: &Generate) -> Children<'a> {
    let mut out = Children::default();
    if generate.children_field.is_empty() {
        return out;
    }

    // The same constraint declared twice is one relationship.
    let mut distinct: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    let children: Vec<&Child> = table
        .children
        .iter()
        .filter(|c| c.schema == table.schema)
        .filter(|c| !excluded(c, generate))
        .filter(|c| distinct.insert((&c.schema, &c.table, &c.column)))
        .collect();

    // How many times each child table appears, to tell when the column
    // has to be part of the name.
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for child in &children {
        *seen.entry(child.table.as_str()).or_default() += 1;
    }

    let overrides = generate
        .relations
        .get(&format!("{}.{}", table.schema, table.name))
        .or_else(|| generate.relations.get(&table.name))
        .and_then(|r| r.children.as_ref());

    // What the struct already has: every column's field name, and the
    // short name of every type the columns import.
    let mut idents: BTreeSet<String> = table
        .columns
        .iter()
        .map(|c| naming::ident(&c.name))
        .collect();
    let mut leaves: BTreeMap<String, String> = BTreeMap::new();
    for column in &table.columns {
        for import in typemap::map(&column.ty, generate).imports {
            let leaf = import.rsplit("::").next().unwrap_or(&import).to_string();
            leaves.insert(leaf, import);
        }
    }
    let parent = naming::pascal_case(&table.name);
    let where_ = format!("{}.{}", table.schema, table.name);

    for child in &children {
        let name = override_for(overrides, child, children.len()).unwrap_or_else(|| {
            if children.len() == 1 {
                generate.children_field.clone()
            } else if seen[child.table.as_str()] > 1 {
                format!("{}_by_{}", child.table, child.column)
            } else {
                child.table.clone()
            }
        });
        let field = naming::ident(&name);
        if !idents.insert(field.clone()) {
            out.warnings.push(format!(
                "{where_}: the children field `{field}` for {}.{} collides with a \
                 column or another child; name it under [generate.relations] \
                 or the generated struct will not compile. Field skipped",
                child.schema, child.table
            ));
            continue;
        }

        let ty = naming::pascal_case(&child.table);
        let module = naming::ident(&child.table);
        let is_self = child.schema == table.schema && child.table == table.name;
        if !is_self {
            let path = format!("super::{module}::{ty}");
            let taken = ty == parent || leaves.get(&ty).is_some_and(|have| have != &path);
            if taken {
                out.warnings.push(format!(
                    "{where_}: the child type `{ty}` for {}.{} shares its name with \
                     a type this struct already uses; rename the table or set \
                     --name. Field `{field}` skipped",
                    child.schema, child.table
                ));
                idents.remove(&field);
                continue;
            }
            leaves.insert(ty.clone(), path);
        }

        out.fields.push(ChildField {
            child,
            stem: field.trim_start_matches("r#").to_string(),
            field,
            ty,
            module,
            call: plan::finder_call(&child.column, &child.primary_key, &child.unique_keys),
        });
    }
    out
}

/// What `[generate.relations]` says this child should be called, if it
/// says anything. A bare name applies to a table with one child; the
/// keyed form is matched by `schema.table.column`, `table.column`,
/// `schema.table`, then `table`.
fn override_for(overrides: Option<&ChildrenName>, child: &Child, count: usize) -> Option<String> {
    match overrides? {
        ChildrenName::One(name) if count == 1 => Some(name.clone()),
        ChildrenName::One(_) => None,
        ChildrenName::Each(map) => [
            format!("{}.{}.{}", child.schema, child.table, child.column),
            format!("{}.{}", child.table, child.column),
            format!("{}.{}", child.schema, child.table),
            child.table.clone(),
        ]
        .iter()
        .find_map(|key| map.get(key).cloned()),
    }
}

fn excluded(child: &Child, generate: &Generate) -> bool {
    let qualified = format!("{}.{}", child.schema, child.table);
    generate
        .exclude_tables
        .iter()
        .any(|t| t == &child.table || t == &qualified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Relation;
    use crate::introspect::PgType;
    use crate::render::fixture;

    fn child(table: &str, column: &str) -> Child {
        Child {
            schema: "shop".into(),
            table: table.into(),
            column: column.into(),
            ref_column: "id".into(),
            primary_key: vec!["id".into()],
            unique_keys: Vec::new(),
        }
    }

    fn names<'a>(children: &'a Children<'a>) -> Vec<&'a str> {
        children.fields.iter().map(|f| f.field.as_str()).collect()
    }

    #[test]
    fn one_child_table_takes_the_configured_name() {
        let generate = Generate::default();
        let model = fixture::product();
        let children = of(&model.table, &generate);
        assert!(children.warnings.is_empty(), "{:?}", children.warnings);
        assert_eq!(names(&children), ["children"]);
        assert_eq!(children.fields[0].ty, "Variant");
        assert_eq!(children.fields[0].module, "variant");
        assert_eq!(children.fields[0].call, "by_product_id");

        let generate = Generate {
            children_field: "kids".into(),
            ..Generate::default()
        };
        assert_eq!(names(&of(&model.table, &generate)), ["kids"]);
    }

    #[test]
    fn several_child_tables_are_named_after_themselves() {
        let mut model = fixture::product();
        model.table.children = vec![
            child("variant", "product_id"),
            child("review", "product_id"),
            child("type", "product_id"),
        ];
        let children = of(&model.table, &Generate::default());
        // A Rust keyword is a raw identifier as a field, and bare in a
        // method name, where `load_r#type` would not parse.
        assert_eq!(names(&children), ["variant", "review", "r#type"]);
        assert_eq!(children.fields[2].stem, "type");
    }

    #[test]
    fn a_child_that_points_here_twice_is_named_by_column() {
        let mut model = fixture::product();
        model.table.children = vec![
            child("link", "from_id"),
            child("link", "to_id"),
            child("review", "product_id"),
        ];
        let children = of(&model.table, &Generate::default());
        assert_eq!(
            names(&children),
            ["link_by_from_id", "link_by_to_id", "review"]
        );
    }

    #[test]
    fn the_same_constraint_twice_is_one_relationship() {
        let mut model = fixture::product();
        model.table.children = vec![
            child("variant", "product_id"),
            child("variant", "product_id"),
        ];
        let children = of(&model.table, &Generate::default());
        assert_eq!(names(&children), ["children"]);
        assert!(children.warnings.is_empty(), "{:?}", children.warnings);
    }

    #[test]
    fn relations_override_by_parent() {
        let mut generate = Generate::default();
        generate.relations.insert(
            "shop.product".into(),
            Relation {
                children: Some(ChildrenName::One("variants".into())),
            },
        );
        let model = fixture::product();
        assert_eq!(names(&of(&model.table, &generate)), ["variants"]);

        // The keyed form, for a parent with several child tables.
        let mut model = fixture::product();
        model.table.children = vec![
            child("variant", "product_id"),
            child("review", "product_id"),
        ];
        let mut generate = Generate::default();
        generate.relations.insert(
            "product".into(),
            Relation {
                children: Some(ChildrenName::Each(
                    [("shop.variant".to_string(), "variants".to_string())]
                        .into_iter()
                        .collect(),
                )),
            },
        );
        assert_eq!(names(&of(&model.table, &generate)), ["variants", "review"]);
    }

    #[test]
    fn an_empty_name_turns_the_fields_off() {
        let generate = Generate {
            children_field: String::new(),
            ..Generate::default()
        };
        let model = fixture::product();
        assert!(of(&model.table, &generate).fields.is_empty());
    }

    #[test]
    fn children_proto_will_not_generate_are_left_out() {
        let mut model = fixture::product();
        model.table.children = vec![
            child("variant", "product_id"),
            Child {
                schema: "audit".into(),
                ..child("log", "product_id")
            },
        ];
        let mut generate = Generate::default();
        generate.exclude_tables.push("shop.variant".into());
        let children = of(&model.table, &generate);
        assert!(children.fields.is_empty());
        // Not a collision, not a warning: they are simply not here.
        assert!(children.warnings.is_empty(), "{:?}", children.warnings);
    }

    #[test]
    fn a_tree_knows_it_is_one() {
        let model = fixture::category();
        let children = of(&model.table, &Generate::default());
        assert_eq!(children.fields.len(), 1);
        assert!(children.fields[0].is_self(&model.table));
        assert_eq!(children.fields[0].ty, "Category");
    }

    /// A parent with a real `children` column cannot also hold its
    /// children in one. The column is the database's; the field gives
    /// way, and the warning says how to rename it.
    #[test]
    fn a_field_that_collides_with_a_column_is_skipped_and_said() {
        let mut model = fixture::product();
        model.table.columns.push(fixture::column(
            "children",
            PgType::Scalar("int4".into()),
            "integer",
            true,
        ));
        let children = of(&model.table, &Generate::default());
        assert!(children.fields.is_empty());
        assert_eq!(children.warnings.len(), 1);
        assert!(
            children.warnings[0].contains("`children`")
                && children.warnings[0].contains("[generate.relations]"),
            "{}",
            children.warnings[0]
        );

        // Two relationships that reduce to one field name: the second
        // gives way.
        let mut model = fixture::product();
        model.table.children = vec![
            child("variant", "product_id"),
            child("Variant", "product_id"),
        ];
        let children = of(&model.table, &Generate::default());
        assert_eq!(names(&children), ["variant"]);
        assert_eq!(children.warnings.len(), 1);
    }

    /// `chrono::DateTime` is already in the file; a child table named
    /// `date_time` would import a second `DateTime` beside it.
    #[test]
    fn a_child_type_that_shadows_an_import_is_skipped_and_said() {
        let mut model = fixture::product();
        model.table.children = vec![child("date_time", "product_id")];
        let children = of(&model.table, &Generate::default());
        assert!(children.fields.is_empty());
        assert_eq!(children.warnings.len(), 1);
        assert!(
            children.warnings[0].contains("`DateTime`"),
            "{}",
            children.warnings[0]
        );
    }

    /// The server function the loader calls is the child's finder, named
    /// by the same rule that names the finder itself.
    #[test]
    fn the_call_follows_the_childs_own_finder() {
        let mut model = fixture::product();
        // A unique column on the child whose ident matches the foreign
        // key column's: the finder is the unique one, so is the function.
        model.table.children = vec![Child {
            unique_keys: vec![vec!["product-id".into()]],
            ..child("variant", "product_id")
        }];
        let children = of(&model.table, &Generate::default());
        assert_eq!(children.fields[0].call, "by_product_id");

        model.table.children = vec![Child {
            primary_key: vec!["product_id".into()],
            ..child("variant", "product_id")
        }];
        let children = of(&model.table, &Generate::default());
        assert_eq!(children.fields[0].call, "get");
    }
}
