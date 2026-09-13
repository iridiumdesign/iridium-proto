//! The `WHERE` clause a caller builds at run time.
//!
//! `find_where` and `count_where` on every mapper take a `Query`: a list
//! of conditions on the table's columns, an order, a limit and an
//! offset. The type is generated once beside the mappers, as the Python
//! bridge is, so a consuming crate has no runtime dependency on proto.
//! It knows nothing about any table: the mapper that runs it supplies
//! the column names it may use, and refuses any other before a
//! statement is sent.

use super::{Opts, header};

/// The file, `query.rs`, beside the mappers.
pub fn query_file(opts: &Opts) -> String {
    let mut code = header(opts, "every mapper", "query builder");
    code.push_str(DOCS);
    code.push('\n');
    code.push_str(BODY);
    code
}

/// The module's own doc comment. Kept apart from the body because an
/// `include!` cannot carry one, and the tests include the body.
const DOCS: &str = "\
//! A `WHERE` clause built at run time, for `find_where` and `count_where`\n\
//! on every mapper.\n\
//!\n\
//! Every value is bound, never written into the statement. Column names\n\
//! are checked by the mapper that runs the query against the columns it\n\
//! has, so a name that is not a column is an error before any SQL is\n\
//! sent.\n";

/// What the file says. No placeholders: the module stands on `sqlx`
/// alone, and its place is fixed relative to the mappers. It is a file
/// of its own so the tests below can compile it and run it, rather than
/// read it.
const BODY: &str = include_str!("query_body.rs");

/// The rendered module, compiled here so its behaviour is tested and
/// not only its text — and so proto's own clippy sees it before a
/// consumer's does.
#[cfg(test)]
#[allow(dead_code)]
mod body {
    include!("query_body.rs");
}

#[cfg(test)]
mod tests {
    use super::body::{Op, Query};
    use super::*;
    use crate::config::Generate;
    use crate::render::{Strategy, fixture};

    const PAYLOAD: &str = r#"x"; DROP TABLE keep_me; --"#;
    const QUOTED: &str = r#""x""; DROP TABLE keep_me; --""#;
    const COLUMNS: &[(&str, &str)] = &[("id", "id"), (PAYLOAD, QUOTED)];

    #[test]
    fn a_name_that_is_not_a_column_is_refused_before_any_sql() {
        let err = Query::new()
            .eq(PAYLOAD, 1)
            .select("t", &[("id", "id")])
            .unwrap_err();
        assert!(matches!(err, sqlx::Error::ColumnNotFound(name) if name == PAYLOAD));
        let err = Query::new()
            .order_by(PAYLOAD)
            .select("t", &[("id", "id")])
            .unwrap_err();
        assert!(matches!(err, sqlx::Error::ColumnNotFound(_)));
    }

    #[test]
    fn a_hostile_column_stays_one_quoted_identifier() {
        let (sql, _) = Query::new()
            .eq(PAYLOAD, "v")
            .order_by_desc(PAYLOAD)
            .select("t", COLUMNS)
            .unwrap();
        assert_eq!(
            sql,
            format!("SELECT * FROM t WHERE {QUOTED} = $1 ORDER BY {QUOTED} DESC")
        );
    }

    #[test]
    fn values_are_placeholders_never_text() {
        let hostile = "'; DROP TABLE keep_me; --";
        let (sql, _) = Query::new()
            .eq("id", hostile)
            .ne("id", hostile)
            .lt("id", 1)
            .lte("id", 1)
            .gt("id", 1)
            .gte("id", 1)
            .like("id", hostile)
            .any("id", vec![1, 2])
            .null("id")
            .not_null("id")
            .select("t", COLUMNS)
            .unwrap();
        assert!(!sql.contains("DROP"), "{sql}");
        assert_eq!(
            sql,
            "SELECT * FROM t WHERE id = $1 AND id <> $2 AND id < $3 AND id <= $4 \
             AND id > $5 AND id >= $6 AND id LIKE $7 AND id = ANY($8) \
             AND id IS NULL AND id IS NOT NULL"
        );
    }

    #[test]
    fn limit_and_offset_bind_after_the_clauses_and_count_drops_them() {
        let (sql, _) = Query::new()
            .eq("id", 1)
            .order_by("id")
            .limit(10)
            .offset(20)
            .select("t", COLUMNS)
            .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM t WHERE id = $1 ORDER BY id LIMIT $2 OFFSET $3"
        );
        let (sql, _) = Query::new()
            .eq("id", 1)
            .order_by("id")
            .limit(10)
            .count("t", COLUMNS)
            .unwrap();
        assert_eq!(sql, "SELECT count(*) FROM t WHERE id = $1");
        let (sql, _) = Query::new().select("t", COLUMNS).unwrap();
        assert_eq!(sql, "SELECT * FROM t");
    }

    #[test]
    fn an_operator_chosen_at_run_time_takes_the_same_path() {
        let (sql, _) = Query::new()
            .cond("id", Op::Gte, 1)
            .cond("id", Op::IsNull, 0)
            .select("t", COLUMNS)
            .unwrap();
        assert_eq!(sql, "SELECT * FROM t WHERE id >= $1 AND id IS NULL");
        assert_eq!(Op::from_suffix("in"), Some(Op::Any));
        assert_eq!(Op::from_suffix("between"), None);
    }

    #[test]
    fn the_file_is_rust_and_says_what_it_offers() {
        let generate = Generate::default();
        let out = query_file(&fixture::opts(&generate, Strategy::Embedded));
        syn::parse_file(&out).expect("query.rs parses");
        for needed in [
            "pub enum Op {",
            "pub struct Query {",
            "pub fn eq<'a, T>(self, column: &str, value: T) -> Self",
            "pub fn any<'a, T>(self, column: &str, values: Vec<T>) -> Self",
            "pub fn null(mut self, column: &str) -> Self",
            "pub fn cond<'a, T>(self, column: &str, op: Op, value: T) -> Self",
            "pub fn order_by_desc(mut self, column: &str) -> Self",
            "pub fn select(",
            "pub fn count(",
            "sqlx::Error::ColumnNotFound(name.to_string())",
        ] {
            assert!(out.contains(needed), "missing {needed}\n{out}");
        }
        assert!(out.starts_with("// @generated by proto"), "{out}");
    }

    #[test]
    fn the_file_reconciles_to_itself() {
        let generate = Generate::default();
        let out = query_file(&fixture::opts(&generate, Strategy::Embedded));
        assert_eq!(
            crate::reconcile::reconcile(&out, &out).as_deref(),
            Some(out.as_str())
        );
    }
}
