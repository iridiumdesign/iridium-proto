//! Generate Rust models and mappers from a live PostgreSQL schema.
//!
//! `iridium-proto` reads the Postgres catalogs and writes the Rust you
//! would otherwise write by hand: a struct per table, the Rust enums
//! behind its enum types, and a repository that runs the queries. It is
//! primarily a command-line tool, `proto`, with the library exposed so the
//! same work can be done from a build script or a test.
//!
//! # The command
//!
//! ```text
//! proto model shop.product --pyo3      # one model, to stdout
//! proto mapper shop.product            # its repository
//! proto schema shop --out-dir src/model
//! proto database --out-dir src/model --mapper-dir src/mapper --mappers
//! proto list [schema]                  # what is there
//! proto config                         # what proto resolved
//! ```
//!
//! Output goes to stdout unless a destination is given, which is what
//! makes `:%!proto model shop.product` work from an editor buffer.
//!
//! # Generated code you can live in
//!
//! A regeneration corrects a file rather than replacing it. Proto
//! renders what the file should say, parses both that and what is on
//! disk, and edits only where they disagree — so a field whose type no
//! longer matches its column is replaced, and the comment a developer
//! wrote above it stays above it.
//!
//! The database is right about the parts it owns: names, types,
//! nullability. Everything else in the file is yours, including the
//! order, since fields are matched by name. Nothing has to be marked to
//! be spared, because nothing is rewritten. See [`reconcile`].
//!
//! # What comes out
//!
//! A table becomes a struct. `NOT NULL` columns get a bare type and
//! everything else gets [`Option`]; column comments become doc comments.
//!
//! ```text
//! #[derive(sqlx::FromRow, Debug, Clone, Serialize, Deserialize)]
//! pub struct Product {
//!     pub id: Uuid,
//!     pub slug: String,
//!     pub status: ProductStatus,
//!     pub price: Option<Decimal>,
//! }
//! ```
//!
//! Its mapper owns the SQL:
//!
//! ```text
//! let products = ProductMapper::new(&pool);
//! let one = products.find_by_slug("dovetail-saw").await?;
//! let all = products.find_by_org_id(org).await?;
//! ```
//!
//! # Two places for the SQL
//!
//! [`render::Strategy`] decides where the statements live. Under
//! [`Embedded`](render::Strategy::Embedded) they are in the Rust source.
//! Under [`Server`](render::Strategy::Server) they are `LANGUAGE sql`
//! functions in a migration [`render::sql`] writes, and the mapper only
//! calls them. Both produce the same Rust API, so the choice is about
//! where the logic should live, not about how the code is used.
//!
//! # Using it as a library
//!
//! [`introspect`] reads a table, [`render`] turns it into source. Nothing
//! in [`render`] touches a database, so the emitters are testable on their
//! own.
//!
//! ```no_run
//! use iridium_proto::config::Generate;
//! use iridium_proto::{introspect, render};
//! use sqlx::PgPool;
//!
//! # async fn example(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
//! let model = introspect::model(pool, "shop", "product").await?;
//!
//! let generate = Generate::default();
//! let opts = render::Opts {
//!     generate: &generate,
//!     pyo3: false,
//!     inputs: true,
//!     model_path: "crate::model".to_string(),
//!     strategy: render::Strategy::Embedded,
//!     target: "dev",
//!     command: "proto model shop.product".to_string(),
//!     name_override: None,
//! };
//!
//! print!("{}", render::model::model_file(&model, &opts, None).code);
//! print!("{}", render::mapper::mapper_file(&model, &opts).code);
//! # Ok(())
//! # }
//! ```
//!
//! # Configuration
//!
//! Database targets and generation defaults come from a TOML file, read
//! from `$PROTO_CONFIG`, else `$XDG_CONFIG_HOME/proto/proto.toml`, else
//! `~/.config/proto/proto.toml`. See [`config`] for the shape of it.

#![warn(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod config;
pub mod error;
pub mod inspect;
pub mod introspect;
pub mod naming;
pub mod output;
pub mod quoting;
pub mod reconcile;
pub mod render;
pub mod typemap;
