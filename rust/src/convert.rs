//! The conversion pipeline: input bytes in, encoded output out, nothing on
//! disk in between.
//!
//! ```text
//!  producer ──jobs──▶ worker × N ──chunks──▶ ordered writer ──▶ sink
//!  (reads the         (inflate, decode,       (shard order,
//!   input in order)    encode in parallel)     limits, stats)
//! ```
//!
//! The producer walks the archive (or loose file) front to back and hands
//! each shard, or each 8 MiB run of records, to a worker. Each job gets its
//! own small bounded channel to the writer, which drains jobs strictly in
//! input order. A worker that races ahead simply blocks on its own channel,
//! so memory stays bounded by a handful of shards however large the input
//! is, and a slow client slows the whole pipeline down instead of filling
//! memory. (Credits are per job rather than global so the job the writer is
//! waiting for can never be starved by later ones.)

use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::archive::{self, ShardFormat};
pub use crate::decode::DecimalMode;
use crate::decode::{Block, Decoder};
use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::text::{decode_text_shard, TextOptions};
pub use crate::writer::Compression;
use crate::writer::{
    arrow_schema, encode_text, to_record_batch, DirSink, Encoded, MergedSink, OutputFormat, Sink,
    TextKind, ZipSink,
};
use crate::zip::{Entries, IndexedReader, StreamReader};

/// Records decoded per block: large enough to amortise per-block work,
/// small enough to stay in cache.
const BLOCK_ROWS: usize = 16_384;
/// Bytes of raw records handed to a worker at a time for loose files.
const CHUNK_BYTES: usize = 8 << 20;

/// Output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Csv,
    Tsv,
    Jsonl,
    Parquet,
}

impl Format {
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "tsv" => Some(Self::Tsv),
            "jsonl" | "ndjson" | "json" => Some(Self::Jsonl),
            "parquet" => Some(Self::Parquet),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Jsonl => "jsonl",
            Self::Parquet => "parquet",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Csv => "text/csv; charset=utf-8",
            Self::Tsv => "text/tab-separated-values; charset=utf-8",
            Self::Jsonl => "application/x-ndjson",
            Self::Parquet => "application/vnd.apache.parquet",
        }
    }
}

/// What to do with a record that will not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnError {
    /// Fail the conversion with an error naming the record and field.
    #[default]
    Stop,
    /// Leave the failing values empty, report a count, and carry on.
    Skip,
}

/// How to read input that is not a zip archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputKind {
    /// Decide from the file name or the first bytes.
    #[default]
    Auto,
    Bin,
    Text,
}

/// Every knob a conversion has. The CLI flags, the HTTP query parameters
/// and the web page's controls all map onto this one struct.
#[derive(Debug, Clone)]
pub struct Options {
    pub format: Format,
    /// CSV delimiter (ignored for TSV, which always uses a tab).
    pub delimiter: u8,
    /// Prefix text output with a UTF-8 byte-order mark (for Excel).
    pub bom: bool,
    pub decimals: DecimalMode,
    pub on_error: OnError,
    /// Override the record size the schema implies.
    pub record_size: Option<usize>,
    /// Encoding of tab-separated `.TXT` shards.
    pub text_encoding: Option<String>,
    pub compression: Compression,
    /// Stop after this many records (per shard with `split`, as in Python).
    pub limit: Option<u64>,
    /// One output per shard.
    pub split: bool,
    /// Worker threads; 0 picks one per CPU.
    pub threads: usize,
    /// A schema to use instead of the archive's sidecar.
    pub schema: Option<Schema>,
    pub input_kind: InputKind,
    /// The input's file name, if known, for naming the output.
    pub name_hint: Option<String>,
    /// Refuse a single shard larger than this (compressed, as stored).
    pub max_shard_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            format: Format::Csv,
            delimiter: b',',
            bom: false,
            decimals: DecimalMode::Exact,
            on_error: OnError::Stop,
            record_size: None,
            text_encoding: None,
            compression: Compression::Zstd,
            limit: None,
            split: false,
            threads: 0,
            schema: None,
            input_kind: InputKind::Auto,
            name_hint: None,
            max_shard_bytes: 2 << 30,
        }
    }
}

impl Options {
    fn strict(&self) -> bool {
        self.on_error == OnError::Stop
    }

    fn text_kind(&self) -> Option<TextKind> {
        match self.format {
            Format::Csv => Some(TextKind::Csv {
                delimiter: self.delimiter,
            }),
            Format::Tsv => Some(TextKind::Csv { delimiter: b'\t' }),
            Format::Jsonl => Some(TextKind::Jsonl),
            Format::Parquet => None,
        }
    }

