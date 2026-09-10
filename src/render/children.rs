//! The parent's side of a foreign key.
//!
//! A single-column foreign key gives the child a finder. This module
//! gives the parent the other half: a field holding the child rows, and
//! the names both the model and the mapper agree on for it. Everything
//! about naming lives here so the struct field and the method that fills
//! it cannot drift apart.

use std::collections::BTreeMap;

use crate::config::{ChildrenName, Generate};
use crate::introspect::{Child, Table};
use crate::naming;

/// One child relationship as the parent's model and mapper see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildField<'a> {
    /// The catalog's side of it.
    pub child: &'a Child,
    /// The field on the parent struct, as an identifier (`r#order` when
    /// it has to be).
    pub field: String,
    /// The same, bare, for building method names: `load_order`.
    pub stem: String,
    /// The child's Rust type name, `Variant`.
    pub ty: String,
    /// The child's module, `variant`, for the import.
    pub module: String,
}

impl ChildField<'_> {
    /// Whether the child is the parent itself — a tree.
    pub fn is_self(&self, table: &Table) -> bool {
        self.child.schema == table.schema && self.child.table == table.name
    }
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
pub fn of<'a>(table: &'a Table, generate: &Generate) -> Vec<ChildField<'a>> {
    if generate.children_field.is_empty() {
        return Vec::new();
    }
    let children: Vec<&Child> = table
        .children
        .iter()
        .filter(|c| c.schema == table.schema)
        .filter(|c| !excluded(c, generate))
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

    children
        .iter()
        .map(|child| {
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
            ChildField {
                child,
                stem: field.trim_start_matches("r#").to_string(),
                field,
                ty: naming::pascal_case(&child.table),
                module: naming::ident(&child.table),
            }
        })
        .collect()
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
    use crate::render::fixture;

    fn child(table: &str, column: &str) -> Child {
        Child {
            schema: "shop".into(),
            table: table.into(),
            column: column.into(),
            ref_column: "id".into(),
        }
    }

    #[test]
    fn one_child_table_takes_the_configured_name() {
        let generate = Generate::default();
        let model = fixture::product();
        let fields = of(&model.table, &generate);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field, "children");
        assert_eq!(fields[0].ty, "Variant");
        assert_eq!(fields[0].module, "variant");

        let generate = Generate {
            children_field: "kids".into(),
            ..Generate::default()
        };
        assert_eq!(of(&fixture::product().table, &generate)[0].field, "kids");
    }

    #[test]
    fn several_child_tables_are_named_after_themselves() {
        let mut model = fixture::product();
        model.table.children = vec![
            child("variant", "product_id"),
            child("review", "product_id"),
            child("type", "product_id"),
        ];
        let fields = of(&model.table, &Generate::default());
        let names: Vec<&str> = fields.iter().map(|f| f.field.as_str()).collect();
        // A Rust keyword is a raw identifier as a field, and bare in a
        // method name, where `load_r#type` would not parse.
        assert_eq!(names, ["variant", "review", "r#type"]);
        assert_eq!(fields[2].stem, "type");
    }

    #[test]
    fn a_child_that_points_here_twice_is_named_by_column() {
        let mut model = fixture::product();
        model.table.children = vec![
            child("link", "from_id"),
            child("link", "to_id"),
            child("review", "product_id"),
        ];
        let fields = of(&model.table, &Generate::default());
        let names: Vec<&str> = fields.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(names, ["link_by_from_id", "link_by_to_id", "review"]);
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
        assert_eq!(
            of(&fixture::product().table, &generate)[0].field,
            "variants"
        );

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
        let fields = of(&model.table, &generate);
        let names: Vec<&str> = fields.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(names, ["variants", "review"]);
    }

    #[test]
    fn an_empty_name_turns_the_fields_off() {
        let generate = Generate {
            children_field: String::new(),
            ..Generate::default()
        };
        assert!(of(&fixture::product().table, &generate).is_empty());
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
        assert!(of(&model.table, &generate).is_empty());
    }

    #[test]
    fn a_tree_knows_it_is_one() {
        let model = fixture::category();
        let fields = of(&model.table, &Generate::default());
        assert_eq!(fields.len(), 1);
        assert!(fields[0].is_self(&model.table));
        assert_eq!(fields[0].ty, "Category");
    }
}
