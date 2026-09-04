use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use iridium_proto::config::{self, Config, Generate, Target};
use iridium_proto::error::{Error, Result};
use iridium_proto::introspect::{self, Model};
use iridium_proto::naming;
use iridium_proto::output;
use iridium_proto::render::{self, Opts};

#[derive(Parser)]
#[command(
    name = "proto",
    version,
    about = "Generate Rust models from a live PostgreSQL schema",
    long_about = "Generate Rust models from a live PostgreSQL schema.\n\n\
                  Output goes to stdout unless a destination is given, so a \
                  model can be filtered straight into a buffer:\n\n    \
                  :%!proto model arboreal.species --pyo3"
)]
struct Cli {
    /// Database target from the config file
    #[arg(long, short = 'd', global = true, value_name = "TARGET")]
    db: Option<String>,

    /// Config file (default: $PROTO_CONFIG, else ~/.config/proto/proto.toml)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Connection string, bypassing the config file
    #[arg(long, global = true, value_name = "URL")]
    url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate one model: proto model myschema.mytable
    Model {
        /// Table as `schema.table`, or `table` to use the default schema
        table: String,

        /// Emit feature-gated pyo3 attributes
        #[arg(long)]
        pyo3: bool,

        /// Write to this file instead of stdout
        #[arg(long, short = 'o', value_name = "FILE")]
        out: Option<PathBuf>,

        /// Struct name, overriding the one derived from the table name
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Overwrite a destination proto did not generate
        #[arg(long)]
        force: bool,
    },

    /// Generate every model in a schema
    Schema {
        /// Schema name
        schema: String,

        /// Emit feature-gated pyo3 attributes
        #[arg(long)]
        pyo3: bool,

        /// Write one file per table into this directory instead of stdout
        #[arg(long, value_name = "DIR")]
        out_dir: Option<PathBuf>,

        /// Skip the generated mod.rs
        #[arg(long)]
        no_mod: bool,

        /// Overwrite destinations proto did not generate
        #[arg(long)]
        force: bool,
    },

    /// Generate every schema in the database, one directory each
    Database {
        /// Emit feature-gated pyo3 attributes
        #[arg(long)]
        pyo3: bool,

        /// Root directory for the generated tree
        #[arg(long, value_name = "DIR")]
        out_dir: PathBuf,

        /// Skip the generated mod.rs files
        #[arg(long)]
        no_mod: bool,

        /// Overwrite destinations proto did not generate
        #[arg(long)]
        force: bool,
    },

    /// List schemas, or the tables in one schema
    List {
        /// Schema to list; omitted, lists the schemas
        schema: Option<String>,
    },

    /// Show the resolved config file and its targets
    Config,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let path = config::config_path(cli.config.as_deref());
    let file = Config::load(&path)?;

    if let Command::Config = cli.command {
        return show_config(&path, file.as_ref());
    }

    let (target, generate) = resolve(&cli, &path, file)?;
    let pool = connect(&target).await?;