    fn worker_count(&self) -> usize {
        let wanted = if self.threads == 0 {
            std::thread::available_parallelism().map_or(1, |n| n.get())
        } else {
            self.threads
        };
        wanted.clamp(1, 64)
    }
}

/// Live counters for a running conversion, readable from another thread.
#[derive(Debug, Default)]
pub struct Progress {
    pub bytes_in: AtomicU64,
    pub records: AtomicU64,
    pub shards: AtomicU64,
    cancelled: AtomicBool,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the conversion to stop as soon as it can.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// What is known once the schema has been found, before any output.
#[derive(Debug, Clone)]
pub struct Meta {
    pub table: String,
    pub schema: Arc<Schema>,
    pub archive: bool,
}

/// The result of a completed conversion.
#[derive(Debug, Clone)]
pub struct Stats {
    pub table: String,
    pub records: u64,
    pub shards: u64,
    pub warnings: Vec<String>,
    pub elapsed: Duration,
}

/// Where the input comes from.
pub enum Input {
    /// Any byte stream: an upload, stdin. Read strictly front to back.
    Reader(Box<dyn Read + Send>),
    /// A local file: archives are read through their central directory, so
    /// shards come out in index order wherever the sidecar sits.
    File(PathBuf),
}

/// Where the output goes.
pub enum Output {
    /// One stream. With `split`, a zip of one file per shard.
    Writer(Box<dyn Write + Send>),
    /// A directory of one file per shard (requires `split`).
    Directory(PathBuf),
}

struct Counting<'a, R> {
    inner: R,
    counter: &'a AtomicU64,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.counter.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl<R: Seek> Seek for Counting<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Read until `buf` holds `want` bytes or the stream ends.
fn read_up_to(reader: &mut (impl Read + ?Sized), want: usize, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    buf.reserve(want);
    let mut chunk = reader.take(want as u64);
    chunk.read_to_end(buf)?;
    Ok(())
}

const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

enum Source<'a> {
    Archive(Box<dyn Entries + Send + 'a>),
    Loose {
        reader: Box<dyn Read + Send + 'a>,
        head: Vec<u8>,
        text_by_name: Option<bool>,
    },
}

fn open_source<'a>(input: Input, options: &Options, counter: &'a AtomicU64) -> Result<Source<'a>> {
    match input {
        Input::File(path) => {
            let mut file = File::open(&path)?;
            let mut magic = [0u8; 4];
            let n = file.read(&mut magic)?;
            file.seek(SeekFrom::Start(0))?;
            let counting = Counting {
                inner: file,
                counter,
            };
            if n == 4 && magic == ZIP_MAGIC {
                let mut reader = IndexedReader::new(counting)?;
                reader.sort_by_key(|e| archive::member_index(&e.name).unwrap_or(u32::MAX));
                return Ok(Source::Archive(Box::new(reader)));
            }
            let text = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("txt"));
            Ok(Source::Loose {
                reader: Box::new(counting),
                head: Vec::new(),
                text_by_name: text,
            })
        }
        Input::Reader(reader) => {
            let mut counting = Counting {
                inner: reader,
                counter,
            };
            let mut head = Vec::new();
            read_up_to(&mut counting, 512, &mut head)?;
            if head.starts_with(ZIP_MAGIC) {
                let chained = Cursor::new(head).chain(counting);
                return Ok(Source::Archive(Box::new(StreamReader::new(chained))));
            }
            let text_by_name = options.name_hint.as_deref().map(|n| n.to_ascii_lowercase().ends_with(".txt"));
            Ok(Source::Loose {
                reader: Box::new(counting),
                head,
                text_by_name,
            })
        }
    }
}

/// A data shard found while scanning, waiting to be handed to workers.
enum Pending {
    /// A nested `DATA.N.zip`, read whole (it is the compressed shard).
    Nested { name: Arc<str>, bytes: Vec<u8> },
    /// A text shard, read whole.
    Text { name: Arc<str>, bytes: Vec<u8> },
    /// A `.BIN` entry to stream from the archive reader itself.
    Bin { name: Arc<str> },
}

enum Classified {
    Sidecar(Vec<u8>),
    Data(Pending),
    Skip,
}

