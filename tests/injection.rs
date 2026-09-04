//! SQL injection: what is bound, what is quoted, and what is neither.
//!
//! Three surfaces, and they fail in different ways, so they are tested
//! separately.
//!
//! 1. **The queries the generated mappers run.** Values are bound as
//!    parameters and never interpolated, so an application passing user
//!    input to a finder cannot be injected through it. This is the one
//!    people mean when they ask.
//!
//! 2. **The identifiers proto copies out of the catalogs.** Table and
//!    column names are written into SQL text at generation time. They are
//!    quoted, so a name carrying a payload stays one identifier instead
//!    of becoming a second statement.
//!
//! 3. **The Rust the SQL is embedded in.** A quoted identifier carries
//!    the character that ends a Rust string literal, so the statement is
//!    escaped on its way into the source. A failure here would be a
//!    payload escaping into generated *code* rather than into SQL.
//!
//! Surface 2 is defence in depth, not a trust boundary: proto reads a
//! database you point it at, and a schema you do not control can make it
//! write anything into the doc comments regardless. See SECURITY.md.

use std::collections::BTreeSet;

use iridium_proto::config::Generate;
use iridium_proto::introspect::{Column, ForeignKey, Model, PgType, RelKind, Table};
use iridium_proto::render::{self, Opts, Strategy};

/// A payload that would end the statement and start another, in the two
/// places an identifier reaches SQL.
const PAYLOAD: &str = r#"x"; DROP TABLE keep_me; --"#;

fn column(name: &str, sql_type: &str, not_null: bool) -> Column {
    Column {
        name: name.to_string(),
        ty: PgType::Scalar("text".into()),
        sql_type: sql_type.to_string(),
        not_null,
        comment: None,
        has_default: false,
        default_expr: None,
        identity: false,
        generated: false,
    }
}

/// A table whose own name, and one of whose columns, is a payload.
fn hostile() -> Model {
    Model {
        table: Table {
            schema: "public".into(),
            name: PAYLOAD.to_string(),
            kind: RelKind::Table,
            comment: None,
            columns: vec![
                column("id", "uuid", true),
                column(PAYLOAD, "text", true),
                column("ordinary", "text", false),
            ],
            primary_key: vec!["id".into()],
            unique_keys: vec![vec![PAYLOAD.to_string()]],
            foreign_keys: vec![ForeignKey {
                columns: vec!["ordinary".into()],
                ref_schema: "public".into(),
                ref_table: "other".into(),
            }],
        },
        enums: Vec::new(),
    }
}

fn opts(generate: &Generate, strategy: Strategy) -> Opts<'_> {
    Opts {
        generate,
        pyo3: false,
        inputs: true,
        model_path: "crate::model".to_string(),
        strategy,
        target: "dev",
        command: "proto mapper public.x".to_string(),
        name_override: None,
    }
}

/// Pull each query string out of a generated mapper, with the rest of the
/// method that runs it.
fn queries(code: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = code;

    while let Some(at) = rest.find("sqlx::query") {
        let after = &rest[at..];
        let open = after.find('"').expect("a query takes a string literal");
        let body = &after[open + 1..];

        // Walk to the closing quote, honouring escapes.
        let mut end = None;
        let mut escaped = false;
        for (i, c) in body.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                end = Some(i);
                break;
            }
        }
        let end = end.expect("the literal must be closed");
        let sql = &body[..end];

        // The method's tail, up to whatever runs it.
        let tail = &body[end..];
        let stop = tail
            .find(".fetch")
            .or_else(|| tail.find(".execute"))
            .unwrap_or(tail.len());
        found.push((sql.to_string(), tail[..stop].to_string()));

        rest = &after[open + 1 + end..];
    }

    assert!(!found.is_empty(), "no queries found in:\n{code}");
    found
}

fn placeholders(sql: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes: Vec<char> = sql.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        if *c == '$' {
            let digits: String = bytes[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                found.insert(digits);
            }
        }
    }
    found
}

