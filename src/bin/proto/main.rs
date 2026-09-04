//! `proto` — generate Rust models and mappers from a live PostgreSQL
//! schema.
//!
//! This file is the command-line surface and nothing else: the flags, the
//! subcommands, and the help text a reader sees. What each one does lives
//! in [`run`], and what any of it means lives in the library.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use iridium_proto::render::Strategy;

mod run;

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
pub struct Cli {
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
pub enum SqlStrategy {
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
pub enum Command {
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
    match run::dispatch(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
