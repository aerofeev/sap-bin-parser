//! `sap-bin`: the command line, the local app and the web service in one
//! binary.
//!
//! Run with no arguments (or double-click it) to open the local app in your
//! browser. Every command mirrors the Python CLI's names and flags.

use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};

#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use sap_bin::convert::{convert, Input, InputKind, OnError, Options, Output, Progress};
use sap_bin::files::FileEntries;
use sap_bin::inspect::{inspect, Sample};
use sap_bin::zip::{Entries, IndexedReader};
use sap_bin::{archive, server, Compression, DecimalMode, Error, Format, Schema};

#[derive(Parser)]
#[command(
    name = "sap-bin",
    version,
    about = "Convert SAP binary table exports (.BIN) to CSV, Parquet or JSON Lines.",
    long_about = "Convert SAP binary table exports (.BIN) to CSV, Parquet or JSON Lines.\n\n\
                  Run with no arguments to open the app in your browser.",
    after_help = "Examples:\n  \
        sap-bin                                  open the app in your browser\n  \
        sap-bin info BSIS.QUERY.zip --fields     what is in this export?\n  \
        sap-bin head BSIS.QUERY.zip -n 3         the first three records\n  \
        sap-bin convert BSIS.QUERY.zip -o bsis.parquet\n  \
        sap-bin convert BSIS.QUERY/ -o bsis.csv      an unzipped export folder\n  \
        sap-bin convert DATA.1.BIN DATA.2.BIN --schema DATA.0.TXT -o bsis.csv\n  \
        cat BSIS.QUERY.zip | sap-bin convert - -o - > bsis.csv"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the app in your browser, running on this computer.
    App(AppArgs),
    /// Run the web service.
    Serve(ServeArgs),
    /// Describe an export: schema, record geometry, shards.
    Info(InfoArgs),
    /// Print the first few decoded records.
    Head(HeadArgs),
    /// Rank candidate record sizes for data that will not line up.
    Probe(ProbeArgs),
    /// Convert an export to CSV, TSV, JSON Lines or Parquet.
    Convert(ConvertArgs),
    /// Measure throughput on synthetic data.
    Bench(BenchArgs),
}

#[derive(Args, Clone)]
struct Common {
    /// A delivered .zip, a .BIN or data .TXT file, or - for stdin.
    path: String,
    /// Schema sidecar (DATA.0.TXT); defaults to the one inside the archive.
    #[arg(long)]
    schema: Option<PathBuf>,
}

/// One export: a .zip, a .BIN or .TXT file, an unzipped export folder,
/// or several shard files; - reads stdin.
#[derive(Args, Clone)]
struct Inputs {
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    paths: Vec<String>,
    /// Schema sidecar (DATA.0.TXT, or DATA.0.zip); overrides the one in the export.
    #[arg(long)]
    schema: Option<PathBuf>,
}

impl Inputs {
    fn name(&self) -> String {
        self.paths[0].clone()
    }
}

#[derive(Args)]
struct AppArgs {
    /// Port to listen on (default: a free one).
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Do not open the browser.
    #[arg(long)]
    no_open: bool,
}

#[derive(Args)]
struct ServeArgs {
    /// Address to bind. Use 0.0.0.0 to accept connections from other machines.
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    #[arg(long, default_value_t = 8080, env = "PORT")]
    port: u16,
    /// Largest upload accepted, in megabytes (0: no limit).
    #[arg(long, default_value_t = 20_000, env = "SAPBIN_MAX_UPLOAD_MB")]
    max_upload_mb: u64,
    /// Conversions allowed at once; further requests get 503.
    #[arg(long, default_value_t = 4, env = "SAPBIN_MAX_CONCURRENCY")]
    max_concurrency: usize,
    /// Worker threads per conversion (0: CPUs divided by concurrency).
    #[arg(long, default_value_t = 0, env = "SAPBIN_THREADS")]
    threads: usize,
}

#[derive(Args)]
struct InfoArgs {
    #[command(flatten)]
    common: Common,
    /// List every field with its offset.
    #[arg(long)]
    fields: bool,
}

#[derive(Args)]
struct HeadArgs {
    #[command(flatten)]
    inputs: Inputs,
    /// Records to print.
    #[arg(short = 'n', long = "count", default_value_t = 5)]
    count: u64,
    /// Override the schema's record size.
    #[arg(long)]
    record_size: Option<usize>,
    /// Encoding of .TXT shards.
    #[arg(long, default_value = "windows-1251")]
    text_encoding: String,
}