    match &cli.command {
        Command::Model {
            table,
            pyo3,
            out,
            name,
            force,
        } => {
            let (schema, table_name) = split_ref(table, default_schema(&target, &generate))?;
            let model = introspect::model(&pool, &schema, &table_name).await?;
            let opts = Opts {
                generate: &generate,
                pyo3: *pyo3,
                target: &target.label,
                name_override: name.clone(),
                command: command_line(&cli),
            };
            let rendered = render::model_file(&model, &opts, None);
            emit(rendered, out.as_deref(), *force)
        }

        Command::Schema {
            schema,
            pyo3,
            out_dir,
            no_mod,
            force,
        } => {
            let models = read_schema(&pool, schema, &generate).await?;
            let opts = Opts {
                generate: &generate,
                pyo3: *pyo3,
                target: &target.label,
                name_override: None,
                command: command_line(&cli),
            };
            match out_dir {
                None => emit(render::schema_file(&models, schema, &opts), None, *force),
                Some(dir) => write_schema(&models, schema, &opts, dir, *no_mod, *force),
            }
        }

        Command::Database {
            pyo3,
            out_dir,
            no_mod,
            force,
        } => {
            let opts = Opts {
                generate: &generate,
                pyo3: *pyo3,
                target: &target.label,
                name_override: None,
                command: command_line(&cli),
            };
            let mut written = Vec::new();
            for schema in introspect::schemas(&pool).await? {
                if generate.exclude_schemas.iter().any(|s| s == &schema) {
                    continue;
                }
                let models = read_schema(&pool, &schema, &generate).await?;
                if models.is_empty() {
                    continue;
                }
                let dir = out_dir.join(naming::ident(&schema));
                write_schema(&models, &schema, &opts, &dir, *no_mod, *force)?;
                written.push(schema);
            }
            if !*no_mod && !written.is_empty() {
                let modules: Vec<String> = written.iter().map(|s| naming::ident(s)).collect();
                let code = render::mod_file(&modules, "database", &opts);
                output::write_file(&out_dir.join("mod.rs"), &code, *force)?;
            }
            eprintln!("wrote {} schemas to {}", written.len(), out_dir.display());
            Ok(())
        }

        Command::List { schema } => match schema {
            Some(schema) => {
                for table in introspect::tables(&pool, schema).await? {
                    println!("{schema}.{table}");
                }
                Ok(())
            }
            None => {
                for schema in introspect::schemas(&pool).await? {
                    if !generate.exclude_schemas.iter().any(|s| s == &schema) {
                        println!("{schema}");
                    }
                }
                Ok(())
            }
        },

        Command::Config => unreachable!("handled above"),
    }
}

// ── Wiring ──────────────────────────────────────────────────────────────────

fn resolve(cli: &Cli, path: &std::path::Path, file: Option<Config>) -> Result<(Target, Generate)> {
    // An explicit URL wins, then DATABASE_URL, then the config file.
    let generate = file
        .as_ref()
        .map(|c| c.generate.clone())
        .unwrap_or_default();

    if let Some(url) = &cli.url {
        return Ok((Target::from_url(url, "--url")?, generate));
    }

    if let Some(config) = &file {
        let requested = cli.db.as_deref();
        // DATABASE_URL only stands in when no target was asked for and the
        // file cannot pick one on its own.
        match config.select(requested, path) {
            Ok((name, target)) => return Ok((Target::from_config(name, target)?, generate)),
            Err(e) => {
                if requested.is_none()
                    && let Ok(url) = std::env::var("DATABASE_URL")
                {
                    return Ok((Target::from_url(&url, "DATABASE_URL")?, generate));
                }
                return Err(e);
            }
        }
    }

    if let Ok(url) = std::env::var("DATABASE_URL") {
        return Ok((Target::from_url(&url, "DATABASE_URL")?, generate));
    }

    Err(Error::NoConfig {
        path: path.to_path_buf(),
    })
}

async fn connect(target: &Target) -> Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(2)
        .connect_with(target.options.clone())
        .await?)
}

fn default_schema(target: &Target, generate: &Generate) -> String {
    target
        .schema
        .clone()
        .unwrap_or_else(|| generate.default_schema.clone())
}

fn split_ref(reference: &str, default: String) -> Result<(String, String)> {
    let parts: Vec<&str> = reference.split('.').collect();
    match parts.as_slice() {
        [table] if !table.is_empty() => Ok((default, (*table).to_string())),
        [schema, table] if !schema.is_empty() && !table.is_empty() => {
            Ok(((*schema).to_string(), (*table).to_string()))
        }
        _ => Err(Error::BadTableRef(reference.to_string())),
    }
}

async fn read_schema(pool: &PgPool, schema: &str, generate: &Generate) -> Result<Vec<Model>> {
    let mut models = Vec::new();
    for table in introspect::tables(pool, schema).await? {
        let qualified = format!("{schema}.{table}");
        if generate
            .exclude_tables
            .iter()
            .any(|t| t == &table || t == &qualified)
        {
            continue;
        }
        models.push(introspect::model(pool, schema, &table).await?);
    }
    Ok(models)
}

