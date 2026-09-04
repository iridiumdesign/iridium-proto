//! `proto` — generate Rust models and mappers from a live PostgreSQL
//! schema. See the crate documentation for what the generated code looks
//! like; this file is the command-line surface over it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use iridium_proto::config::{self, Config, Generate, Target};
use iridium_proto::error::{Error, Result};
use iridium_proto::introspect::{self, Model};
use iridium_proto::naming;
use iridium_proto::output::{self, Migration};
use iridium_proto::render::{self, Opts, Rendered, Strategy};

#[derive(Parser)]
#[command(
    name = "proto",
    version,
    about = "Generate Rust models and mappers from a live PostgreSQL schema",
    long_about = "Generate Rust models and mappers from a live PostgreSQL schema.\n\n\
                  Output goes to stdout unless a destination is given, so a \
                  model can be filtered straight into a buffer:\n\n    \
                  :%!proto model shop.product --pyo3"
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

    /// Where a mapper's SQL lives: in the Rust source, or in Postgres
    /// functions a migration defines
    #[arg(long, global = true, value_name = "WHERE", default_value = "embedded")]
    sql: SqlStrategy,

    /// Module the mappers import the model types from
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        default_value = "crate::model"
    )]
    model_path: String,

    /// Directory for the migrations `--sql server` generates
    #[arg(long, global = true, value_name = "DIR")]
    migrations_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SqlStrategy {
    /// Statements live in the generated Rust
    Embedded,
    /// Statements live in Postgres functions the mapper calls
    Server,
}

impl From<SqlStrategy> for Strategy {
    fn from(value: SqlStrategy) -> Self {
        match value {
            SqlStrategy::Embedded => Strategy::Embedded,
            SqlStrategy::Server => Strategy::Server,
        }
    }
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

        /// Also emit the `New…` insert input type
        #[arg(long)]
        input: bool,

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

    /// Generate one mapper: proto mapper myschema.mytable
    Mapper {
        /// Table as `schema.table`, or `table` to use the default schema
        table: String,

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

        /// Also generate mappers; requires --out-dir and --mapper-dir
        #[arg(long)]
        mappers: bool,

        /// Write one model file per table into this directory
        #[arg(long, value_name = "DIR")]
        out_dir: Option<PathBuf>,

        /// Write one mapper file per table into this directory
        #[arg(long, value_name = "DIR")]
        mapper_dir: Option<PathBuf>,

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

        /// Also generate mappers; requires --mapper-dir
        #[arg(long)]
        mappers: bool,

        /// Root directory for the generated models
        #[arg(long, value_name = "DIR")]
        out_dir: PathBuf,

        /// Root directory for the generated mappers
        #[arg(long, value_name = "DIR")]
        mapper_dir: Option<PathBuf>,

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
            input,
            out,
            name,
            force,
        } => {
            let (schema, relation) = split_ref(table, default_schema(&target, &generate))?;
            let model = introspect::model(&pool, &schema, &relation).await?;
            let opts = options(&cli, &generate, &target, *pyo3, *input, name.clone());
            emit(
                render::model::model_file(&model, &opts, None),
                out.as_deref(),
                *force,
            )
        }

        Command::Mapper {
            table,
            out,
            name,
            force,
        } => {
            let (schema, relation) = split_ref(table, default_schema(&target, &generate))?;
            let model = introspect::model(&pool, &schema, &relation).await?;
            let opts = options(&cli, &generate, &target, false, true, name.clone());
            if opts.strategy == Strategy::Server {
                let dir = migrations_dir(&cli)?;
                report(output::write_migration(
                    dir,
                    &migration_slug(&model),
                    &render::sql::migration_file(&model, &opts),
                )?);
            }
            emit(
                render::mapper::mapper_file(&model, &opts),
                out.as_deref(),
                *force,
            )
        }