#[derive(Args)]
struct ProbeArgs {
    #[command(flatten)]
    common: Common,
    /// Records to test per candidate.
    #[arg(long, default_value_t = 200)]
    sample: usize,
}

#[derive(Clone, Copy, ValueEnum)]
enum FormatArg {
    Csv,
    Tsv,
    Jsonl,
    Parquet,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnErrorArg {
    Stop,
    Skip,
}

#[derive(Args)]
struct ConvertArgs {
    #[command(flatten)]
    inputs: Inputs,
    /// Output file (- for stdout), or a directory with --split.
    #[arg(short, long)]
    output: String,
    #[arg(short, long, value_enum, default_value = "csv")]
    format: FormatArg,
    /// One output file per shard.
    #[arg(long)]
    split: bool,
    /// Stop after this many records (per shard with --split).
    #[arg(long)]
    limit: Option<u64>,
    /// Override the schema's record size.
    #[arg(long)]
    record_size: Option<usize>,
    /// CSV delimiter.
    #[arg(long, default_value = ",")]
    delimiter: String,
    /// CSV encoding: utf-8, or utf-8-sig to add the BOM Excel likes.
    #[arg(long, default_value = "utf-8")]
    encoding: String,
    /// Parquet codec: zstd, snappy, gzip or none.
    #[arg(long, default_value = "zstd")]
    compression: String,
    /// Encoding of .TXT shards.
    #[arg(long, default_value = "windows-1251")]
    text_encoding: String,
    /// Emit packed decimals as float64 rather than exact decimals.
    #[arg(long)]
    float_decimals: bool,
    /// Stop at the first bad record, or leave bad values empty and continue.
    #[arg(long, value_enum, default_value = "stop")]
    on_error: OnErrorArg,
    /// Worker threads (0: one per CPU).
    #[arg(long, default_value_t = 0)]
    threads: usize,
    /// Do not show progress.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct BenchArgs {
    /// Records to synthesise.
    #[arg(long, default_value_t = 1_000_000)]
    records: usize,
    /// Worker threads (0: one per CPU).
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(error) => report_error(&error),
    };
    std::process::exit(code);
}

fn report_error(error: &Error) -> i32 {
    match error {
        Error::Io(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("error: no such file: {e}");
            2
        }
        Error::Io(e) if e.kind() == io::ErrorKind::BrokenPipe => 0,
        other => {
            eprintln!("error: {other}");
            if let Some(hint) = other.hint() {
                eprintln!("{hint}");
            }
            other.exit_code()
        }
    }
}

fn run(cli: Cli) -> sap_bin::Result<i32> {
    match cli.command {
        None => run_app(AppArgs {
            port: 0,
            no_open: false,
        }),
        Some(Command::App(args)) => run_app(args),
        Some(Command::Serve(args)) => run_serve(args),
        Some(Command::Info(args)) => cmd_info(args),
        Some(Command::Head(args)) => cmd_head(args),
        Some(Command::Probe(args)) => cmd_probe(args),
        Some(Command::Convert(args)) => cmd_convert(args),
        Some(Command::Bench(args)) => cmd_bench(args),
    }
}

fn runtime() -> sap_bin::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn init_logging(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("SAPBIN_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(io::stderr().is_terminal())
        .with_writer(io::stderr)
        .try_init();
}

fn run_app(args: AppArgs) -> sap_bin::Result<i32> {
    init_logging("warn");
    let config = server::Config {
        addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.port),
        local: true,
        max_upload: 0,
        max_concurrency: 64,
        threads: 0,
        open_browser: !args.no_open,
    };
    runtime()?.block_on(server::serve(config))?;
    Ok(0)
}

fn run_serve(args: ServeArgs) -> sap_bin::Result<i32> {
    init_logging("info");
    let config = server::Config {
        addr: SocketAddr::new(args.host, args.port),
        local: false,
        max_upload: args.max_upload_mb.saturating_mul(1_000_000),
        max_concurrency: args.max_concurrency.max(1),
        threads: args.threads,
        open_browser: false,
    };
    runtime()?.block_on(server::serve(config))?;
    Ok(0)
}

