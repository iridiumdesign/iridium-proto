//! A hand-built table for the renderer tests, so they do not need a
//! database. `shop.product` covers the cases that decide output: a
//! server-owned key, a literal default, a unique column, a foreign key, an
//! enum type, and a nullable column.

use crate::config::Generate;
use crate::introspect::{Column, ForeignKey, Model, PgEnum, PgType, RelKind, Table};

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
        },
        enums: vec![PgEnum {
            schema: "shop".into(),
            name: "product_status".into(),
            labels: vec!["draft".into(), "active".into()],
        }],
    }
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
    }
}
