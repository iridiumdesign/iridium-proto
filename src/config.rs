//! XDG-style configuration.
//!
//! The file is read from `$PROTO_CONFIG`, else
//! `$XDG_CONFIG_HOME/proto/proto.toml`, else `~/.config/proto/proto.toml`.
//!
//! Unknown keys in a `[databases.*]` table are ignored on purpose, so a
//! config written for another tool can be pointed at directly rather than
//! copied: `PROTO_CONFIG=~/.config/other/other.toml` works as long as the
//! file carries the fields [`DbTarget`] needs.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sqlx::postgres::PgConnectOptions;

use crate::error::{Error, Result};

// ── File structures ─────────────────────────────────────────────────────────

/// A parsed config file.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Target used when `--db` is omitted. Unnecessary when the file defines
    /// exactly one database — that one is the default.
    pub default_db: Option<String>,
    /// The `[databases.*]` tables, by target name.
    #[serde(default)]
    pub databases: BTreeMap<String, DbTarget>,
    /// The `[generate]` table. Absent, every field takes its default.
    #[serde(default)]
    pub generate: Generate,
}

/// One database proto can be pointed at.
///
/// Unknown keys are ignored, so a file written for another tool can be
/// reused as long as it carries these.
#[derive(Debug, Deserialize)]
pub struct DbTarget {
    /// Full connection string. When present, the discrete fields are ignored.
    pub url: Option<String>,
    /// Host to connect to. Defaults to `localhost`.
    pub host: Option<String>,
    /// Port to connect to. Defaults to 5432.
    pub port: Option<u16>,
    /// Database name. Defaults to the target's own name.
    pub name: Option<String>,
    /// Role to connect as. Falls back to `$PGUSER`, then `$USER`.
    pub user: Option<String>,
    /// The password, inline.
    pub password: Option<String>,
    /// Path to a file holding the password (Docker Swarm secrets, mostly).
    pub password_file: Option<String>,
    /// Environment variable holding the password.
    pub password_env: Option<String>,
    /// Schema assumed when a table is named without one.
    pub schema: Option<String>,
}

/// Generation defaults. Every field has a flag or an override that beats
/// it for a single run.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Generate {
    /// Derives on the row structs. An entry containing `::` is written
    /// out as given; `Serialize` and `Deserialize` also pull their import.
    pub derives: Vec<String>,
    /// Derives on the Rust enums behind Postgres enum types.
    pub enum_derives: Vec<String>,
    /// Derives for the `New…` insert input types.
    pub input_derives: Vec<String>,
    /// Cargo feature that gates the pyo3 attributes.
    pub pyo3_feature: String,
    /// Schema assumed when a table is named without one.
    pub default_schema: String,
    /// Tables to skip, named bare or as `schema.table`.
    pub exclude_tables: Vec<String>,
    /// Schemas to skip entirely.
    pub exclude_schemas: Vec<String>,
    /// Postgres type name -> Rust path, e.g. `money = "rust_decimal::Decimal"`.
    /// Takes precedence over the built-in map, so it doubles as the escape
    /// hatch for types proto does not know.
    pub types: HashMap<String, String>,
}

impl Default for Generate {
    fn default() -> Self {
        Self {
            derives: [
                "sqlx::FromRow",
                "Debug",
                "Clone",
                "Serialize",
                "Deserialize",
            ]
            .map(String::from)
            .to_vec(),
            input_derives: ["Debug", "Clone", "Serialize", "Deserialize"]
                .map(String::from)
                .to_vec(),
            enum_derives: [
                "sqlx::Type",
                "Debug",
                "Clone",
                "Copy",
                "PartialEq",
                "Eq",
                "Serialize",
                "Deserialize",
            ]
            .map(String::from)
            .to_vec(),
            pyo3_feature: "python".to_string(),
            default_schema: "public".to_string(),
            exclude_tables: vec!["_sqlx_migrations".to_string()],
            exclude_schemas: vec![
                "information_schema".to_string(),
                "pg_catalog".to_string(),
                "pg_toast".to_string(),
            ],
            types: HashMap::new(),
        }
    }
}

// ── Loading ─────────────────────────────────────────────────────────────────

/// Where the config lives: `explicit` if given, else `$PROTO_CONFIG`,
/// else `$XDG_CONFIG_HOME/proto/proto.toml`, else
/// `~/.config/proto/proto.toml`.
pub fn config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(p) = std::env::var("PROTO_CONFIG") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("proto").join("proto.toml");
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("proto")
        .join("proto.toml")
}

impl Config {
    /// Load the config file. A missing file is not an error on its own —
    /// `--url` and `DATABASE_URL` both work without one, so absence comes
    /// back as `Ok(None)`.
    ///
    /// # Errors
    ///
    /// [`Error::ReadFile`] if the file exists but cannot be read, and
    /// [`Error::ParseConfig`] if it is not the TOML this expects.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::ReadFile {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let config: Config = toml::from_str(&contents).map_err(|source| Error::ParseConfig {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Some(config))
    }

    /// Every defined target, comma separated, for an error message.
    pub fn target_names(&self) -> String {
        self.databases
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Pick the target named by `--db`, else `default_db`, else the only
    /// one defined.
    ///
    /// # Errors
    ///
    /// [`Error::NoTargets`] when the file defines none,
    /// [`Error::UnknownTarget`] when the name is not among them, and
    /// [`Error::AmbiguousTarget`] when there are several and nothing said
    /// which.
    pub fn select(&self, requested: Option<&str>, path: &Path) -> Result<(&str, &DbTarget)> {
        if self.databases.is_empty() {
            return Err(Error::NoTargets {
                path: path.to_path_buf(),
            });
        }
        let name = match requested.or(self.default_db.as_deref()) {
            Some(name) => name,
            None if self.databases.len() == 1 => self.databases.keys().next().unwrap(),
            None => {
                return Err(Error::AmbiguousTarget {
                    available: self.target_names(),
                });
            }
        };
        let (key, target) =
            self.databases
                .get_key_value(name)
                .ok_or_else(|| Error::UnknownTarget {
                    name: name.to_string(),
                    available: self.target_names(),
                })?;
        Ok((key.as_str(), target))
    }
}

