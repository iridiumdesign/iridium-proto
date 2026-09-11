//! A hand-built table for the renderer tests, so they do not need a
//! database. `shop.product` covers the cases that decide output: a
//! server-owned key, a literal default, a unique column, a foreign key, an
//! enum type, a nullable column, and a child table (`shop.variant`)
//! pointing back at it. `shop.category` is a tree.

use crate::config::Generate;
use crate::introspect::{
    Child, Column, ForeignKey, Model, PgComposite, PgEnum, PgType, RelKind, Table,
};

use super::{Opts, Strategy};

pub fn column(name: &str, ty: PgType, sql_type: &str, not_null: bool) -> Column {
    Column {
        name: name.to_string(),
        ty,
        sql_type: sql_type.to_string(),
        not_null,
        comment: None,
        has_default: false,
        default_expr: None,
        identity: false,
        generated: false,
        extension: None,
    }
}

fn with_default(mut column: Column, expr: &str) -> Column {
    column.has_default = true;
    column.default_expr = Some(expr.to_string());
    column
}

pub fn product() -> Model {
    let columns = vec![
        with_default(
            column("id", PgType::Scalar("uuid".into()), "uuid", true),
            "gen_random_uuid()",
        ),
        column("slug", PgType::Scalar("text".into()), "text", true),
        column("name", PgType::Scalar("text".into()), "text", true),
        with_default(
            column(
                "status",
                PgType::Enum {
                    schema: "shop".into(),
                    name: "product_status".into(),
                },
                "shop.product_status",
                true,
            ),
            "'draft'::shop.product_status",
        ),
        column("price", PgType::Scalar("numeric".into()), "numeric", false),
        column("org_id", PgType::Scalar("uuid".into()), "uuid", true),
        with_default(
            column(
                "created_at",
                PgType::Scalar("timestamptz".into()),
                "timestamp with time zone",
                true,
            ),
            "now()",
        ),
    ];

    Model {
        table: Table {
            schema: "shop".into(),
            name: "product".into(),
            kind: RelKind::Table,
            comment: None,
            columns,
            primary_key: vec!["id".into()],
            unique_keys: vec![vec!["slug".into()]],
            foreign_keys: vec![ForeignKey {
                columns: vec!["org_id".into()],
                ref_schema: "shop".into(),
                ref_table: "organization".into(),
            }],
            children: vec![Child {
                schema: "shop".into(),
                table: "variant".into(),
                column: "product_id".into(),
                ref_column: "id".into(),
                primary_key: vec!["id".into()],
                unique_keys: Vec::new(),
            }],
        },
        enums: vec![PgEnum {
            schema: "shop".into(),
            name: "product_status".into(),
            labels: vec!["draft".into(), "active".into()],
        }],
        composites: Vec::new(),
    }
}

/// `shop.product` with two more columns: a composite, and an array of
/// it. The composite nests another, so both levels are covered.
pub fn sized_product() -> Model {
    let composite = |name: &str| PgType::Composite {
        schema: "shop".into(),
        name: name.into(),
    };
    let mut model = product();
    model.table.columns.push(column(
        "size",
        composite("dimensions"),
        "shop.dimensions",
        false,
    ));
    model.table.columns.push(column(
        "sizes",
        PgType::Array(Box::new(composite("dimensions"))),
        "shop.dimensions[]",
        false,
    ));
    // Nested first, as introspection lists them.
    model.composites = vec![
        PgComposite {
            schema: "shop".into(),
            name: "span".into(),
            comment: None,
            fields: vec![
                column("lo", PgType::Scalar("numeric".into()), "numeric", false),
                column("hi", PgType::Scalar("numeric".into()), "numeric", false),
            ],
        },
        PgComposite {
            schema: "shop".into(),
            name: "dimensions".into(),
            comment: Some("Width, height and a unit.".into()),
            fields: vec![
                column("width", composite("span"), "shop.span", false),
                column("height", composite("span"), "shop.span", false),
                column("unit", PgType::Scalar("text".into()), "text", false),
            ],
        },
    ];
    model
}

/// Options pointing at that table, with everything else at its default.
pub fn opts<'a>(generate: &'a Generate, strategy: Strategy) -> Opts<'a> {
    Opts {
        generate,
        pyo3: false,
        inputs: true,
        model_path: "crate::model".to_string(),
        strategy,
        target: "dev",
        command: "proto mapper shop.product".into(),
        name_override: None,
        bridge_path: "super::python".to_string(),
    }
}

/// A table that unquoted SQL cannot express: a reserved word for its own
/// name, a reserved word for a column, a column Postgres would fold, and
/// one with a space in it.
pub fn awkward() -> Model {
    let columns = vec![
        with_default(
            column("id", PgType::Scalar("uuid".into()), "uuid", true),
            "gen_random_uuid()",
        ),
        column("select", PgType::Scalar("text".into()), "text", true),
        column(
            "Mixed Case",
            PgType::Scalar("int4".into()),
            "integer",
            false,
        ),
        column("desc", PgType::Scalar("text".into()), "text", false),
    ];

    Model {
        table: Table {
            schema: "shop".into(),
            name: "order".into(),
            kind: RelKind::Table,
            comment: None,
            columns,
            primary_key: vec!["id".into()],
            unique_keys: vec![vec!["select".into()]],
            foreign_keys: Vec::new(),
            children: Vec::new(),
        },
        enums: Vec::new(),
        composites: Vec::new(),
    }
}

/// A tree: `category.parent_id` refers to `category.id`.
pub fn category() -> Model {
    let columns = vec![
        with_default(
            column("id", PgType::Scalar("uuid".into()), "uuid", true),
            "gen_random_uuid()",
        ),
        column("name", PgType::Scalar("text".into()), "text", true),
        column("parent_id", PgType::Scalar("uuid".into()), "uuid", false),
    ];
    Model {
        table: Table {
            schema: "shop".into(),
            name: "category".into(),
            kind: RelKind::Table,
            comment: None,
            columns,
            primary_key: vec!["id".into()],
            unique_keys: Vec::new(),
            foreign_keys: vec![ForeignKey {
                columns: vec!["parent_id".into()],
                ref_schema: "shop".into(),
                ref_table: "category".into(),
            }],
            children: vec![Child {
                schema: "shop".into(),
                table: "category".into(),
                column: "parent_id".into(),
                ref_column: "id".into(),
                primary_key: vec!["id".into()],
                unique_keys: Vec::new(),
            }],
        },
        enums: Vec::new(),
        composites: Vec::new(),
    }
}