fn write_schema(
    models: &[Model],
    schema: &str,
    opts: &Opts,
    dir: &std::path::Path,
    no_mod: bool,
    force: bool,
) -> Result<()> {
    let enum_path = if render::uses_enums(models) {
        Some("super::enums")
    } else {
        None
    };

    let mut modules = Vec::new();
    if enum_path.is_some() {
        let rendered = render::enums_file(models, schema, opts);
        output::write_file(&dir.join("enums.rs"), &rendered.code, force)?;
        modules.push("enums".to_string());
    }

    for model in models {
        let rendered = render::model_file(model, opts, enum_path);
        for warning in &rendered.warnings {
            output::warn(warning);
        }
        let module = naming::ident(&model.table.name);
        output::write_file(&dir.join(format!("{module}.rs")), &rendered.code, force)?;
        modules.push(module);
    }

    if !no_mod {
        let code = render::mod_file(&modules, schema, opts);
        output::write_file(&dir.join("mod.rs"), &code, force)?;
    }

    eprintln!("wrote {} models to {}", models.len(), dir.display());
    Ok(())
}

fn emit(rendered: render::Rendered, out: Option<&std::path::Path>, force: bool) -> Result<()> {
    for warning in &rendered.warnings {
        output::warn(warning);
    }
    match out {
        Some(path) => {
            output::write_file(path, &rendered.code, force)?;
            eprintln!("wrote {}", path.display());
            Ok(())
        }
        None => output::write_stdout(&rendered.code),
    }
}

/// Reconstruct the invocation for the generated header. Built from parsed
/// arguments rather than argv so a `--url` never lands in a source file.
fn command_line(cli: &Cli) -> String {
    let mut parts = vec!["proto".to_string()];
    match &cli.command {
        Command::Model {
            table, pyo3, name, ..
        } => {
            parts.push("model".into());
            parts.push(table.clone());
            if *pyo3 {
                parts.push("--pyo3".into());
            }
            if let Some(name) = name {
                parts.push(format!("--name {name}"));
            }
        }
        Command::Schema { schema, pyo3, .. } => {
            parts.push("schema".into());
            parts.push(schema.clone());
            if *pyo3 {
                parts.push("--pyo3".into());
            }
        }
        Command::Database { pyo3, .. } => {
            parts.push("database".into());
            if *pyo3 {
                parts.push("--pyo3".into());
            }
        }
        // Neither reads a table, so neither ends up in a header.
        Command::List { .. } | Command::Config => {}
    }
    if let Some(db) = &cli.db
        && cli.url.is_none()
    {
        parts.push(format!("--db {db}"));
    }
    parts.join(" ")
}

fn show_config(path: &std::path::Path, file: Option<&Config>) -> Result<()> {
    println!("config: {}", path.display());
    match file {
        None => {
            println!("  (not found)");
            if std::env::var("DATABASE_URL").is_ok() {
                println!("  DATABASE_URL is set and will be used");
            }
        }
        Some(config) => {
            let default = config
                .default_db
                .clone()
                .or_else(|| {
                    (config.databases.len() == 1)
                        .then(|| config.databases.keys().next().cloned())
                        .flatten()
                })
                .unwrap_or_else(|| "(none)".to_string());
            println!("  default: {default}");
            println!("  targets:");
            for (name, target) in &config.databases {
                let host = target.host.as_deref().unwrap_or("localhost");
                let db = target.name.as_deref().unwrap_or(name.as_str());
                println!("    {name}  ->  {host}/{db}");
            }
            let g = &config.generate;
            println!("  default schema: {}", g.default_schema);
            println!("  pyo3 feature:   {}", g.pyo3_feature);
            println!("  derives:        {}", g.derives.join(", "));
            if !g.types.is_empty() {
                let mut keys: Vec<&String> = g.types.keys().collect();
                keys.sort();
                println!(
                    "  type overrides: {}",
                    keys.iter()
                        .map(|k| format!("{k}={}", g.types[*k]))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    Ok(())
}