fn classify_entry(entries: &mut (dyn Entries + Send + '_), name: &str, limit: u64) -> Result<Classified> {
    if archive::is_zip_member(name) {
        let bytes = archive::read_bounded(entries, limit, name)?;
        let (inner, mut reader) = archive::open_inner(&bytes)?;
        let inner_base = inner.name.rsplit('/').next().unwrap_or(&inner.name).to_owned();
        let found = archive::classify(&inner.name).or_else(|| archive::classify(name));
        return Ok(match found {
            Some((0, _)) => Classified::Sidecar(archive::read_bounded(&mut reader, 64 << 20, "the schema sidecar")?),
            Some(_) => Classified::Data(Pending::Nested {
                name: inner_base.into(),
                bytes,
            }),
            None => Classified::Skip,
        });
    }
    let base: Arc<str> = name.rsplit('/').next().unwrap_or(name).into();
    Ok(match archive::classify(name) {
        Some((0, _)) => Classified::Sidecar(archive::read_bounded(entries, 64 << 20, "the schema sidecar")?),
        Some((_, ShardFormat::Bin)) => Classified::Data(Pending::Bin { name: base }),
        Some((_, ShardFormat::Text)) => Classified::Data(Pending::Text {
            name: base,
            bytes: archive::read_bounded(entries, limit, name)?,
        }),
        None => Classified::Skip,
    })
}

/// One unit of work for a worker.
struct Job {
    name: Arc<str>,
    payload: Payload,
    first_record: u64,
    /// Bytes after the last whole record (only on a shard's last chunk).
    trailing: usize,
    last: bool,
    out: Sender<Chunk>,
}

enum Payload {
    Records(Vec<u8>),
    Nested(Vec<u8>),
    Text(Vec<u8>),
}

enum Chunk {
    Data(Encoded),
    Failed(u64),
    Warning(String),
    Error(Error),
}

struct Ticket {
    name: Arc<str>,
    begins: bool,
    ends: bool,
}

/// Shared, read-only state for workers.
struct Context<'a> {
    decoder: Decoder,
    text: TextOptions,
    format: OutputFormat,
    limit: Option<u64>,
    progress: &'a Progress,
    abort: &'a AtomicBool,
}

impl Context<'_> {
    fn stopped(&self) -> bool {
        self.abort.load(Ordering::Relaxed) || self.progress.is_cancelled()
    }

    fn encode(&self, block: Block) -> Result<Chunk> {
        Ok(Chunk::Data(match self.format.text {
            Some(kind) => {
                let mut bytes = Vec::with_capacity(block.rows * 128);
                let mut ends = self.limit.map(|_| Vec::with_capacity(block.rows));
                encode_text(&block, kind, &mut bytes, ends.as_mut());
                Encoded::Text {
                    bytes,
                    rows: block.rows,
                    row_ends: ends,
                }
            }
            None => Encoded::Batch(to_record_batch(block, &self.format.arrow)?),
        }))
    }
}

/// Sends chunks for one job; a closed channel means the writer has stopped
/// (limit reached or failure elsewhere), which ends the job quietly.
struct JobOut<'a> {
    tx: &'a Sender<Chunk>,
}

impl JobOut<'_> {
    fn send(&self, chunk: Chunk) -> bool {
        self.tx.send(chunk).is_ok()
    }
}

fn run_worker(jobs: Receiver<Job>, ctx: &Context<'_>) {
    for job in jobs {
        let out = JobOut { tx: &job.out };
        if let Err(error) = process(&job, ctx, &out) {
            out.send(Chunk::Error(error));
        }
    }
}

/// Decode whole records, honouring the per-job cap. Returns the number of
/// records decoded, or `None` if the cap cut the job short.
fn decode_records(
    data: &[u8],
    first_record: u64,
    remaining: &mut Option<u64>,
    ctx: &Context<'_>,
    out: &JobOut<'_>,
) -> Result<Option<u64>> {
    let size = ctx.decoder.record_size();
    let total = data.len() / size;
    let mut row = 0;
    while row < total {
        if ctx.stopped() {
            return Err(Error::Cancelled);
        }
        let mut n = BLOCK_ROWS.min(total - row);
        if let Some(cap) = remaining {
            if *cap == 0 {
                return Ok(None);
            }
            n = n.min(*cap as usize);
        }
        let slice = &data[row * size..(row + n) * size];
        let first = first_record + row as u64;
        match ctx.decoder.decode(slice, first) {
            Ok(block) => {
                let failed = block.failed as u64;
                if failed > 0 && !out.send(Chunk::Failed(failed)) {
                    return Ok(None);
                }
                if !out.send(ctx.encode(block)?) {
                    return Ok(None);
                }
            }
            Err(failure) => {
                // Emit the good records before the bad one, as a row-at-a-time
                // reader would have, then fail.
                if failure.row > 0 {
                    let good = ctx
                        .decoder
                        .decode(&slice[..failure.row * size], first)
                        .map_err(|f| f.error)?;
                    out.send(ctx.encode(good)?);
                }
                return Err(failure.error);
            }
        }
        row += n;
        if let Some(cap) = remaining {
            *cap -= n as u64;
        }
    }
    Ok(Some(first_record + total as u64))
}