fn load_schema(path: &Option<PathBuf>) -> sap_bin::Result<Option<Schema>> {
    match path {
        None => Ok(None),
        Some(path) => {
            let bytes = std::fs::read(path)?;
            let name = path.file_stem().and_then(|s| s.to_str());
            Ok(Some(archive::schema_from_bytes(&bytes, name)?))
        }
    }
}

fn open_input(paths: &[String]) -> sap_bin::Result<Input> {
    if paths.len() == 1 && paths[0] == "-" {
        return Ok(Input::Reader(Box::new(io::stdin())));
    }
    let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    if let Some(missing) = paths.iter().find(|p| !p.exists()) {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            missing.display().to_string(),
        )));
    }
    if paths.len() == 1 && paths[0].is_file() {
        return Ok(Input::File(paths[0].clone()));
    }
    Ok(Input::Files(Box::new(FileEntries::new(&paths)?)))
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn is_zip(path: &Path) -> io::Result<bool> {
    let mut magic = [0u8; 4];
    let n = File::open(path)?.read(&mut magic)?;
    Ok(n == 4 && &magic == b"PK\x03\x04")
}

/// Read the whole of a small file, or the first `limit` bytes of a large one.
fn read_head(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    File::open(path)?.take(limit).read_to_end(&mut out)?;
    Ok(out)
}

fn cmd_info(args: InfoArgs) -> sap_bin::Result<i32> {
    let path = PathBuf::from(&args.common.path);
    if !path.exists() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            path.display().to_string(),
        )));
    }
    let explicit = load_schema(&args.common.schema)?;
    let size = std::fs::metadata(&path)?.len();

    if is_zip(&path)? {
        let described = describe_archive(&path, explicit)?;
        let schema = described.schema.ok_or_else(|| {
            Error::Archive(
                "the archive contains no DATA.0.TXT schema sidecar; supply a schema explicitly"
                    .into(),
            )
        })?;
        print_schema(&schema, args.fields);
        println!();
        println!(
            "archive: {} — {} data shard(s)",
            described.table, described.shards
        );
        if let Some(format) = described.format {
            println!("format:  {}", format.describe());
        }
        println!("stored:  {:.1} MB", described.stored as f64 / 1e6);
        if let Some(records) = described.records {
            println!("records: {}", group(records));
        }
        return Ok(0);
    }

    let Some(schema) = explicit else {
        return Err(Error::Schema(
            "no schema available: pass --schema DATA.0.TXT, or point at an archive that carries its own sidecar"
                .into(),
        ));
    };
    print_schema(&schema, args.fields);
    let record = schema.record_size() as u64;
    println!();
    println!(
        "file:    {}",
        path.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    );
    println!("size:    {} bytes", group(size));
    println!("records: {}", group(size / record));
    if size % record != 0 {
        eprintln!(
            "warning: {} trailing byte(s) — file is not a whole multiple of {record}. Try `sap-bin probe`.",
            size % record
        );
    }
    Ok(0)
}

fn print_schema(schema: &Schema, fields: bool) {
    println!(
        "schema: {} — {} fields",
        schema.name.as_deref().unwrap_or("(unnamed)"),
        schema.len()
    );
    println!("payload: {} bytes", schema.payload_size());
    if schema.padding_size() > 0 {
        println!(
            "record:  {} bytes  ({} pad byte to even alignment)",
            schema.record_size(),
            schema.padding_size()
        );
    } else {
        println!("record:  {} bytes", schema.record_size());
    }
    for problem in schema.inconsistencies() {
        eprintln!("warning: {problem}");
    }
    if fields {
        println!();
        println!(
            "{:>7}  {:<24} {:<5} {:>4} {:>4} {:>6}",
            "OFFSET", "NAME", "TYPE", "LEN", "DEC", "BYTES"
        );
        for (field, offset) in schema.offsets() {
            println!(
                "{:>7}  {:<24} {:<5} {:>4} {:>4} {:>6}",
                offset,
                field.name,
                field.kind.as_str(),
                field.length,
                field.decimals,
                field.size
            );
        }
    }
}

struct ArchiveSummary {
    table: String,
    schema: Option<Schema>,
    shards: usize,
    stored: u64,
    format: Option<archive::ShardFormat>,
    records: Option<u64>,
}

