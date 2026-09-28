//! The sap-bin engine in the browser.
//!
//! The page runs this in a Web Worker, so the conversion never leaves the
//! user's machine: files are read with `FileReaderSync`, a few megabytes at a
//! time, and the output goes to a JavaScript function in chunks, which the
//! worker writes to the browser's private file system. It is the same engine
//! as the command line and the web service, taking the same parameters
//! ([`ConvertParams`], the query of the HTTP API), in one thread.
//!
//! Results and errors cross into JavaScript as JSON text: the same report
//! and the same `{"error", "hint"}` shape the web service answers with, so the
//! page renders both alike.

use std::io::{self, Read, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use sap_bin::archive::schema_from_bytes;
use sap_bin::convert::{convert as run, Input, InputKind, Output, Progress};
use sap_bin::inspect::{inspect as describe, Sample};
use sap_bin::params::{output_name, ConvertParams};
use sap_bin::zip::{Entries, EntryInfo};
use sap_bin::Error;
use serde_json::json;
use wasm_bindgen::prelude::*;
use web_sys::{Blob, File, FileReaderSync};

/// Bytes read from a file at a time.
const READ_BLOCK: f64 = 4.0 * 1024.0 * 1024.0;
/// Bytes handed to JavaScript at a time.
const WRITE_BLOCK: usize = 1024 * 1024;

/// The engine's version, for the page to show.
#[wasm_bindgen]
pub fn version() -> String {
    sap_bin::VERSION.to_owned()
}

/// Convert an export.
///
/// `files` is one `File` (the delivered zip, or a single `.BIN`) or several
/// (the members of an unzipped export, in shard order). `schema` is a
/// `DATA.0.TXT` to use instead of the export's own. `params` is JSON with the
/// HTTP API's parameter names. `write(bytes)` receives the output in order,
/// and `progress(records, bytes_read)` is called as the conversion goes.
///
/// Returns JSON: `table`, `records`, `shards`, `warnings`, `seconds` and
/// `file_name`, the name to save the output under. Fails with JSON
/// `{"error", "hint"}`.
#[wasm_bindgen]
pub fn convert(
    files: Vec<File>,
    schema: Option<Vec<u8>>,
    params: &str,
    write: js_sys::Function,
    progress: js_sys::Function,
) -> Result<String, String> {
    let params: ConvertParams =
        serde_json::from_str(params).map_err(|e| problem(&format!("bad parameters: {e}"), None))?;
    let mut options = params.options(1).map_err(|e| problem(&e, None))?;
    options.sequential = true;
    if let Some(bytes) = schema.filter(|s| !s.is_empty()) {
        options.schema = Some(schema_from_bytes(&bytes, None).map_err(|e| failure(&e))?);
    }
    if files.is_empty() {
        return Err(problem("no files to convert", None));
    }

    let counters = Arc::new(Progress::new());
    let report = Reporter {
        callback: progress,
        counters: counters.clone(),
    };
    let multi = files.len() > 1 || params.multi.unwrap_or(false);
    let input = if multi {
        // Name the export (folder or table) from `name`, not from a file.
        options.input_kind = InputKind::Auto;
        Input::Files(Box::new(FileEntries::new(files, report.clone())))
    } else {
        let file = files.into_iter().next().expect("one file");
        if options.name_hint.is_none() {
            options.name_hint = Some(file.name());
        }
        Input::Reader(Box::new(BlobReader::new(file.into(), None)))
    };
    let output = Output::Writer(Box::new(JsWriter {
        callback: write,
        buffer: Vec::with_capacity(WRITE_BLOCK),
        report: report.clone(),
    }));

    let stats = run(input, output, &options, &counters, |_| {}).map_err(|e| failure(&e))?;
    report.send();
    Ok(json!({
        "table": stats.table,
        "records": stats.records,
        "shards": stats.shards,
        "warnings": stats.warnings,
        "seconds": stats.elapsed.as_secs_f64(),
        "file_name": output_name(&stats.table, &options),
    })
    .to_string())
}

/// Describe an export from its first bytes (`head`) and, for a zip, its last
/// (`tail`), exactly as the web service's `api/inspect` does. Returns the
/// report as JSON; fails with JSON `{"error", "hint"}`.
#[wasm_bindgen]
pub fn inspect(
    head: &[u8],
    tail: Option<Vec<u8>>,
    size: f64,
    name: Option<String>,
    schema: Option<Vec<u8>>,
    record_size: Option<u32>,
    text_encoding: Option<String>,
) -> Result<String, String> {
    let schema = match schema.filter(|s| !s.is_empty()) {
        Some(bytes) => Some(schema_from_bytes(&bytes, None).map_err(|e| failure(&e))?),
        None => None,
    };
    let report = describe(Sample {
        head,
        tail: tail.as_deref(),
        total_size: (size > 0.0).then_some(size as u64),
        schema,
        record_size: record_size.map(|s| s as usize).filter(|&s| s > 0),
        text_encoding: text_encoding.filter(|e| !e.is_empty()),
        name_hint: name.filter(|n| !n.is_empty()),
        preview_rows: 20,
    })
    .map_err(|e| failure(&e))?;
    serde_json::to_string(&report).map_err(|e| problem(&e.to_string(), None))
}

/// A synthetic export to try things with, as `api/sample` makes.
#[wasm_bindgen]
pub fn sample(records: u32, shards: u32) -> Vec<u8> {
    sap_bin::sample::sample_archive(
        (records as usize).clamp(1, 200_000),
        (shards as usize).clamp(1, 8),
    )
}

fn problem(message: &str, hint: Option<&str>) -> String {
    json!({ "error": message, "hint": hint }).to_string()
}

fn failure(error: &Error) -> String {
    problem(&error.to_string(), error.hint())
}

/// Calls the page's progress function with the live counters.
#[derive(Clone)]
struct Reporter {
    callback: js_sys::Function,
    counters: Arc<Progress>,
}

impl Reporter {
    fn send(&self) {
        let records = self.counters.records.load(Ordering::Relaxed) as f64;
        let bytes = self.counters.bytes_in.load(Ordering::Relaxed) as f64;
        let _ = self
            .callback
            .call2(&JsValue::NULL, &records.into(), &bytes.into());
    }

    fn read(&self, bytes: usize) {
        self.counters
            .bytes_in
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.send();
    }
}

/// A `File` or `Blob` read front to back, a block at a time. Synchronous
/// reads are allowed in a worker, which is where this runs.
struct BlobReader {
    blob: Blob,
    reader: Option<FileReaderSync>,
    position: f64,
    buffer: Vec<u8>,
    at: usize,
    /// Counts the bytes read, for a file that is one member of several (the
    /// engine counts a single file's bytes itself).
    report: Option<Reporter>,
}

impl BlobReader {
    fn new(blob: Blob, report: Option<Reporter>) -> Self {
        Self {
            blob,
            reader: None,
            position: 0.0,
            buffer: Vec::new(),
            at: 0,
            report,
        }
    }

    fn fill(&mut self) -> io::Result<()> {
        let size = self.blob.size();
        if self.position >= size {
            self.buffer.clear();
            self.at = 0;
            return Ok(());
        }
        let end = (self.position + READ_BLOCK).min(size);
        let reader = match &self.reader {
            Some(reader) => reader,
            None => self.reader.insert(FileReaderSync::new().map_err(js_error)?),
        };
        let part = self
            .blob
            .slice_with_f64_and_f64(self.position, end)
            .map_err(js_error)?;
        let bytes = reader.read_as_array_buffer(&part).map_err(js_error)?;
        self.buffer = js_sys::Uint8Array::new(&bytes).to_vec();
        self.at = 0;
        self.position = end;
        if let Some(report) = &self.report {
            report.read(self.buffer.len());
        }
        Ok(())
    }
}

impl Read for BlobReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at >= self.buffer.len() {
            self.fill()?;
        }
        let n = out.len().min(self.buffer.len() - self.at);
        out[..n].copy_from_slice(&self.buffer[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

fn js_error(value: JsValue) -> io::Error {
    let text = value
        .as_string()
        .or_else(|| {
            value
                .dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .unwrap_or_else(|| "the file could not be read".to_owned());
    io::Error::other(text)
}

/// Several files as the members of one export, like the web service's
/// multi-file upload.
struct FileEntries {
    files: std::vec::IntoIter<File>,
    current: Option<BlobReader>,
    report: Reporter,
}

impl FileEntries {
    fn new(files: Vec<File>, report: Reporter) -> Self {
        Self {
            files: files.into_iter(),
            current: None,
            report,
        }
    }
}

impl Entries for FileEntries {
    fn next_entry(&mut self) -> io::Result<Option<EntryInfo>> {
        let Some(file) = self.files.next() else {
            self.current = None;
            return Ok(None);
        };
        let name = file.name();
        self.current = Some(BlobReader::new(file.into(), Some(self.report.clone())));
        Ok(Some(EntryInfo {
            name,
            method: 0,
            size: None,
            compressed_size: None,
        }))
    }
}

impl Read for FileEntries {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match &mut self.current {
            Some(reader) => reader.read(out),
            None => Ok(0),
        }
    }
}

/// Hands the output to the page, a megabyte at a time.
struct JsWriter {
    callback: js_sys::Function,
    buffer: Vec<u8>,
    report: Reporter,
}

impl JsWriter {
    fn send(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = js_sys::Uint8Array::from(self.buffer.as_slice());
        self.buffer.clear();
        self.callback
            .call1(&JsValue::NULL, &chunk)
            .map_err(js_error)?;
        self.report.send();
        Ok(())
    }
}

impl Write for JsWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= WRITE_BLOCK {
            self.send()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

impl Drop for JsWriter {
    fn drop(&mut self) {
        let _ = self.send();
    }
}

// The engine's inputs and outputs must be `Send`, as they cross threads on
// other platforms. JavaScript objects are not, but this module runs on one
// thread only: it is built without the atomics a threaded WebAssembly
// module needs, so nothing here can ever be reached from another thread.
unsafe impl Send for BlobReader {}
unsafe impl Send for FileEntries {}
unsafe impl Send for JsWriter {}
