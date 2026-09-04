//! The operation list a table gets.
//!
//! Both the mapper and the migration are rendered from this, so a method
//! and the function it calls cannot drift apart: if an operation is not
//! planned here, neither backend emits it.

use std::collections::BTreeSet;

use crate::introspect::{Column, Table};
use crate::naming;

/// What an operation does, which decides its shape on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Insert a row and return it.
    Insert,
    /// Replace every writable column of one row and return it.
    Update,
    /// Remove one row.
    Delete,
    /// At most one row, looked up by a key that is unique.
    FindOne,
    /// Any number of rows, looked up by a column that is not.
    FindMany,
    /// Every row.
    List,
}

/// One generated operation: a method on the mapper, and — under
/// [`crate::render::Strategy::Server`] — a Postgres function behind it.
#[derive(Debug, Clone)]
pub struct Operation<'a> {
    /// Rust method name, e.g. `find_by_slug`.
    pub method: String,
    /// Suffix of the server-side function name, e.g. `by_slug` in
    /// `shop.product_by_slug`.
    pub call: String,
    /// What the operation does.
    pub kind: Kind,
    /// Columns the caller supplies: the lookup key, or the row key for
    /// `Update` and `Delete`. Empty for `Insert` and `List`, which take the
    /// whole row and nothing respectively.
    pub columns: Vec<&'a Column>,
}

/// Plan every operation for a table.
///
/// Read-only relations get finders and `list` but no writers. A table with
/// no primary key gets no `find_by_id`, `update`, or `delete` — there is
/// nothing to address a single row by. Names are deduplicated: a column
/// that is both unique and a foreign key yields one finder, the unique one.
pub fn operations(table: &Table) -> Vec<Operation<'_>> {
    let mut ops = Vec::new();
    let mut names = BTreeSet::new();
    let writable = table.writable();
    let key = table.primary_key_columns();
    let has_key = !key.is_empty() && key.len() == table.primary_key.len();

    if writable && !table.insert_columns().is_empty() {
        ops.push(Operation {
            method: "create".to_string(),
            call: "insert".to_string(),
            kind: Kind::Insert,
            columns: Vec::new(),
        });
    }

    if has_key {
        names.insert(finder_name(&key));
        ops.push(Operation {
            method: finder_name(&key),
            call: "get".to_string(),
            kind: Kind::FindOne,
            columns: key.clone(),
        });
    }

    for unique in &table.unique_keys {
        let columns: Vec<&Column> = unique.iter().filter_map(|c| table.column(c)).collect();
        if columns.len() != unique.len() {
            continue;
        }
        let method = finder_name(&columns);
        if names.insert(method.clone()) {
            ops.push(Operation {
                call: format!("by_{}", joined(&columns, "_and_")),
                method,
                kind: Kind::FindOne,
                columns,
            });
        }
    }

    for fk in &table.foreign_keys {
        // A composite foreign key would take every column as an argument;
        // the single-column case is the one worth generating.
        let [name] = fk.columns.as_slice() else {
            continue;
        };
        let Some(column) = table.column(name) else {
            continue;
        };
        let columns = vec![column];
        let method = finder_name(&columns);
        if names.insert(method.clone()) {
            ops.push(Operation {
                call: format!("by_{}", column.name),
                method,
                kind: Kind::FindMany,
                columns,
            });
        }
    }

    ops.push(Operation {
        method: "list".to_string(),
        call: "list".to_string(),
        kind: Kind::List,
        columns: Vec::new(),
    });

    if writable && has_key && !table.update_columns().is_empty() {
        ops.push(Operation {
            method: "update".to_string(),
            call: "update".to_string(),
            kind: Kind::Update,
            columns: key.clone(),
        });
    }
    if writable && has_key {
        ops.push(Operation {
            method: "delete".to_string(),
            call: "delete".to_string(),
            kind: Kind::Delete,
            columns: key,
        });
    }

    ops
}

/// `find_by_id`, `find_by_species_id_and_threat_id`.
fn finder_name(columns: &[&Column]) -> String {
    format!("find_by_{}", joined(columns, "_and_"))
}

pub(crate) fn joined(columns: &[&Column], sep: &str) -> String {
    columns
        .iter()
        .map(|c| naming::ident(&c.name).trim_start_matches("r#").to_string())
        .collect::<Vec<_>>()
        .join(sep)
}

/// The qualified name of a generated function, e.g. `shop.product_insert`.
pub fn function(table: &Table, call: &str) -> String {
    format!("{}.{}_{call}", table.schema, table.name)
}