/// Walk an archive's central directory, reading only each shard's inner
/// header, to count shards and (when the headers state sizes) records.
fn describe_archive(path: &Path, explicit: Option<Schema>) -> sap_bin::Result<ArchiveSummary> {
    let mut reader = IndexedReader::new(File::open(path)?)?;
    reader.sort_by_key(|e| archive::member_index(&e.name).unwrap_or(u32::MAX));
    let file_name = path.file_name().and_then(|n| n.to_str());
    let table = archive::table_name(reader.entries().first().map(|e| e.name.as_str()), file_name);
    let mut summary = ArchiveSummary {
        table: table.clone(),
        schema: explicit,
        shards: 0,
        stored: 0,
        format: None,
        records: Some(0),
    };
    let mut sizes = Vec::new();
    while let Some(info) = reader.next_entry()? {
        if info.is_dir() {
            continue;
        }
        let stored = info.compressed_size.unwrap_or(0);
        let (name, size) = if archive::is_zip_member(&info.name) {
            let head = archive::read_bounded(&mut reader, u64::MAX, &info.name)?;
            match archive::open_inner(&head) {
                Ok((inner, mut data)) => {
                    let is_sidecar = archive::classify(&inner.name).is_some_and(|(i, _)| i == 0);
                    if is_sidecar && summary.schema.is_none() {
                        let bytes = archive::read_bounded(&mut data, 64 << 20, "sidecar")?;
                        summary.schema = Some(Schema::parse_bytes(&bytes, Some(&table))?);
                    }
                    (inner.name, inner.size)
                }
                Err(_) => continue,
            }
        } else {
            let classified = archive::classify(&info.name);
            if classified.is_some_and(|(i, _)| i == 0) && summary.schema.is_none() {
                let bytes = archive::read_bounded(&mut reader, 64 << 20, "sidecar")?;
                summary.schema = Some(Schema::parse_bytes(&bytes, Some(&table))?);
            }
            (info.name.clone(), info.size)
        };
        match archive::classify(&name) {
            Some((0, _)) | None => {}
            Some((_, format)) => {
                summary.shards += 1;
                summary.stored += stored;
                if summary.format.is_some_and(|f| f != format) {
                    return Err(Error::Archive(
                        "the archive mixes .BIN and .TXT shards".into(),
                    ));
                }
                summary.format = Some(format);
                sizes.push((format, size));
            }
        }
    }
    let record = summary.schema.as_ref().map(|s| s.record_size() as u64);
    summary.records = match record {
        Some(record)
            if sizes
                .iter()
                .all(|(f, s)| *f == archive::ShardFormat::Bin && s.is_some()) =>
        {
            Some(sizes.iter().map(|(_, s)| s.unwrap() / record).sum())
        }
        _ => None,
    };
    Ok(summary)
}

fn cmd_probe(args: ProbeArgs) -> sap_bin::Result<i32> {
    let path = PathBuf::from(&args.common.path);
    if !path.exists() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            path.display().to_string(),
        )));
    }
    let explicit = load_schema(&args.common.schema)?;
    let size = std::fs::metadata(&path)?.len();
    let report = if is_zip(&path)? {
        let head = read_head(&path, 16 << 20)?;
        inspect(Sample {
            head: &head,
            schema: explicit,
            ..Sample::default()
        })?
    } else {
        let schema = explicit
            .ok_or_else(|| Error::Schema("no schema available: pass --schema DATA.0.TXT".into()))?;
        let head = read_head(
            &path,
            (schema.payload_size() as u64 + 8) * args.sample as u64,
        )?;
        let candidates =
            sap_bin::probe::probe(&head, &Arc::new(schema.clone()), Some(size), args.sample);
        print_probe(&candidates, schema.record_size());
        return Ok(0);
    };
    let Some(schema) = report.schema.as_ref() else {
        return Err(Error::Archive("no schema: pass --schema DATA.0.TXT".into()));
    };
    let candidates = if report.probe.is_empty() {
        // The schema's size decodes; still show the ranking for completeness.
        match &report.preview {
            Some(_) => {
                println!(
                    "The schema's record size ({} bytes) decodes cleanly.",
                    schema.record_size
                );
                return Ok(0);
            }
            None => Vec::new(),
        }
    } else {
        report.probe.clone()
    };
    print_probe(&candidates, schema.record_size);
    Ok(0)
}

