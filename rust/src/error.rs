//! One error type for the whole engine.
//!
//! The variants map onto what a user can do about the problem, which is also
//! how the CLI picks exit codes and the server picks HTTP statuses: a bad
//! schema or archive is the input's fault (2 / 400), a record that will not
//! decode almost always means the record size is wrong (1 / 422), and I/O is
//! the environment's fault.

use std::fmt;

/// The decoded value of a field could not be produced from its bytes.
///
/// Nearly always means the record size is wrong and fields are being read at
/// the wrong offsets, rather than that the data itself is corrupt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The schema sidecar could not be understood.
    #[error("{0}")]
    Schema(String),

    /// The archive did not have the expected shape.
    #[error("{0}")]
    Archive(String),

    /// A record could not be decoded. Carries the record's index within its
    /// shard and the field that failed, when known.
    #[error("{message}")]
    Record {
        message: String,
        record_index: u64,
        field: Option<String>,
    },

    /// A limit configured by the operator was exceeded.
    #[error("{0}")]
    Limit(String),

    /// The conversion was cancelled (client went away, Ctrl+C).
    #[error("cancelled")]
    Cancelled,

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Parquet(#[from] parquet::errors::ParquetError),

    #[error("{0}")]
    Arrow(#[from] arrow_schema::ArrowError),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The exit code the CLI uses for this error, matching the Python CLI.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Record { .. } => 1,
            Error::Schema(_) | Error::Archive(_) | Error::Limit(_) => 2,
            Error::Cancelled => 130,
            Error::Io(_) | Error::Parquet(_) | Error::Arrow(_) => 1,
        }
    }

    /// A hint printed under the error when it helps, matching the Python CLI.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Error::Record { .. } => Some(
                "hint: if this is the first record, the record size is probably wrong \
                 — try `sap-bin probe`.",
            ),
            _ => None,
        }
    }

    pub(crate) fn record(message: impl Into<String>, record_index: u64, field: Option<&str>) -> Self {
        Error::Record {
            message: message.into(),
            record_index,
            field: field.map(str::to_owned),
        }
    }
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::Io(io) => io,
            other => std::io::Error::other(other),
        }
    }
}
