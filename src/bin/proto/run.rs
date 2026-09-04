//! What each subcommand does.
//!
//! Everything here is plumbing between the library and the filesystem:
//! resolve a target, read the catalogs, render, and put the result where
//! it was asked to go. The decisions all live in `iridium_proto`.

use std::path::Path;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use iridium_proto::config::{self, Config, Generate, Target};
use iridium_proto::error::{Error, Result};
use iridium_proto::introspect::{self, Model};
use iridium_proto::naming;
use iridium_proto::output::{self, Journal, Migration};
use iridium_proto::render::python::Group;
use iridium_proto::render::{self, Opts, Rendered, Strategy};

use crate::{Cli, Command, SqlStrategy};

pub async fn dispatch(cli: Cli) -> Result<()> {
    let path = config::config_path(cli.config.as_deref());
    let file = Config::load(&path)?;

    if let Command::Config = cli.command {
        return show_config(&path, file.as_ref());
    }

    let (target, generate) = resolve(&cli, &path, file)?;
    let pool = connect(&target).await?;
    let mut journal = Journal::new(cli.check);

    let outcome = run(&cli, &pool, &target, &generate, &mut journal).await;

    // A run that touched files says what it did, even when it failed
    // partway: knowing which half landed is the point of the summary.
    if !journal.summary().is_empty() {
        eprint!("{}", journal.summary());
    }
    outcome?;

    if cli.check && journal.changed() {
        return Err(Error::Drift);
    }
    Ok(())
}

