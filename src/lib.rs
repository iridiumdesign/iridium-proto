//! proto — generate Rust models from a live PostgreSQL schema.
//!
//! The crate is split so the pieces stay testable on their own:
//! [`introspect`] reads the catalogs, [`typemap`] decides the Rust type,
//! [`render`] writes source, and nothing in [`render`] touches a database.

pub mod config;
pub mod error;
pub mod introspect;
pub mod naming;
pub mod output;
pub mod render;
pub mod typemap;