fn trailing_bytes(name: &str, trailing: usize, records: u64, ctx: &Context<'_>, out: &JobOut<'_>) -> Result<()> {
    if trailing == 0 {
        return Ok(());
    }
    let message = format!(
        "{name}: {trailing} trailing byte(s) after {records} records — not a whole multiple of record size {}",
        ctx.decoder.record_size()
    );
    if ctx.decoder.strict() {
        Err(Error::record(message, records, None))
    } else {
        out.send(Chunk::Warning(format!("{message}; ignored")));
        Ok(())
    }
}

fn process(job: &Job, ctx: &Context<'_>, out: &JobOut<'_>) -> Result<()> {
    let mut remaining = ctx.limit;
    match &job.payload {
        Payload::Records(data) => {
            let done = decode_records(data, job.first_record, &mut remaining, ctx, out)?;
            match done {
                Some(records) if job.last => trailing_bytes(&job.name, job.trailing, records, ctx, out),
                _ => Ok(()),
            }
        }
        Payload::Text(bytes) => decode_text(bytes, ctx, out),
        Payload::Nested(bytes) => {
            let (inner, mut reader) = archive::open_inner(bytes)?;
            if archive::classify(&inner.name).map(|(_, f)| f) == Some(ShardFormat::Text) {
                let bytes = archive::read_bounded(&mut reader, 8 << 30, &inner.name)?;
                return decode_text(&bytes, ctx, out);
            }
            let size = ctx.decoder.record_size();
            let want = size * BLOCK_ROWS * 4;
            let mut buf = Vec::with_capacity(want);
            let mut first = 0u64;
            loop {
                read_up_to(&mut reader, want, &mut buf)?;
                let whole = buf.len() - buf.len() % size;
                match decode_records(&buf[..whole], first, &mut remaining, ctx, out)? {
                    None => return Ok(()),
                    Some(next) => first = next,
                }
                if buf.len() < want {
                    return trailing_bytes(&job.name, buf.len() - whole, first, ctx, out);
                }
            }
        }
    }
}

fn decode_text(bytes: &[u8], ctx: &Context<'_>, out: &JobOut<'_>) -> Result<()> {
    let schema = ctx.decoder.schema().clone();
    let result = decode_text_shard(bytes, &schema, &ctx.text, BLOCK_ROWS, ctx.limit, |block| {
        if ctx.stopped() {
            return Err(Error::Cancelled);
        }
        if block.failed > 0 {
            out.send(Chunk::Failed(block.failed as u64));
        }
        if out.send(ctx.encode(block)?) {
            Ok(())
        } else {
            Err(Error::Cancelled)
        }
    });
    match result {
        Err(Error::Cancelled) if !ctx.stopped() => Ok(()),
        other => other,
    }
}

struct WriterOutcome {
    records: u64,
    shards: u64,
    failed: u64,
    warnings: Vec<String>,
}

