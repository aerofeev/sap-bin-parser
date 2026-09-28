//! Convert SAP fixed-width binary table exports (`.BIN`) into CSV, JSON Lines
//! or Parquet.
//!
//! An export arrives as a zip of per-shard zips holding `DATA.N.BIN` files:
//! no delimiter, no header row, UTF-16BE text, COMP-3 packed decimals, and
//! records padded to an even byte boundary. This crate reads that format as a
//! stream (an upload, stdin, or a file) without unpacking anything to disk,
//! decodes it in parallel per shard, and streams the result out.
//!
//! The crate is the engine behind three front doors that share every option:
//! the `sap-bin` command line, the local app (`sap-bin app`), and the web
//! service (`sap-bin serve`). The Python package in the same repository is the
//! reference implementation; the two are held to identical output in CI.

pub mod archive;
pub mod convert;
pub mod decode;
pub mod error;
pub mod files;
pub mod inspect;
pub mod probe;
pub mod sample;
pub mod schema;
pub mod server;
pub mod text;
pub mod usage;
pub mod writer;
pub mod zip;

pub use convert::{Compression, DecimalMode, Format, OnError, Options, Stats};
pub use error::{Error, Result};
pub use schema::{Field, FieldType, Schema};

/// Crate version, as reported by `sap-bin --version` and the `/healthz` endpoint.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
