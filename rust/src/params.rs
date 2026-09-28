//! Conversion parameters by name, shared by the web service and the
//! WebAssembly build, so both accept exactly the same options.

use serde::Deserialize;

use crate::convert::{Format, InputKind, OnError, Options};
use crate::decode::DecimalMode;
use crate::writer::Compression;

/// The parameters of a conversion, as the HTTP API takes them (the query of
/// `api/convert`) and as the page passes them to the WebAssembly build. They
/// mirror the CLI flags.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConvertParams {
    pub format: Option<String>,
    pub delimiter: Option<String>,
    pub bom: Option<bool>,
    pub decimals: Option<String>,
    pub on_error: Option<String>,
    pub record_size: Option<usize>,
    pub text_encoding: Option<String>,
    pub compression: Option<String>,
    pub limit: Option<u64>,
    pub split: Option<bool>,
    pub input: Option<String>,
    pub name: Option<String>,
    /// The web service's chunked job this conversion belongs to.
    pub job: Option<String>,
    /// Each uploaded `file` part is one member of the export (shards, and
    /// optionally the sidecar), rather than the whole export.
    pub multi: Option<bool>,
}

impl ConvertParams {
    /// The conversion options these parameters describe, with `threads`
    /// worker threads. The error says which parameter is wrong.
    pub fn options(&self, threads: usize) -> Result<Options, String> {
        let mut options = Options {
            threads,
            ..Options::default()
        };
        if let Some(format) = self.format.as_deref().filter(|f| !f.is_empty()) {
            options.format = Format::parse(format).ok_or(format!("unknown format '{format}'"))?;
        }
        if let Some(delimiter) = self.delimiter.as_deref().filter(|d| !d.is_empty()) {
            let delimiter = if delimiter == "\\t" || delimiter == "tab" {
                "\t"
            } else {
                delimiter
            };
            match delimiter.as_bytes() {
                [b] if b.is_ascii() && *b != b'"' && *b != b'\r' && *b != b'\n' => {
                    options.delimiter = *b
                }
                _ => return Err("the delimiter must be a single character".into()),
            }
        }
        options.bom = self.bom.unwrap_or(false);
        options.decimals = match self.decimals.as_deref() {
            None | Some("") | Some("exact") => DecimalMode::Exact,
            Some("float") => DecimalMode::Float,
            Some(other) => return Err(format!("decimals must be exact or float, not '{other}'")),
        };
        options.on_error = match self.on_error.as_deref() {
            None | Some("") | Some("stop") => OnError::Stop,
            Some("skip") => OnError::Skip,
            Some(other) => return Err(format!("on_error must be stop or skip, not '{other}'")),
        };
        options.record_size = self.record_size.filter(|&s| s > 0);
        options.text_encoding = self.text_encoding.clone().filter(|e| !e.is_empty());
        if let Some(compression) = self.compression.as_deref().filter(|c| !c.is_empty()) {
            options.compression = Compression::parse(compression)
                .ok_or(format!("unknown compression '{compression}'"))?;
        }
        options.limit = self.limit.filter(|&l| l > 0);
        options.split = self.split.unwrap_or(false);
        options.input_kind = match self.input.as_deref() {
            None | Some("") | Some("auto") => InputKind::Auto,
            Some("bin") => InputKind::Bin,
            Some("txt") | Some("text") => InputKind::Text,
            Some(other) => return Err(format!("input must be auto, bin or txt, not '{other}'")),
        };
        options.name_hint = self.name.clone();
        Ok(options)
    }
}

/// The file name for a conversion's output: the table name, made safe for
/// a file system, with the format's extension.
pub fn output_name(table: &str, options: &Options) -> String {
    let stem: String = table
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = if stem.is_empty() {
        "export".to_owned()
    } else {
        stem
    };
    if options.split {
        format!("{stem}.{}.zip", options.format.extension())
    } else {
        format!("{stem}.{}", options.format.extension())
    }
}