async fn run(
    cli: &Cli,
    pool: &PgPool,
    target: &Target,
    generate: &Generate,
    journal: &mut Journal,
) -> Result<()> {
    match &cli.command {
        Command::Model {
            table,
            pyo3,
            input,
            out,
            name,
            force,
        } => {
            let (schema, relation) = split_ref(table, default_schema(target, generate))?;
            let model = introspect::model(pool, &schema, &relation).await?;
            let opts = options(cli, generate, target, *pyo3, *input, name.clone());
            emit(
                journal,
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
            let (schema, relation) = split_ref(table, default_schema(target, generate))?;
            let model = introspect::model(pool, &schema, &relation).await?;
            let opts = options(cli, generate, target, false, true, name.clone());
            if opts.strategy == Strategy::Server {
                let dir = migrations_dir(cli)?;
                report(output::write_migration(
                    journal,
                    dir,
                    &migration_slug(&model),
                    &render::sql::migration_file(&model, &opts),
                )?);
            }
            emit(
                journal,
                render::mapper::mapper_file(&model, &opts),
                out.as_deref(),
                *force,
            )
        }

        Command::Schema {
            schema,
            pyo3,
            mappers,
            pymodule,
            out_dir,
            mapper_dir,
            prune,
            no_mod,
            force,
        } => {
            let models = read_schema(pool, schema, generate).await?;
            let opts = options(cli, generate, target, *pyo3, *mappers, None);
            match out_dir {
                Some(dir) => {
                    let mappers = mapper_target(cli, *mappers, mapper_dir.as_deref())?;
                    let python = python_target(pymodule.as_deref(), *pyo3)?;
                    if let Some(name) = python {
                        let groups = [Group {
                            schema: schema.clone(),
                            path: "super".to_string(),
                            models: &models,
                        }];
                        let code = render::python::pymodule_file(name, &groups, &opts);
                        journal.write(&dir.join("python.rs"), &code, *force)?;
                    }
                    let feature = python.map(|_| opts.generate.pyo3_feature.as_str());
                    write_schema(
                        journal, &models, schema, &opts, dir, mappers, feature, *prune, *no_mod,
                        *force,
                    )
                }
                None if *mappers => Err(Error::Usage(
                    "--mappers needs --out-dir and --mapper-dir".to_string(),
                )),
                None => emit(
                    journal,
                    render::model::schema_file(&models, schema, &opts),
                    None,
                    *force,
                ),
            }
        }

        Command::Database {
            pyo3,
            mappers,
            pymodule,
            out_dir,
            mapper_dir,
            prune,
            no_mod,
            force,
        } => {
            let opts = options(cli, generate, target, *pyo3, *mappers, None);
            let mappers = mapper_target(cli, *mappers, mapper_dir.as_deref())?;
            let python = python_target(pymodule.as_deref(), *pyo3)?;
            let mut written = Vec::new();
            let mut groups = Vec::new();

            for schema in introspect::schemas(pool).await? {
                if generate.exclude_schemas.iter().any(|s| s == &schema) {
                    continue;
                }
                let models = read_schema(pool, &schema, generate).await?;
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
                    journal,
                    &models,
                    &schema,
                    &opts,
                    &out_dir.join(&module),
                    per_schema.as_ref().map(|(d, m)| (d.as_path(), *m)),
                    None,
                    *prune,
                    *no_mod,
                    *force,
                )?;
                groups.push((schema.clone(), module.clone(), models));
                written.push(module);
            }

            if let Some(name) = python {
                let groups: Vec<Group> = groups
                    .iter()
                    .map(|(schema, module, models)| Group {
                        schema: schema.clone(),
                        path: format!("super::{module}"),
                        models,
                    })
                    .collect();
                let code = render::python::pymodule_file(name, &groups, &opts);
                journal.write(&out_dir.join("python.rs"), &code, *force)?;
            }

            if !*no_mod && !written.is_empty() {
                let feature = python.map(|_| opts.generate.pyo3_feature.as_str());
                let code = render::model::mod_file(&written, "database", &opts, feature);
                journal.write(&out_dir.join("mod.rs"), &code, *force)?;
                if let Some((dir, _)) = mappers {
                    let code = render::model::mod_file(&written, "database", &opts, None);
                    journal.write(&dir.join("mod.rs"), &code, *force)?;
                }
            }
            Ok(())
        }

        Command::List { schema } => match schema {
            Some(schema) => {
                for table in introspect::tables(pool, schema).await? {
                    println!("{schema}.{table}");
                }
                Ok(())
            }
            None => {
                for schema in introspect::schemas(pool).await? {
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

/// A `#[pymodule]` is only meaningful over classes that carry pyo3
/// attributes, so it needs `--pyo3` to have been asked for too.
fn python_target(name: Option<&str>, pyo3: bool) -> Result<Option<&str>> {
    match name {
        Some(_) if !pyo3 => Err(Error::Usage(
            "--pymodule needs --pyo3: there would be no classes to register".to_string(),
        )),
        other => Ok(other),
    }
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
    journal: &mut Journal,
    models: &[Model],
    schema: &str,
    opts: &Opts,
    dir: &Path,
    mappers: Option<(&Path, Option<&Path>)>,
    python: Option<&str>,
    prune: bool,
    no_mod: bool,
    force: bool,
) -> Result<()> {
    let enum_path = render::uses_enums(models).then_some("super::enums");

    let mut modules = Vec::new();
    if enum_path.is_some() {
        let rendered = render::model::enums_file(models, schema, opts);
        journal.write(&dir.join("enums.rs"), &rendered.code, force)?;
        modules.push("enums".to_string());
    }

    for model in models {
        let rendered = render::model::model_file(model, opts, enum_path);
        for warning in &rendered.warnings {
            output::warn(warning);
        }
        let module = naming::ident(&model.table.name);
        journal.write(&dir.join(format!("{module}.rs")), &rendered.code, force)?;
        modules.push(module);
    }

    if !no_mod {
        let code = render::model::mod_file(&modules, schema, opts, python);
        journal.write(&dir.join("mod.rs"), &code, force)?;
    }
    if prune {
        let mut kept: Vec<String> = modules.iter().map(|m| format!("{m}.rs")).collect();
        kept.push("mod.rs".to_string());
        kept.push("python.rs".to_string());
        prune_dir(journal, dir, &kept)?;
    }

    if let Some((mapper_dir, migrations)) = mappers {
        let mut written = Vec::new();
        for model in models {
            let rendered = render::mapper::mapper_file(model, opts);
            let module = naming::ident(&model.table.name);
            journal.write(
                &mapper_dir.join(format!("{module}.rs")),
                &rendered.code,
                force,
            )?;
            written.push(module);
        }
        if !no_mod {
            let code = render::model::mod_file(&written, schema, opts, None);
            journal.write(&mapper_dir.join("mod.rs"), &code, force)?;
        }
        if prune {
            let mut kept: Vec<String> = written.iter().map(|m| format!("{m}.rs")).collect();
            kept.push("mod.rs".to_string());
            prune_dir(journal, mapper_dir, &kept)?;
        }

        if let Some(migrations) = migrations {
            for model in models {
                report(output::write_migration(
                    journal,
                    migrations,
                    &migration_slug(model),
                    &render::sql::migration_file(model, opts),
                )?);
            }
        }
    }

    Ok(())
}

/// Remove generated files in `dir` that are not in `kept`.
///
/// Only files carrying the generated marker are touched, so a directory
/// someone also keeps hand-written code in survives a prune intact.
fn prune_dir(journal: &mut Journal, dir: &Path, kept: &[String]) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::ReadFile {
                path: dir.to_path_buf(),
                source,
            });
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.extension().is_some_and(|e| e == "rs") && !kept.iter().any(|k| k == name) {
            journal.remove(&path)?;
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

fn emit(journal: &mut Journal, rendered: Rendered, out: Option<&Path>, force: bool) -> Result<()> {
    for warning in &rendered.warnings {
        output::warn(warning);
    }
    match out {
        Some(path) => {
            journal.write(path, &rendered.code, force)?;
            Ok(())
        }
        // Nothing to compare against and nothing to keep in step, so a
        // stream is written whatever `--check` says.
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