fn print_probe(candidates: &[sap_bin::probe::Candidate], schema_size: usize) {
    println!(
        "{:>6}  {:>14}  {:>15}",
        "SIZE", "CLEAN RECORDS", "DIVIDES EVENLY"
    );
    for c in candidates {
        let divides = c.divides_evenly.map_or("unknown".to_owned(), |d| {
            if d {
                "True".into()
            } else {
                "False".into()
            }
        });
        let marker = if c.record_size == schema_size {
            "  <- schema"
        } else {
            ""
        };
        println!(
            "{:>6}  {:>14}  {:>15}{marker}",
            c.record_size, c.clean_records, divides
        );
    }
}

fn cmd_head(args: HeadArgs) -> sap_bin::Result<i32> {
    let options = Options {
        format: Format::Jsonl,
        limit: Some(args.count),
        on_error: OnError::Skip,
        record_size: args.record_size,
        text_encoding: Some(args.text_encoding.clone()),
        schema: load_schema(&args.inputs.schema)?,
        threads: 1,
        name_hint: Some(args.inputs.name()),
        ..Options::default()
    };
    let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = SharedBuffer(buffer.clone());
    let mut names = Vec::new();
    convert(
        open_input(&args.inputs.paths)?,
        Output::Writer(Box::new(sink)),
        &options,
        &Progress::new(),
        |meta| names = meta.schema.field_names().map(str::to_owned).collect(),
    )?;
    let text = String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned();
    let width = names.iter().map(String::len).max().unwrap_or(0);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (index, line) in text.lines().enumerate() {
        let row: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).unwrap_or_default();
        writeln!(out, "--- record {index} ---")?;
        for name in &names {
            let value = match row.get(name) {
                Some(serde_json::Value::String(s)) => format!("{s:?}"),
                Some(serde_json::Value::Null) | None => "empty".to_owned(),
                Some(other) => other.to_string(),
            };
            writeln!(out, "  {name:<width$}  {value}")?;
        }
    }
    Ok(0)
}

struct SharedBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn convert_options(args: &ConvertArgs) -> sap_bin::Result<Options> {
    let delimiter = match args.delimiter.as_str() {
        "\\t" | "tab" => b'\t',
        d if d.len() == 1 && d.is_ascii() => d.as_bytes()[0],
        _ => {
            return Err(Error::Schema(
                "--delimiter must be a single character".into(),
            ))
        }
    };
    let bom = match args
        .encoding
        .to_ascii_lowercase()
        .replace('_', "-")
        .as_str()
    {
        "utf-8" | "utf8" => false,
        "utf-8-sig" | "utf8-sig" => true,
        other => {
            return Err(Error::Schema(format!(
                "--encoding {other} is not supported; use utf-8, or utf-8-sig for Excel"
            )))
        }
    };
    Ok(Options {
        format: match args.format {
            FormatArg::Csv => Format::Csv,
            FormatArg::Tsv => Format::Tsv,
            FormatArg::Jsonl => Format::Jsonl,
            FormatArg::Parquet => Format::Parquet,
        },
        delimiter,
        bom,
        decimals: if args.float_decimals {
            DecimalMode::Float
        } else {
            DecimalMode::Exact
        },
        on_error: match args.on_error {
            OnErrorArg::Stop => OnError::Stop,
            OnErrorArg::Skip => OnError::Skip,
        },
        record_size: args.record_size,
        text_encoding: Some(args.text_encoding.clone()),
        compression: Compression::parse(&args.compression)
            .ok_or_else(|| Error::Schema(format!("unknown compression '{}'", args.compression)))?,
        limit: args.limit,
        split: args.split,
        threads: args.threads,
        schema: load_schema(&args.inputs.schema)?,
        input_kind: InputKind::Auto,
        name_hint: Some(args.inputs.name()),
        ..Options::default()
    })
}