// ── Resolution ──────────────────────────────────────────────────────────────

/// A connection ready to open, plus the label used in generated headers.
/// The label is the target name, never a URL — credentials do not belong in
/// a file that gets committed.
pub struct Target {
    /// The target's name, as it appears in generated headers. Never a URL:
    /// credentials do not belong in a file that gets committed.
    pub label: String,
    /// Default schema for this target, if it set one.
    pub schema: Option<String>,
    /// Everything needed to open a connection.
    pub options: PgConnectOptions,
}

impl Target {
    /// Build from an explicit connection string (`--url` or
    /// `DATABASE_URL`).
    ///
    /// # Errors
    ///
    /// [`Error::Database`] if the string is not a connection URL sqlx
    /// understands.
    pub fn from_url(url: &str, label: &str) -> Result<Self> {
        let options: PgConnectOptions = url.parse()?;
        Ok(Self {
            label: label.to_string(),
            schema: None,
            options,
        })
    }

    /// Resolve a configured target into something connectable.
    ///
    /// # Errors
    ///
    /// [`Error::NoPassword`] when none of
    /// the password sources yield one, or a parse failure on a bad `url`.
    pub fn from_config(name: &str, target: &DbTarget) -> Result<Self> {
        if let Some(url) = &target.url {
            let mut resolved = Self::from_url(url, name)?;
            resolved.schema = target.schema.clone();
            return Ok(resolved);
        }

        let host = target.host.as_deref().unwrap_or("localhost");
        let port = target.port.unwrap_or(5432);
        let db = target.name.as_deref().unwrap_or(name);
        let user = target
            .user
            .as_deref()
            .map(str::to_string)
            .or_else(|| std::env::var("PGUSER").ok())
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "postgres".to_string());

        let password = resolve_password(target, host, port, db, &user)?;

        let options = PgConnectOptions::new()
            .host(host)
            .port(port)
            .database(db)
            .username(&user)
            .password(&password);

        Ok(Self {
            label: name.to_string(),
            schema: target.schema.clone(),
            options,
        })
    }
}

fn resolve_password(
    target: &DbTarget,
    host: &str,
    port: u16,
    db: &str,
    user: &str,
) -> Result<String> {
    if let Some(p) = &target.password {
        return Ok(p.clone());
    }
    if let Some(path) = &target.password_file {
        let contents = std::fs::read_to_string(path).map_err(|source| Error::ReadFile {
            path: PathBuf::from(path),
            source,
        })?;
        return Ok(contents.trim().to_string());
    }
    if let Some(var) = &target.password_env
        && let Ok(p) = std::env::var(var)
    {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("PGPASSWORD") {
        return Ok(p);
    }
    if let Some(p) = pgpass_lookup(host, port, db, user) {
        return Ok(p);
    }
    Err(Error::NoPassword {
        name: db.to_string(),
    })
}

/// Look up `host:port:database:user:password` in `~/.pgpass`, honouring the
/// `*` wildcard and backslash escapes. Silently ignores a missing file.
fn pgpass_lookup(host: &str, port: u16, db: &str, user: &str) -> Option<String> {
    let path = match std::env::var("PGPASSFILE") {
        Ok(p) => PathBuf::from(p),
        Err(_) => dirs::home_dir()?.join(".pgpass"),
    };
    let contents = std::fs::read_to_string(&path).ok()?;
    let port = port.to_string();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = split_pgpass(line);
        if fields.len() != 5 {
            continue;
        }
        let matches = [
            (&fields[0], host),
            (&fields[1], port.as_str()),
            (&fields[2], db),
            (&fields[3], user),
        ]
        .iter()
        .all(|(pattern, value)| pattern.as_str() == "*" || pattern.as_str() == *value);

        if matches {
            return Some(fields[4].clone());
        }
    }
    None
}

fn split_pgpass(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            fields.last_mut().unwrap().push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == ':' {
            fields.push(String::new());
        } else {
            fields.last_mut().unwrap().push(c);
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_foreign_config_still_parses() {
        // Keys proto knows nothing about must not stop it reading the
        // ones it does.
        let toml = r#"
            [databases.shop]
            host = "localhost"
            name = "shop"
            user = "shop"
            password = "x"
            superuser = "postgres"
            superdb = "postgres"
        "#;
        let config: Config = toml::from_str(toml).expect("unknown keys are ignored");
        assert_eq!(config.databases.len(), 1);
        assert_eq!(config.generate.default_schema, "public");
    }

    #[test]
    fn lone_database_is_the_default() {
        let toml = r#"
            [databases.only]
            name = "only"
            user = "u"
            password = "p"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        let (name, _) = config.select(None, Path::new("test.toml")).unwrap();
        assert_eq!(name, "only");
    }

    #[test]
    fn two_databases_need_a_choice() {
        let toml = r#"
            [databases.a]
            name = "a"
            [databases.b]
            name = "b"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.select(None, Path::new("test.toml")).is_err());
        assert!(config.select(Some("b"), Path::new("test.toml")).is_ok());
    }

    #[test]
    fn pgpass_fields_split_on_escapes() {
        let fields = split_pgpass(r"localhost:5432:my\:db:user:pa\\ss");
        assert_eq!(fields, vec!["localhost", "5432", "my:db", "user", r"pa\ss"]);
    }
}