fn run_writer(
    mut sink: Box<dyn Sink + Send + '_>,
    order: Receiver<(Ticket, Receiver<Chunk>)>,
    limit: Option<u64>,
    split: bool,
    progress: &Progress,
    abort: &AtomicBool,
) -> Result<WriterOutcome> {
    let mut outcome = WriterOutcome {
        records: 0,
        shards: 0,
        failed: 0,
        warnings: Vec::new(),
    };
    let mut shard_rows = 0u64;
    let mut open = false;
    let mut reached = false;
    let fail = |error: Error| {
        abort.store(true, Ordering::SeqCst);
        Err(error)
    };

    'jobs: for (ticket, chunks) in order.iter() {
        if ticket.begins {
            if let Err(e) = sink.begin_shard(&ticket.name) {
                return fail(e);
            }
            open = true;
            shard_rows = 0;
        }
        for chunk in chunks.iter() {
            match chunk {
                Chunk::Data(encoded) => {
                    let used = if split { shard_rows } else { outcome.records };
                    let allowed = limit.map_or(u64::MAX, |l| l.saturating_sub(used));
                    let encoded = if (encoded.rows() as u64) > allowed {
                        encoded.truncate(allowed as usize)
                    } else {
                        encoded
                    };
                    let rows = encoded.rows() as u64;
                    if rows > 0 {
                        if let Err(e) = sink.write(encoded) {
                            return fail(e);
                        }
                    }
                    outcome.records += rows;
                    shard_rows += rows;
                    progress.records.fetch_add(rows, Ordering::Relaxed);
                    if !split && limit.is_some_and(|l| outcome.records >= l) {
                        reached = true;
                        break 'jobs;
                    }
                }
                Chunk::Failed(n) => outcome.failed += n,
                Chunk::Warning(w) => outcome.warnings.push(w),
                Chunk::Error(e) => return fail(e),
            }
        }
        if ticket.ends {
            if let Err(e) = sink.end_shard() {
                return fail(e);
            }
            open = false;
            outcome.shards += 1;
            progress.shards.fetch_add(1, Ordering::Relaxed);
        }
    }

    if reached {
        // Stop the producer and workers; everything needed is written.
        abort.store(true, Ordering::SeqCst);
        if open {
            sink.end_shard()?;
            outcome.shards += 1;
        }
    } else if abort.load(Ordering::SeqCst) || progress.is_cancelled() {
        // The producer failed or the conversion was cancelled: leave the
        // output unfinished rather than make it look complete.
        return Err(Error::Cancelled);
    }
    sink.finish()?;
    Ok(outcome)
}

struct Producer<'a> {
    jobs: Sender<Job>,
    order: Sender<(Ticket, Receiver<Chunk>)>,
    record_size: usize,
    limit: Option<u64>,
    split: bool,
    submitted: u64,
    abort: &'a AtomicBool,
    progress: &'a Progress,
}

impl Producer<'_> {
    fn stopped(&self) -> bool {
        self.abort.load(Ordering::Relaxed) || self.progress.is_cancelled()
    }

    fn merged_limit_met(&self) -> bool {
        !self.split && self.limit.is_some_and(|l| self.submitted >= l)
    }

    /// Queue a job. False means stop producing.
    fn submit(&mut self, ticket: Ticket, payload: Payload, first_record: u64, trailing: usize) -> bool {
        if self.stopped() {
            return false;
        }
        let (tx, rx) = bounded(4);
        let job = Job {
            name: ticket.name.clone(),
            payload,
            first_record,
            trailing,
            last: ticket.ends,
            out: tx,
        };
        self.order.send((ticket, rx)).is_ok() && self.jobs.send(job).is_ok()
    }

    fn whole(&mut self, name: Arc<str>, payload: Payload) -> bool {
        let ticket = Ticket {
            name,
            begins: true,
            ends: true,
        };
        self.submit(ticket, payload, 0, 0)
    }

    /// Stream a `.BIN` shard in record-aligned chunks.
    fn records(&mut self, name: Arc<str>, reader: &mut (dyn Read + Send + '_), head: Vec<u8>) -> Result<bool> {
        let size = self.record_size;
        let chunk = size * (CHUNK_BYTES / size).max(1);
        let mut first = 0u64;
        let mut begins = true;
        let mut carry = head;
        loop {
            let mut buf = Vec::with_capacity(chunk);
            buf.append(&mut carry);
            let mut rest = Vec::new();
            read_up_to(reader, chunk - buf.len().min(chunk), &mut rest)?;
            buf.extend_from_slice(&rest);
            let at_end = buf.len() < chunk;
            let whole = buf.len() - buf.len() % size;
            let trailing = buf.len() - whole;
            buf.truncate(whole);
            let rows = (whole / size) as u64;

            let shard_capped = self.split && self.limit.is_some_and(|l| first + rows >= l);
            let merged_capped = !self.split && self.limit.is_some_and(|l| self.submitted + rows >= l);
            let ends = at_end || shard_capped || merged_capped;
            let ticket = Ticket {
                name: name.clone(),
                begins,
                ends,
            };
            if !self.submit(ticket, Payload::Records(buf), first, if at_end { trailing } else { 0 }) {
                return Ok(false);
            }
            first += rows;
            self.submitted += rows;
            begins = false;
            if ends {
                return Ok(!merged_capped);
            }
        }
    }
}