fn cmd_convert(args: ConvertArgs) -> sap_bin::Result<i32> {
    let options = convert_options(&args)?;
    let input = open_input(&args.inputs.paths)?;
    let to_stdout = args.output == "-";
    let output = if args.split && !to_stdout {
        Output::Directory(PathBuf::from(&args.output))
    } else if to_stdout {
        Output::Writer(Box::new(BufWriter::with_capacity(1 << 20, io::stdout())))
    } else {
        let path = PathBuf::from(&args.output);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Output::Writer(Box::new(BufWriter::with_capacity(
            1 << 20,
            File::create(&path)?,
        )))
    };

    let progress = Arc::new(Progress::new());
    let show = !args.quiet && io::stderr().is_terminal();
    let ticker = show.then(|| spawn_ticker(progress.clone()));
    let result = convert(input, output, &options, &progress, |_| {});
    if let Some((stop, handle)) = ticker {
        stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }
    let stats = result?;
    for warning in &stats.warnings {
        eprintln!("warning: {warning}");
    }
    let rate = stats.records as f64 / stats.elapsed.as_secs_f64().max(1e-9);
    let summary = format!(
        "{} records from {} shard(s) in {:.2}s ({} records/s)",
        group(stats.records),
        stats.shards,
        stats.elapsed.as_secs_f64(),
        group(rate as u64)
    );
    if to_stdout {
        eprintln!("{summary}");
    } else {
        eprintln!("{summary} -> {}", args.output);
    }
    Ok(0)
}

fn spawn_ticker(
    progress: Arc<Progress>,
) -> (
    Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        let started = Instant::now();
        let mut shown = false;
        while !flag.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
            let records = progress.records.load(Ordering::Relaxed);
            let read = progress.bytes_in.load(Ordering::Relaxed);
            let secs = started.elapsed().as_secs_f64();
            if secs > 0.5 {
                eprint!(
                    "\r  {} records · {:.0} MB read · {} records/s   ",
                    group(records),
                    read as f64 / 1e6,
                    group((records as f64 / secs) as u64)
                );
                shown = true;
            }
        }
        if shown {
            eprint!("\r{:60}\r", "");
        }
    });
    (stop, handle)
}

/// Discards output, counting it.
struct Null(u64);

impl Write for Null {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn cmd_bench(args: BenchArgs) -> sap_bin::Result<i32> {
    let schema = Schema::parse(sap_bin::sample::BSIS_SIDECAR, Some("BSIS"))?;
    eprintln!("synthesising {} BSIS records…", group(args.records as u64));
    let mut loose = Vec::new();
    sap_bin::sample::encode_bsis_records(args.records, 1, &mut loose);
    let shards = 8;
    let per = args.records / shards;
    let archive = {
        let payloads: Vec<Vec<u8>> = (0..shards)
            .map(|i| loose[i * per * 126..(i + 1) * per * 126].to_vec())
            .collect();
        sap_bin::sample::build_archive(sap_bin::sample::BSIS_SIDECAR, "BSIS", "BIN", &payloads)
    };
    let threads = if args.threads == 0 {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    } else {
        args.threads
    };
    println!(
        "{} records of {} bytes ({:.0} MB raw, {:.0} MB as a deflated archive), {} thread(s)\n",
        group(args.records as u64),
        schema.record_size(),
        loose.len() as f64 / 1e6,
        archive.len() as f64 / 1e6,
        threads
    );
    println!(
        "{:<34} {:>14} {:>10} {:>10}",
        "", "records/s", "MB/s in", "seconds"
    );
    let cases: [(&str, bool, Format, usize); 7] = [
        ("loose .BIN -> CSV, 1 thread", false, Format::Csv, 1),
        ("loose .BIN -> CSV", false, Format::Csv, threads),
        ("loose .BIN -> JSON Lines", false, Format::Jsonl, threads),
        (
            "loose .BIN -> Parquet (zstd)",
            false,
            Format::Parquet,
            threads,
        ),
        ("zipped archive -> CSV, 1 thread", true, Format::Csv, 1),
        ("zipped archive -> CSV", true, Format::Csv, threads),
        (
            "zipped archive -> Parquet (zstd)",
            true,
            Format::Parquet,
            threads,
        ),
    ];
    for (label, zipped, format, threads) in cases {
        let options = Options {
            format,
            threads,
            schema: (!zipped).then(|| schema.clone()),
            ..Options::default()
        };
        let input = if zipped {
            archive.clone()
        } else {
            loose.clone()
        };
        let size = input.len();
        let started = Instant::now();
        let stats = convert(
            Input::Reader(Box::new(io::Cursor::new(input))),
            Output::Writer(Box::new(Null(0))),
            &options,
            &Progress::new(),
            |_| {},
        )?;
        let secs = started.elapsed().as_secs_f64();
        println!(
            "{:<34} {:>14} {:>10.0} {:>10.2}",
            label,
            group((stats.records as f64 / secs) as u64),
            size as f64 / 1e6 / secs,
            secs
        );
    }
    Ok(0)
}