        Command::Schema {
            schema,
            pyo3,
            mappers,
            out_dir,
            mapper_dir,
            no_mod,
            force,
        } => {
            let models = read_schema(&pool, schema, &generate).await?;
            let opts = options(&cli, &generate, &target, *pyo3, *mappers, None);
            match out_dir {
                Some(dir) => {
                    let mappers = mapper_target(&cli, *mappers, mapper_dir.as_deref())?;
                    write_schema(&models, schema, &opts, dir, mappers, *no_mod, *force)
                }
                None if *mappers => Err(Error::Usage(
                    "--mappers needs --out-dir and --mapper-dir".to_string(),
                )),
                None => emit(
                    render::model::schema_file(&models, schema, &opts),
                    None,
                    *force,
                ),
            }
        }

        Command::Database {
            pyo3,
            mappers,
            out_dir,
            mapper_dir,
            no_mod,
            force,
        } => {
            let opts = options(&cli, &generate, &target, *pyo3, *mappers, None);
            let mappers = mapper_target(&cli, *mappers, mapper_dir.as_deref())?;
            let mut written = Vec::new();

            for schema in introspect::schemas(&pool).await? {
                if generate.exclude_schemas.iter().any(|s| s == &schema) {
                    continue;
                }
                let models = read_schema(&pool, &schema, &generate).await?;
                if models.is_empty() {
                    continue;
                }
                let module = naming::ident(&schema);
                // proto chose these directory names, so it also knows the
                // module path the mappers must import their models from.
                let mut opts = opts.clone();
                opts.model_path = format!("{}::{module}", cli.model_path);
                // Models and mappers split by schema; migrations all land
                // in the one directory, numbered in sequence.
                let per_schema = mappers.map(|(dir, migrations)| (dir.join(&module), migrations));
                write_schema(
                    &models,
                    &schema,
                    &opts,
                    &out_dir.join(&module),
                    per_schema.as_ref().map(|(d, m)| (d.as_path(), *m)),
                    *no_mod,
                    *force,
                )?;
                written.push(module);
            }

            if !*no_mod && !written.is_empty() {
                let code = render::model::mod_file(&written, "database", &opts);
                output::write_file(&out_dir.join("mod.rs"), &code, *force)?;
                if let Some((dir, _)) = mappers {
                    let code = render::model::mod_file(&written, "database", &opts);
                    output::write_file(&dir.join("mod.rs"), &code, *force)?;
                }
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

fn options<'a>(
    cli: &'a Cli,
    generate: &'a Generate,
    target: &'a Target,
    pyo3: bool,
    inputs: bool,
    name_override: Option<String>,
) -> Opts<'a> {
    Opts {
        generate,
        pyo3,
        inputs,
        model_path: cli.model_path.clone(),
        strategy: cli.sql.into(),
        target: &target.label,
        name_override,
        command: command_line(cli),
    }
}

fn resolve(cli: &Cli, path: &Path, file: Option<Config>) -> Result<(Target, Generate)> {
    // An explicit URL wins, then the config file, then DATABASE_URL.
    let generate = file
        .as_ref()
        .map(|c| c.generate.clone())
        .unwrap_or_default();

    if let Some(url) = &cli.url {
        return Ok((Target::from_url(url, "--url")?, generate));
    }

    if let Some(config) = &file {
        let requested = cli.db.as_deref();
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
        .after_connect(|conn, _| {
            Box::pin(async move {
                // An empty search_path makes format_type and pg_get_expr
                // spell out every schema, so a generated cast such as
                // `'draft'::shop.product_status` does not depend on the
                // search_path of whoever runs it later.
                sqlx::query("SET search_path TO ''")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
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

/// Where mappers go, and where their migrations go when the statements
/// live on the server. Both are resolved before anything is written, so a
/// missing flag fails before it leaves half a tree on disk.
fn mapper_target<'a>(
    cli: &'a Cli,
    mappers: bool,
    dir: Option<&'a Path>,
) -> Result<Option<(&'a Path, Option<&'a Path>)>> {
    if !mappers {
        return Ok(None);
    }
    let Some(dir) = dir else {
        return Err(Error::Usage("--mappers needs --mapper-dir".to_string()));
    };
    let migrations = match Strategy::from(cli.sql) {
        Strategy::Server => Some(migrations_dir(cli)?),
        Strategy::Embedded => None,
    };
    Ok(Some((dir, migrations)))
}

fn migrations_dir(cli: &Cli) -> Result<&Path> {
    cli.migrations_dir
        .as_deref()
        .ok_or_else(|| Error::Usage("--sql server needs --migrations-dir".to_string()))
}

fn migration_slug(model: &Model) -> String {
    format!(
        "{}_{}_crud",
        naming::ident(&model.table.schema),
        naming::ident(&model.table.name)
    )
}

#[allow(clippy::too_many_arguments)]
fn write_schema(
    models: &[Model],
    schema: &str,
    opts: &Opts,
    dir: &Path,
    mappers: Option<(&Path, Option<&Path>)>,
    no_mod: bool,
    force: bool,
) -> Result<()> {
    let enum_path = render::uses_enums(models).then_some("super::enums");

    let mut modules = Vec::new();
    if enum_path.is_some() {
        let rendered = render::model::enums_file(models, schema, opts);
        output::write_file(&dir.join("enums.rs"), &rendered.code, force)?;
        modules.push("enums".to_string());
    }

    for model in models {
        let rendered = render::model::model_file(model, opts, enum_path);
        for warning in &rendered.warnings {
            output::warn(warning);
        }
        let module = naming::ident(&model.table.name);
        output::write_file(&dir.join(format!("{module}.rs")), &rendered.code, force)?;
        modules.push(module);
    }

    if !no_mod {
        let code = render::model::mod_file(&modules, schema, opts);
        output::write_file(&dir.join("mod.rs"), &code, force)?;
    }
    eprintln!("wrote {} models to {}", models.len(), dir.display());

    if let Some((mapper_dir, migrations)) = mappers {
        let mut written = Vec::new();
        for model in models {
            let rendered = render::mapper::mapper_file(model, opts);
            let module = naming::ident(&model.table.name);
            output::write_file(
                &mapper_dir.join(format!("{module}.rs")),
                &rendered.code,
                force,
            )?;
            written.push(module);
        }
        if !no_mod {
            let code = render::model::mod_file(&written, schema, opts);
            output::write_file(&mapper_dir.join("mod.rs"), &code, force)?;
        }
        eprintln!(
            "wrote {} mappers to {}",
            written.len(),
            mapper_dir.display()
        );

        if let Some(migrations) = migrations {
            for model in models {
                report(output::write_migration(
                    migrations,
                    &migration_slug(model),
                    &render::sql::migration_file(model, opts),
                )?);
            }
        }
    }

    Ok(())
}

fn report(migration: Migration) {
    match migration {
        Migration::Written(path) => eprintln!("wrote {}", path.display()),
        Migration::Unchanged(path) => {
            eprintln!("unchanged, kept {}", path.display());
        }
    }
}

fn emit(rendered: Rendered, out: Option<&Path>, force: bool) -> Result<()> {
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
            table,
            pyo3,
            input,
            name,
            ..
        } => {
            parts.push("model".into());
            parts.push(table.clone());
            if *pyo3 {
                parts.push("--pyo3".into());
            }
            if *input {
                parts.push("--input".into());
            }
            if let Some(name) = name {
                parts.push(format!("--name {name}"));
            }
        }
        Command::Mapper { table, name, .. } => {
            parts.push("mapper".into());
            parts.push(table.clone());
            if let Some(name) = name {
                parts.push(format!("--name {name}"));
            }
        }
        Command::Schema {
            schema,
            pyo3,
            mappers,
            ..
        } => {
            parts.push("schema".into());
            parts.push(schema.clone());
            if *pyo3 {
                parts.push("--pyo3".into());
            }
            if *mappers {
                parts.push("--mappers".into());
            }
        }
        Command::Database { pyo3, mappers, .. } => {
            parts.push("database".into());
            if *pyo3 {
                parts.push("--pyo3".into());
            }
            if *mappers {
                parts.push("--mappers".into());
            }
        }
        // Neither reads a table, so neither ends up in a header.
        Command::List { .. } | Command::Config => {}
    }
    if cli.sql == SqlStrategy::Server {
        parts.push("--sql server".into());
    }
    if let Some(db) = &cli.db
        && cli.url.is_none()
    {
        parts.push(format!("--db {db}"));
    }
    parts.join(" ")
}

fn show_config(path: &Path, file: Option<&Config>) -> Result<()> {
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