/// Convert `input` to `output`.
///
/// `on_ready` runs once the schema is known and the input has been checked,
/// before any output is written; the web service uses it to decide between
/// an error response and a streamed download.
pub fn convert(
    input: Input,
    output: Output,
    options: &Options,
    progress: &Progress,
    on_ready: impl FnOnce(&Meta),
) -> Result<Stats> {
    let started = Instant::now();
    let source = open_source(input, options, &progress.bytes_in)?;
    let mut schema = options.schema.clone().map(Arc::new);
    let mut table = archive::table_name(None, options.name_hint.as_deref());
    let is_archive = matches!(source, Source::Archive(_));

    // Phase 1: find the schema and the first data shard.
    let mut entries: Option<Box<dyn Entries + Send + '_>> = None;
    let mut loose: Option<(Box<dyn Read + Send + '_>, Vec<u8>, bool)> = None;
    let mut pending = None;
    match source {
        Source::Archive(mut reader) => {
            let mut first_member = None;
            while let Some(info) = reader.next_entry()? {
                if first_member.is_none() {
                    first_member = Some(info.name.clone());
                }
                if info.is_dir() {
                    continue;
                }
                match classify_entry(reader.as_mut(), &info.name, options.max_shard_bytes)? {
                    Classified::Skip => {}
                    Classified::Sidecar(bytes) => {
                        if schema.is_none() {
                            let name = archive::table_name(first_member.as_deref(), options.name_hint.as_deref());
                            schema = Some(Arc::new(Schema::parse_bytes(&bytes, Some(&name))?));
                        }
                    }
                    Classified::Data(found) => {
                        if schema.is_none() {
                            return Err(Error::Archive(
                                "the DATA.0 schema sidecar comes after the data in this archive, so it cannot \
                                 be read as a stream; convert the saved file with `sap-bin convert FILE`, \
                                 or supply the sidecar as the schema"
                                    .into(),
                            ));
                        }
                        pending = Some(found);
                        break;
                    }
                }
            }
            table = archive::table_name(first_member.as_deref(), options.name_hint.as_deref());
            if schema.is_none() {
                return Err(Error::Archive(
                    "the archive contains no DATA.0.TXT schema sidecar; supply a schema explicitly".into(),
                ));
            }
            if pending.is_none() {
                return Err(Error::Archive(
                    "the archive contains no DATA.N.BIN or DATA.N.TXT data shards".into(),
                ));
            }
            entries = Some(reader);
        }
        Source::Loose {
            reader,
            head,
            text_by_name,
        } => {
            let Some(schema) = schema.as_ref() else {
                return Err(Error::Schema(
                    "no schema available: pass --schema DATA.0.TXT, or point at an archive that \
                     carries its own sidecar"
                        .into(),
                ));
            };
            let text = match options.input_kind {
                InputKind::Bin => false,
                InputKind::Text => true,
                InputKind::Auto => text_by_name.unwrap_or_else(|| looks_like_text(&head, schema)),
            };
            loose = Some((reader, head, text));
        }
    }
    let schema = schema.expect("schema resolved above");

    let mode = options.decimals;
    let decoder = Decoder::new(schema.clone(), options.record_size, mode, options.strict())?;
    let text = TextOptions::new(options.text_encoding.as_deref(), mode, options.strict())?;
    let format = OutputFormat {
        text: options.text_kind(),
        bom: options.bom,
        compression: options.compression,
        arrow: arrow_schema(&schema, mode),
        extension: options.format.extension(),
    };
    let sink: Box<dyn Sink + Send> = match (output, options.split) {
        (Output::Writer(w), false) => Box::new(MergedSink::new(w, &schema, &format)?),
        (Output::Writer(w), true) => Box::new(ZipSink::new(w, &schema, &format)),
        (Output::Directory(dir), true) => Box::new(DirSink::new(dir, &schema, &format)?),
        (Output::Directory(_), false) => {
            return Err(Error::Schema("a directory output needs split mode".into()))
        }
    };

    on_ready(&Meta {
        table: table.clone(),
        schema: schema.clone(),
        archive: is_archive,
    });

    // Phase 2: run the pipeline.
    let abort = AtomicBool::new(false);
    let workers = options.worker_count();
    let ctx = Context {
        decoder,
        text,
        format,
        limit: options.limit,
        progress,
        abort: &abort,
    };

    let (produced, written) = std::thread::scope(|scope| {
        let (job_tx, job_rx) = bounded::<Job>(workers);
        let (order_tx, order_rx) = bounded(workers * 2 + 2);
        let (limit, split, abort_ref) = (options.limit, options.split, &abort);
        let writer = scope.spawn(move || run_writer(sink, order_rx, limit, split, progress, abort_ref));
        for _ in 0..workers {
            let jobs = job_rx.clone();
            let ctx = &ctx;
            scope.spawn(move || run_worker(jobs, ctx));
        }
        drop(job_rx);

        let mut producer = Producer {
            jobs: job_tx,
            order: order_tx,
            record_size: ctx.decoder.record_size(),
            limit: options.limit,
            split: options.split,
            submitted: 0,
            abort: &abort,
            progress,
        };
        let produced = produce(&mut producer, entries, loose, pending, options);
        if produced.is_err() {
            abort.store(true, Ordering::SeqCst);
        }
        drop(producer);
        let written = writer
            .join()
            .unwrap_or_else(|_| Err(Error::Io(io::Error::other("the output writer panicked"))));
        (produced, written)
    });

    let outcome = match (produced, written) {
        (_, Err(e)) if !matches!(e, Error::Cancelled) => return Err(e),
        (Err(e), _) => return Err(e),
        (Ok(()), Err(e)) => return Err(e),
        (Ok(()), Ok(outcome)) => outcome,
    };

    let mut warnings = outcome.warnings;
    if outcome.failed > 0 {
        warnings.insert(
            0,
            format!(
                "{} value(s) could not be decoded and were left empty",
                outcome.failed
            ),
        );
    }
    Ok(Stats {
        table,
        records: outcome.records,
        shards: outcome.shards,
        warnings,
        elapsed: started.elapsed(),
    })
}

fn produce(
    producer: &mut Producer<'_>,
    entries: Option<Box<dyn Entries + Send + '_>>,
    loose: Option<(Box<dyn Read + Send + '_>, Vec<u8>, bool)>,
    pending: Option<Pending>,
    options: &Options,
) -> Result<()> {
    if let Some((mut reader, head, text)) = loose {
        let name: Arc<str> = options
            .name_hint
            .as_deref()
            .map_or("DATA", |n| n.rsplit(['/', '\\']).next().unwrap_or(n))
            .into();
        if text {
            let mut bytes = head;
            archive::read_bounded(&mut reader, options.max_shard_bytes, &name).map(|rest| bytes.extend(rest))?;
            producer.whole(name, Payload::Text(bytes));
        } else {
            producer.records(name, reader.as_mut(), head)?;
        }
        return Ok(());
    }

    let mut entries = entries.expect("archive source");
    let mut next = pending;
    loop {
        let keep_going = match next.take() {
            Some(Pending::Nested { name, bytes }) => producer.whole(name, Payload::Nested(bytes)),
            Some(Pending::Text { name, bytes }) => producer.whole(name, Payload::Text(bytes)),
            Some(Pending::Bin { name }) => producer.records(name, entries.as_mut(), Vec::new())?,
            None => true,
        };
        if !keep_going || producer.merged_limit_met() || producer.stopped() {
            break;
        }
        // Find the next data shard.
        loop {
            let Some(info) = entries.next_entry()? else {
                return Ok(());
            };
            if info.is_dir() {
                continue;
            }
            if let Classified::Data(found) = classify_entry(entries.as_mut(), &info.name, options.max_shard_bytes)? {
                next = Some(found);
                break;
            }
        }
    }
    Ok(())
}

/// A loose file is tab-separated text when it starts with the schema's
/// first column name and a tab.
fn looks_like_text(head: &[u8], schema: &Schema) -> bool {
    let head = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    let first = schema.fields()[0].name.as_bytes();
    head.starts_with(first) && head.get(first.len()) == Some(&b'\t')
}

/// Convert a whole input held in memory. A convenience for tests and small
/// jobs.
pub fn convert_bytes(input: Vec<u8>, options: &Options) -> Result<(Vec<u8>, Stats)> {
    let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let stats = convert(
        Input::Reader(Box::new(Cursor::new(input))),
        Output::Writer(Box::new(Shared(buffer.clone()))),
        options,
        &Progress::new(),
        |_| {},
    )?;
    let bytes = std::mem::take(&mut *buffer.lock().unwrap());
    Ok((bytes, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{build_archive, bsis_rows, encode_bsis_records, encode_file, sample_archive, BSIS_SIDECAR};

    fn bsis() -> Schema {
        Schema::parse(BSIS_SIDECAR, Some("BSIS")).unwrap()
    }

    fn reference_archive() -> Vec<u8> {
        let payload = encode_file(&bsis(), &bsis_rows());
        build_archive(BSIS_SIDECAR, "BSIS", "BIN", &[payload.clone(), payload])
    }

    #[test]
    fn converts_an_archive_to_csv() {
        let (out, stats) = convert_bytes(reference_archive(), &Options::default()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(stats.records, 6);
        assert_eq!(stats.shards, 2);
        assert_eq!(stats.table, "BSIS");
        assert_eq!(text.lines().count(), 7);
        assert!(text.contains("1000000003,002,,KR,-1234.56\r\n"));
    }

    #[test]
    fn a_limit_caps_total_rows() {
        let options = Options {
            limit: Some(4),
            ..Options::default()
        };
        let (out, stats) = convert_bytes(reference_archive(), &options).unwrap();
        assert_eq!(stats.records, 4);
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 5);
    }

    #[test]
    fn parallel_output_keeps_input_order() {
        let archive = sample_archive(40_000, 6);
        for threads in [1, 4] {
            let options = Options {
                threads,
                ..Options::default()
            };
            let (out, stats) = convert_bytes(archive.clone(), &options).unwrap();
            assert_eq!(stats.records, 240_000);
            let text = String::from_utf8(out).unwrap();
            let belnr: Vec<&str> = text.lines().skip(1).map(|l| l.split(',').nth(4).unwrap()).collect();
            // Each shard restarts at 1000000000 and increments.
            assert_eq!(belnr[0], "1000000000");
            assert_eq!(belnr[39_999], "1000039999");
            assert_eq!(belnr[40_000], "1000000000");
        }
    }

    #[test]
    fn loose_bin_needs_and_uses_a_schema() {
        let mut data = Vec::new();
        encode_bsis_records(20_000, 3, &mut data);
        let err = convert_bytes(data.clone(), &Options::default()).unwrap_err();
        assert!(err.to_string().contains("no schema available"));
        let options = Options {
            schema: Some(bsis()),
            ..Options::default()
        };
        let (_, stats) = convert_bytes(data, &options).unwrap();
        assert_eq!(stats.records, 20_000);
    }

    #[test]
    fn a_wrong_record_size_fails_loudly_or_nulls_when_lenient() {
        let options = Options {
            record_size: Some(128),
            ..Options::default()
        };
        let err = convert_bytes(reference_archive(), &options).unwrap_err();
        assert!(matches!(err, Error::Record { .. }), "{err}");

        let lenient = Options {
            record_size: Some(128),
            on_error: OnError::Skip,
            ..Options::default()
        };
        let (_, stats) = convert_bytes(reference_archive(), &lenient).unwrap();
        assert!(!stats.warnings.is_empty());
    }

    #[test]
    fn trailing_bytes_are_an_error_unless_lenient() {
        let mut data = encode_file(&bsis(), &bsis_rows());
        data.extend_from_slice(&[0, 1, 2]);
        let options = Options {
            schema: Some(bsis()),
            ..Options::default()
        };
        let err = convert_bytes(data.clone(), &options).unwrap_err();
        assert!(err.to_string().contains("3 trailing byte(s) after 3 records"), "{err}");
        let lenient = Options {
            on_error: OnError::Skip,
            ..options
        };
        let (_, stats) = convert_bytes(data, &lenient).unwrap();
        assert_eq!(stats.records, 3);
    }

    #[test]
    fn split_output_is_a_zip_of_shards() {
        let options = Options {
            split: true,
            format: Format::Parquet,
            ..Options::default()
        };
        let (out, stats) = convert_bytes(reference_archive(), &options).unwrap();
        assert_eq!(stats.shards, 2);
        let mut reader = StreamReader::new(Cursor::new(out));
        let mut names = Vec::new();
        while let Some(info) = reader.next_entry().unwrap() {
            let mut body = Vec::new();
            reader.read_to_end(&mut body).unwrap();
            assert!(body.starts_with(b"PAR1") && body.ends_with(b"PAR1"));
            names.push(info.name);
        }
        assert_eq!(names, vec!["DATA.1.parquet", "DATA.2.parquet"]);
    }

    #[test]
    fn text_shards_convert_too() {
        let shard = "BUKRS\tHKONT\tZUONR\tGJAHR\tBELNR\tBUZEI\tBUDAT\tBLART\tDMBTR\r\n\
            0100\t0000123456\t20250601\t2025\t1000000005\t001\t20250601\tPR\t47.12 \r\n";
        let archive = build_archive(BSIS_SIDECAR, "BSIS", "TXT", &[shard.as_bytes().to_vec()]);
        let (out, stats) = convert_bytes(archive, &Options::default()).unwrap();
        assert_eq!(stats.records, 1);
        assert!(String::from_utf8(out).unwrap().ends_with("2025-06-01,PR,47.12\r\n"));
    }
}