/// Strip quoted identifiers and string literals, leaving what Postgres
/// would parse as SQL syntax. Doubling is how both kinds escape their own
/// delimiter, so `"a""b"` is one identifier and `'it''s'` is one literal.
fn outside_quotes(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            i += 1;
            loop {
                match chars.get(i) {
                    None => break,
                    // A doubled delimiter is an escaped one: stay inside.
                    Some(&d) if d == c && chars.get(i + 1) == Some(&c) => i += 2,
                    Some(&d) if d == c => {
                        i += 1;
                        break;
                    }
                    Some(_) => i += 1,
                }
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Undo the escaping that put this statement inside a Rust literal.
fn unescape(literal: &str) -> String {
    literal.replace("\\\"", "\"").replace("\\\\", "\\")
}

// ── 1. Values are bound ─────────────────────────────────────────────────

/// Every value reaches the database as a bound parameter. A query is a
/// plain string literal — nothing is assembled at run time — and each
/// distinct placeholder has exactly one `.bind`.
#[test]
fn generated_queries_bind_every_value() {
    let generate = Generate::default();
    for strategy in [Strategy::Embedded, Strategy::Server] {
        let code = render::mapper::mapper_file(&hostile(), &opts(&generate, strategy)).code;

        for (sql, tail) in queries(&code) {
            let binds = tail.matches(".bind(").count();
            let placeholders = placeholders(&sql);
            assert_eq!(
                binds,
                placeholders.len(),
                "{strategy:?}: {binds} binds for {} placeholders in:\n{sql}",
                placeholders.len()
            );
        }
    }
}

/// Nothing in a generated mapper builds a query out of pieces. If this
/// ever fails, the assumption behind the test above is gone.
#[test]
fn generated_queries_are_never_assembled() {
    let generate = Generate::default();
    for strategy in [Strategy::Embedded, Strategy::Server] {
        let code = render::mapper::mapper_file(&hostile(), &opts(&generate, strategy)).code;
        for forbidden in ["format!", "push_str", "to_string()", "String::from", " + &"] {
            assert!(
                !code.contains(forbidden),
                "{strategy:?}: a mapper should not contain `{forbidden}`:\n{code}"
            );
        }
    }
}

// ── 2. Identifiers stay identifiers ─────────────────────────────────────

/// A relation or column named like a statement terminator stays one
/// quoted identifier. The payload appears in the SQL, because it *is* the
/// name — what must not happen is any of it landing where Postgres would
/// execute it.
#[test]
fn hostile_names_cannot_close_an_identifier() {
    let generate = Generate::default();
    for strategy in [Strategy::Embedded, Strategy::Server] {
        let code = render::mapper::mapper_file(&hostile(), &opts(&generate, strategy)).code;
        for (literal, _) in queries(&code) {
            let sql = unescape(&literal);
            // Quoted, the payload's own quote is doubled — that doubling
            // is exactly what keeps it one identifier.
            assert!(
                sql.contains(&PAYLOAD.replace('"', "\"\"")),
                "the name should be there, quoted:\n{sql}"
            );

            // With every quoted region removed, nothing of the payload is
            // left: no statement separator, no keyword, no comment marker.
            let bare = outside_quotes(&sql);
            assert!(
                !bare.contains(';'),
                "{strategy:?}: a second statement:\n{sql}"
            );
            assert!(
                !bare.to_uppercase().contains("DROP"),
                "{strategy:?}: payload reached executable position:\n{sql}"
            );
            assert!(
                !bare.contains("--"),
                "{strategy:?}: a comment escaped:\n{sql}"
            );
        }
    }
}

/// The migration compares function names as string literals, not as
/// identifiers, so that path needs `''` rather than `""`.
#[test]
fn the_migration_escapes_names_used_as_literals() {
    let generate = Generate::default();
    let sql = render::sql::migration_file(&hostile(), &opts(&generate, Strategy::Server));

    // The drop block's IN list holds function names as literals. A single
    // quote in one would end the literal; there are none here, but an
    // apostrophe in a table name is ordinary enough to check for.
    let apostrophe = Model {
        table: Table {
            name: "it's".to_string(),
            ..hostile().table
        },
        enums: Vec::new(),
    };
    let quoted = render::sql::migration_file(&apostrophe, &opts(&generate, Strategy::Server));
    assert!(quoted.contains("'it''s_insert'"), "{quoted}");

    // And the schema is compared as a literal too.
    assert!(sql.contains("nspname = 'public'"), "{sql}");
}

// ── 3. The Rust the SQL lives in ────────────────────────────────────────

/// A quoted identifier carries the character that ends a Rust string
/// literal. If it is not escaped on the way in, the payload stops being
/// data and becomes code.
#[test]
fn hostile_names_cannot_escape_the_rust_literal() {
    let generate = Generate::default();
    for strategy in [Strategy::Embedded, Strategy::Server] {
        let code = render::mapper::mapper_file(&hostile(), &opts(&generate, strategy)).code;
        let found = queries(&code);

        // The escaping happened at all.
        assert!(
            found.iter().any(|(literal, _)| literal.contains("\\\"")),
            "{strategy:?}: nothing was escaped, so nothing was quoted:\n{code}"
        );

        // And it held. `queries` walks the literal honouring escapes, so
        // if a quote had ended it early the statement would be truncated
        // before the rest of the payload. This is the assertion that
        // catches that.
        let doubled = PAYLOAD.replace('"', "\"\"");
        for (literal, _) in &found {
            let sql = unescape(literal);
            if sql.contains("DROP TABLE") {
                assert!(
                    sql.contains(&doubled),
                    "{strategy:?}: the literal ended inside the payload:\n{sql}"
                );
            }
        }
    }
}
