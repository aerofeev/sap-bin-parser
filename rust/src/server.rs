//! The web service and the local app: the same page, the same API.
//!
//! Nothing is stored. An upload is read as a stream by the conversion
//! pipeline and the result streams straight back as a download; there is no
//! temporary file, no database, and no request body in any log. The only
//! per-request state is a set of counters (bytes read, records written) kept
//! in memory under a random id the browser chose, so the page can show
//! progress, and dropped a few minutes after the conversion ends.
//!
//! Endpoints:
//!
//! | | |
//! |---|---|
//! | `GET /` | the page |
//! | `POST /api/inspect` | describe an export from its first and last bytes |
//! | `POST /api/convert` | convert: raw body (curl) or multipart (the page) |
//! | `GET /api/jobs/{id}` | progress of a running conversion |
//! | `POST /api/jobs/{id}/cancel` | stop it |
//! | `GET /api/sample` | a synthetic export to try things with |
//! | `GET /api/config`, `GET /healthz` | version and limits |

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_stream::wrappers::ReceiverStream;

use crate::archive::schema_from_bytes;
use crate::convert::{
    convert, Format, Input, InputKind, Meta, OnError, Options, Output, Progress, Stats,
};
use crate::decode::DecimalMode;
use crate::error::Error;
use crate::inspect::{inspect, Sample};
use crate::writer::Compression;
use crate::zip::{Entries, EntryInfo};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const FAVICON: &str = include_str!("../web/favicon.svg");

/// The page's Content-Security-Policy: nothing from anywhere but here, and
/// no connection to anywhere but here.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; form-action 'self'; frame-src 'self'; frame-ancestors 'self'; \
                   base-uri 'none'";

/// How the service runs.
#[derive(Debug, Clone)]
pub struct Config {
    pub addr: SocketAddr,
    /// Running on the user's own machine: no upload cap, a friendlier
    /// banner, and requests accepted only for localhost host names.
    pub local: bool,
    /// Largest upload accepted, in bytes (0 for no limit).
    pub max_upload: u64,
    /// Conversions allowed at once; more get 503 with Retry-After.
    pub max_concurrency: usize,
    /// Worker threads per conversion (0: CPUs / concurrency, at least 1).
    pub threads: usize,
    /// Open the page in the default browser once listening.
    pub open_browser: bool,
}

impl Config {
    fn threads_per_job(&self) -> usize {
        if self.threads > 0 {
            return self.threads;
        }
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        if self.local {
            cpus
        } else {
            (cpus / self.max_concurrency.max(1)).max(1)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
enum JobState {
    Running,
    Done { warnings: Vec<String>, seconds: f64 },
    Failed { error: String, hint: Option<String> },
}

struct Job {
    progress: Arc<Progress>,
    started: Instant,
    state: Mutex<JobState>,
    finished: Mutex<Option<Instant>>,
}

impl Job {
    fn finish(&self, state: JobState) {
        *self.state.lock().unwrap() = state;
        *self.finished.lock().unwrap() = Some(Instant::now());
    }
}

struct AppState {
    config: Config,
    permits: Arc<Semaphore>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

type Shared = Arc<AppState>;

/// How long a finished job's counters stay readable.
const JOB_TTL: Duration = Duration::from_secs(300);
const MAX_JOBS: usize = 10_000;

impl AppState {
    fn register(&self, id: &str) -> Option<Arc<Job>> {
        if !(8..=64).contains(&id.len())
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|_, job| {
            job.finished
                .lock()
                .unwrap()
                .is_none_or(|t| t.elapsed() < JOB_TTL)
        });
        if jobs.len() >= MAX_JOBS || jobs.contains_key(id) {
            return None;
        }
        let job = Arc::new(Job {
            progress: Arc::new(Progress::new()),
            started: Instant::now(),
            state: Mutex::new(JobState::Running),
            finished: Mutex::new(None),
        });
        jobs.insert(id.to_owned(), job.clone());
        Some(job)
    }
}

/// Build the router.
pub fn app(config: Config) -> Router {
    let state = Arc::new(AppState {
        permits: Arc::new(Semaphore::new(config.max_concurrency.max(1))),
        jobs: Mutex::new(HashMap::new()),
        config,
    });
    Router::new()
        .route("/", get(index))
        .route(
            "/assets/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/assets/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/favicon.svg",
            get(|| async { asset("image/svg+xml", FAVICON) }),
        )
        .route("/healthz", get(health))
        .route("/api/config", get(config_info))
        .route("/api/sample", get(sample))
        .route("/api/inspect", post(inspect_upload))
        .route("/api/convert", post(convert_upload))
        .route("/api/jobs/{id}", get(job_status))
        .route("/api/jobs/{id}/cancel", post(job_cancel))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Serve until Ctrl+C.
pub async fn serve(config: Config) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    let addr = listener.local_addr()?;
    let host = if addr.ip().is_unspecified() {
        "localhost".to_owned()
    } else if addr.ip().is_loopback() {
        "127.0.0.1".to_owned()
    } else {
        addr.ip().to_string()
    };
    let url = format!("http://{host}:{}/", addr.port());
    let config = Config { addr, ..config };
    if config.local {
        eprintln!(
            "sap-bin {} is running on this computer at {url}",
            crate::VERSION
        );
        eprintln!(
            "Your files never leave this machine. Press Ctrl+C (or close this window) to stop."
        );
    } else {
        eprintln!("sap-bin {} listening on {url}", crate::VERSION);
    }
    if config.open_browser && webbrowser::open(&url).is_err() {
        eprintln!("Open {url} in your browser.");
    }
    axum::serve(
        listener,
        app(config).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "version": crate::VERSION }))
}

async fn config_info(State(state): State<Shared>) -> Json<serde_json::Value> {
    Json(json!({
        "version": crate::VERSION,
        "local": state.config.local,
        "max_upload_bytes": state.config.max_upload,
        "threads": state.config.threads_per_job(),
    }))
}

#[derive(Deserialize)]
struct SampleParams {
    records: Option<usize>,
    shards: Option<usize>,
}

async fn sample(Query(params): Query<SampleParams>) -> Response {
    let records = params.records.unwrap_or(5_000).clamp(1, 200_000);
    let shards = params.shards.unwrap_or(2).clamp(1, 8);
    let bytes = tokio::task::spawn_blocking(move || crate::sample::sample_archive(records, shards))
        .await
        .unwrap_or_default();
    (
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"BSIS.QUERY.sample.zip\"",
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Security headers on everything; localhost-only host names in local mode;
/// cross-site POSTs refused; an access log without queries or bodies.
async fn guard(State(state): State<Shared>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();

    if let Some(problem) = check_request(&state.config, request.method(), request.headers()) {
        return finish_log(problem, &method, &path, started);
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("content-security-policy", HeaderValue::from_static(CSP));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), interest-cohort=()"),
    );
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    if path.starts_with("/api/") {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    finish_log(response, &method, &path, started)
}

fn finish_log(response: Response, method: &Method, path: &str, started: Instant) -> Response {
    // Method, path, status and timing only: never a query, a file name or a body.
    tracing::info!(
        target: "access",
        "{method} {path} {} {}ms",
        response.status().as_u16(),
        started.elapsed().as_millis()
    );
    response
}

fn check_request(config: &Config, method: &Method, headers: &HeaderMap) -> Option<Response> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if config.local {
        // Defeats DNS rebinding: only localhost names may reach the local app.
        let name = host.rsplit_once(':').map_or(host, |(n, _)| n);
        if !matches!(name, "127.0.0.1" | "localhost" | "[::1]") {
            return Some(problem(
                StatusCode::FORBIDDEN,
                "This local app only answers on localhost.",
                None,
            ));
        }
    }
    if method == Method::POST {
        let cross_site = headers
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "cross-site");
        let foreign_origin = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|origin| {
                origin != "null" && origin.split_once("://").map(|(_, rest)| rest) != Some(host)
            });
        if cross_site || foreign_origin {
            return Some(problem(
                StatusCode::FORBIDDEN,
                "Cross-site requests are not accepted.",
                None,
            ));
        }
    }
    None
}

fn problem(status: StatusCode, message: &str, hint: Option<&str>) -> Response {
    (status, Json(json!({ "error": message, "hint": hint }))).into_response()
}

fn error_response(error: &Error) -> Response {
    let status = match error {
        Error::Limit(_) => StatusCode::PAYLOAD_TOO_LARGE,
        Error::Record { .. } | Error::Schema(_) | Error::Archive(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        Error::Cancelled => StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
        Error::Io(e)
            if e.kind() == io::ErrorKind::InvalidData
                || e.kind() == io::ErrorKind::UnexpectedEof =>
        {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    problem(status, &error.to_string(), error.hint())
}

/// Query parameters of `/api/convert`, mirroring the CLI flags.
#[derive(Debug, Default, Deserialize)]
struct ConvertParams {
    format: Option<String>,
    delimiter: Option<String>,
    bom: Option<bool>,
    decimals: Option<String>,
    on_error: Option<String>,
    record_size: Option<usize>,
    text_encoding: Option<String>,
    compression: Option<String>,
    limit: Option<u64>,
    split: Option<bool>,
    input: Option<String>,
    name: Option<String>,
    job: Option<String>,
    /// Each uploaded `file` part is one member of the export (shards, and
    /// optionally the sidecar), rather than the whole export.
    multi: Option<bool>,
}

impl ConvertParams {
    fn options(&self, threads: usize) -> Result<Options, String> {
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

/// What the upload pump sends to the pipeline.
enum Piece {
    /// A new file part begins (multi-file uploads only).
    Begin(String),
    Chunk(Bytes),
}

type PieceResult = io::Result<Piece>;

/// Blocking reader over upload chunks, for the synchronous pipeline. With
/// `multi`, it presents each file part as one entry of an export.
struct ChannelReader {
    rx: mpsc::Receiver<PieceResult>,
    current: Bytes,
    /// A part that has begun but not yet been returned by `next_entry`.
    pending: Option<String>,
    in_entry: bool,
    /// Multi-file uploads bypass the pipeline's own byte counter.
    progress: Option<Arc<Progress>>,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<PieceResult>, progress: Arc<Progress>, multi: bool) -> Self {
        Self {
            rx,
            current: Bytes::new(),
            pending: None,
            in_entry: !multi,
            progress: multi.then_some(progress),
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.in_entry {
            return Ok(0);
        }
        while self.current.is_empty() {
            match self.rx.blocking_recv() {
                None => {
                    self.in_entry = false;
                    return Ok(0);
                }
                Some(Err(e)) => return Err(e),
                Some(Ok(Piece::Chunk(chunk))) => self.current = chunk,
                Some(Ok(Piece::Begin(name))) => {
                    self.pending = Some(name);
                    self.in_entry = false;
                    return Ok(0);
                }
            }
        }
        let n = buf.len().min(self.current.len());
        buf[..n].copy_from_slice(&self.current[..n]);
        self.current = self.current.slice(n..);
        if let Some(progress) = &self.progress {
            progress
                .bytes_in
                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(n)
    }
}

impl Entries for ChannelReader {
    fn next_entry(&mut self) -> io::Result<Option<EntryInfo>> {
        let mut scratch = [0u8; 64 * 1024];
        while self.in_entry {
            if self.read(&mut scratch)? == 0 {
                break;
            }
        }
        self.current = Bytes::new();
        let name = match self.pending.take() {
            Some(name) => name,
            None => loop {
                match self.rx.blocking_recv() {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(Piece::Begin(name))) => break name,
                    Some(Ok(Piece::Chunk(_))) => {}
                }
            },
        };
        self.in_entry = true;
        Ok(Some(EntryInfo {
            name,
            method: 0,
            size: None,
            compressed_size: None,
        }))
    }
}

/// Blocking writer into the response body, in 256 KiB chunks.
struct ChannelWriter {
    tx: mpsc::Sender<io::Result<Bytes>>,
    buffer: Vec<u8>,
}

const OUT_CHUNK: usize = 256 * 1024;

impl ChannelWriter {
    fn send(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buffer, Vec::with_capacity(OUT_CHUNK));
        self.tx
            .blocking_send(Ok(Bytes::from(chunk)))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the client went away"))
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        if self.buffer.len() >= OUT_CHUNK {
            self.send()?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

/// What the upload carried besides the data itself.
struct UploadMeta {
    schema: Option<Vec<u8>>,
    file_name: Option<String>,
}

/// Pump the request body into `tx`: the raw body, or the `file` part of a
/// multipart form (after an optional `schema` part). Enforces the upload
/// limit, then keeps reading to the end so the client sees a clean response
/// even when the pipeline finished early.
async fn feed(
    body: Body,
    boundary: Option<String>,
    limit: u64,
    multi: bool,
    tx: mpsc::Sender<PieceResult>,
    meta_tx: oneshot::Sender<Result<UploadMeta, String>>,
) {
    let mut pump = Pump {
        tx,
        limit,
        sent: 0,
        open: true,
    };
    let Some(boundary) = boundary else {
        let _ = meta_tx.send(Ok(UploadMeta {
            schema: None,
            file_name: None,
        }));
        let mut stream = body.into_data_stream();
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(chunk) => pump.push(chunk).await,
                Err(e) => return pump.fail(e.to_string()).await,
            }
        }
        return;
    };

    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut schema = None;
    let mut meta_tx = Some(meta_tx);
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                match meta_tx.take() {
                    Some(m) => {
                        let _ = m.send(Err(format!("the upload could not be read: {e}")));
                    }
                    None => pump.fail(e.to_string()).await,
                }
                return;
            }
        };
        match field.name() {
            Some("schema") => {
                if let Ok(bytes) = field.bytes().await {
                    if !bytes.is_empty() && bytes.len() <= 4 << 20 {
                        schema = Some(bytes.to_vec());
                    }
                }
            }
            Some("file") => {
                let file_name = field.file_name().map(str::to_owned);
                if let Some(m) = meta_tx.take() {
                    let _ = m.send(Ok(UploadMeta {
                        schema: schema.take(),
                        file_name: file_name.clone(),
                    }));
                }
                if multi {
                    pump.begin(file_name.unwrap_or_else(|| "DATA".into())).await;
                }
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => pump.push(chunk).await,
                        Ok(None) => break,
                        Err(e) => return pump.fail(e.to_string()).await,
                    }
                }
                if !multi {
                    // One export per request: anything further is ignored.
                    while let Ok(Some(rest)) = multipart.next_field().await {
                        let _ = rest.bytes().await;
                    }
                    return;
                }
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    if let Some(m) = meta_tx.take() {
        let _ = m.send(Err("the upload has no 'file' part".into()));
    }
}

/// Forwards upload chunks to the pipeline, enforcing the size limit. Once
/// the pipeline stops reading (it has all it needs, or failed), the rest of
/// the upload is read and discarded, so the browser still sees a clean
/// response rather than a reset connection.
struct Pump {
    tx: mpsc::Sender<PieceResult>,
    limit: u64,
    sent: u64,
    open: bool,
}

impl Pump {
    async fn push(&mut self, chunk: Bytes) {
        if !self.open {
            return;
        }
        self.sent += chunk.len() as u64;
        if self.limit > 0 && self.sent > self.limit {
            let _ = self.tx.send(Err(limit_error(self.limit))).await;
            self.open = false;
        } else if self.tx.send(Ok(Piece::Chunk(chunk))).await.is_err() {
            self.open = false;
        }
    }

    async fn begin(&mut self, name: String) {
        if self.open && self.tx.send(Ok(Piece::Begin(name))).await.is_err() {
            self.open = false;
        }
    }

    async fn fail(&mut self, message: String) {
        if self.open {
            let _ = self.tx.send(Err(io::Error::other(message))).await;
            self.open = false;
        }
    }
}

fn limit_error(limit: u64) -> io::Error {
    io::Error::other(Error::Limit(format!(
        "the upload is larger than this server's {} MB limit; run sap-bin on your own machine for larger files",
        limit / 1_000_000
    )))
}

fn boundary_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| multer::parse_boundary(v).ok())
}

fn attachment_name(meta: &Meta, options: &Options) -> String {
    let stem: String = meta
        .table
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

async fn convert_upload(
    State(state): State<Shared>,
    Query(params): Query<ConvertParams>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let job = params.job.as_deref().and_then(|id| state.register(id));
    let Ok(permit) = state.permits.clone().try_acquire_owned() else {
        let message = "The server is busy with other conversions. Try again in a minute, or run sap-bin on your own computer.";
        if let Some(job) = &job {
            job.finish(JobState::Failed {
                error: message.into(),
                hint: None,
            });
        }
        let mut response = problem(StatusCode::SERVICE_UNAVAILABLE, message, None);
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
        return response;
    };
    let mut options = match params.options(state.config.threads_per_job()) {
        Ok(options) => options,
        Err(message) => {
            if let Some(job) = &job {
                job.finish(JobState::Failed {
                    error: message.clone(),
                    hint: None,
                });
            }
            return problem(StatusCode::BAD_REQUEST, &message, None);
        }
    };
    let progress = job
        .as_ref()
        .map_or_else(|| Arc::new(Progress::new()), |j| j.progress.clone());
    let fail = |job: &Option<Arc<Job>>, error: &Error| {
        if let Some(job) = job {
            job.finish(JobState::Failed {
                error: error.to_string(),
                hint: error.hint().map(str::to_owned),
            });
        }
        error_response(error)
    };

    let limit = if state.config.local {
        0
    } else {
        state.config.max_upload
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if limit > 0 && declared.is_some_and(|d| d > limit + 64 * 1024) {
        let error = Error::Limit(limit_error(limit).to_string());
        return fail(&job, &error);
    }
    let (in_tx, in_rx) = mpsc::channel(16);
    let (meta_tx, meta_rx) = oneshot::channel();
    let multi = params.multi.unwrap_or(false);
    let boundary = boundary_of(&headers);
    if multi && boundary.is_none() {
        return fail(
            &job,
            &Error::Archive(
                "multi=true needs a multipart upload with one 'file' part per file".into(),
            ),
        );
    }
    tokio::spawn(feed(body, boundary, limit, multi, in_tx, meta_tx));

    let upload = match meta_rx.await {
        Ok(Ok(meta)) => meta,
        Ok(Err(message)) => return fail(&job, &Error::Archive(message)),
        Err(_) => {
            return fail(
                &job,
                &Error::Archive("the upload ended before any data".into()),
            )
        }
    };
    if let Some(bytes) = upload.schema {
        match schema_from_bytes(&bytes, None) {
            Ok(schema) => options.schema = Some(schema),
            Err(e) => return fail(&job, &e),
        }
    }
    if options.name_hint.is_none() {
        options.name_hint = upload.file_name;
    }
    if multi {
        // The page names the export (folder or table) in `name`; keep a
        // file part's name from being mistaken for the table.
        options.input_kind = InputKind::Auto;
    }

    let (out_tx, out_rx) = mpsc::channel::<io::Result<Bytes>>(8);
    let (ready_tx, ready_rx) = oneshot::channel::<Meta>();
    let worker_options = options.clone();
    let worker_progress = progress.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let reader = ChannelReader::new(in_rx, worker_progress.clone(), multi);
        let input = if multi {
            Input::Files(Box::new(reader))
        } else {
            Input::Reader(Box::new(reader))
        };
        let writer = ChannelWriter {
            tx: out_tx.clone(),
            buffer: Vec::with_capacity(OUT_CHUNK),
        };
        let result = convert(
            input,
            Output::Writer(Box::new(writer)),
            &worker_options,
            &worker_progress,
            |meta| {
                let _ = ready_tx.send(meta.clone());
            },
        );
        if let Err(error) = &result {
            // Abort the response so a failed conversion never looks complete.
            let _ = out_tx.blocking_send(Err(io::Error::other(error.to_string())));
        }
        drop(permit);
        result
    });

    let meta = match ready_rx.await {
        Ok(meta) => meta,
        Err(_) => {
            let error = match handle.await {
                Ok(Err(error)) => error,
                Ok(Ok(_)) => Error::Archive("the conversion ended without output".into()),
                Err(_) => Error::Io(io::Error::other("the conversion crashed")),
            };
            return fail(&job, &unwrap_limit(error));
        }
    };

    let filename = attachment_name(&meta, &options);
    let content_type = if options.split {
        "application/zip"
    } else {
        options.format.content_type()
    };
    let job_for_task = job.clone();
    tokio::spawn(async move {
        let outcome = handle.await;
        if let Some(job) = job_for_task {
            job.finish(match outcome {
                Ok(Ok(stats)) => done_state(&stats),
                Ok(Err(error)) => {
                    let error = unwrap_limit(error);
                    JobState::Failed {
                        error: error.to_string(),
                        hint: error.hint().map(str::to_owned),
                    }
                }
                Err(_) => JobState::Failed {
                    error: "the conversion crashed".into(),
                    hint: None,
                },
            });
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header("x-sap-bin-table", meta.table.as_str())
        .body(Body::from_stream(ReceiverStream::new(out_rx)))
        .unwrap_or_else(|_| {
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not start the download",
                None,
            )
        })
}

/// An upload-limit error travels through the pipeline as an I/O error;
/// recover it so it maps to 413.
fn unwrap_limit(error: Error) -> Error {
    match error {
        Error::Io(io) if io.get_ref().is_some_and(|e| e.is::<Error>()) => {
            match io.into_inner().and_then(|e| e.downcast::<Error>().ok()) {
                Some(inner) => *inner,
                None => Error::Io(io::Error::other("upload failed")),
            }
        }
        other => other,
    }
}

fn done_state(stats: &Stats) -> JobState {
    JobState::Done {
        warnings: stats.warnings.clone(),
        seconds: stats.elapsed.as_secs_f64(),
    }
}

#[derive(Serialize)]
struct JobStatus {
    #[serde(flatten)]
    state: JobState,
    bytes_in: u64,
    records: u64,
    shards: u64,
    elapsed: f64,
}

async fn job_status(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    let job = state.jobs.lock().unwrap().get(&id).cloned();
    match job {
        None => problem(StatusCode::NOT_FOUND, "no such job", None),
        Some(job) => {
            let progress = &job.progress;
            let finished = *job.finished.lock().unwrap();
            let status = JobStatus {
                state: job.state.lock().unwrap().clone(),
                bytes_in: progress.bytes_in.load(std::sync::atomic::Ordering::Relaxed),
                records: progress.records.load(std::sync::atomic::Ordering::Relaxed),
                shards: progress.shards.load(std::sync::atomic::Ordering::Relaxed),
                elapsed: finished
                    .map_or_else(|| job.started.elapsed(), |f| f - job.started)
                    .as_secs_f64(),
            };
            Json(status).into_response()
        }
    }
}

async fn job_cancel(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    match state.jobs.lock().unwrap().get(&id) {
        Some(job) => {
            job.progress.cancel();
            StatusCode::NO_CONTENT.into_response()
        }
        None => problem(StatusCode::NOT_FOUND, "no such job", None),
    }
}

/// `/api/inspect`: multipart with `head` (required), and optionally `tail`,
/// `size`, `schema`, `name`, `record_size`, `text_encoding`.
async fn inspect_upload(headers: HeaderMap, body: Body) -> Response {
    let Some(boundary) = boundary_of(&headers) else {
        return problem(
            StatusCode::BAD_REQUEST,
            "send multipart/form-data with a 'head' part",
            None,
        );
    };
    let mut multipart = multer::Multipart::with_constraints(
        body.into_data_stream(),
        boundary,
        multer::Constraints::new().size_limit(
            multer::SizeLimit::new()
                .whole_stream(16 << 20)
                .per_field(9 << 20),
        ),
    );
    let mut parts: HashMap<String, Vec<u8>> = HashMap::new();
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or("").to_owned();
                match field.bytes().await {
                    Ok(bytes) => {
                        parts.insert(name, bytes.to_vec());
                    }
                    Err(e) => return problem(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string(), None),
                }
            }
            Ok(None) => break,
            Err(e) => return problem(StatusCode::BAD_REQUEST, &e.to_string(), None),
        }
    }
    let Some(head) = parts.remove("head") else {
        return problem(StatusCode::BAD_REQUEST, "the 'head' part is missing", None);
    };
    let text = |parts: &HashMap<String, Vec<u8>>, key: &str| {
        parts
            .get(key)
            .map(|v| String::from_utf8_lossy(v).trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let schema = match parts.get("schema").filter(|s| !s.is_empty()) {
        Some(bytes) => match schema_from_bytes(bytes, None) {
            Ok(schema) => Some(schema),
            Err(e) => return error_response(&e),
        },
        None => None,
    };
    let total_size = text(&parts, "size").and_then(|s| s.parse().ok());
    let record_size = text(&parts, "record_size")
        .and_then(|s| s.parse().ok())
        .filter(|&s| s > 0);
    let text_encoding = text(&parts, "text_encoding");
    let name_hint = text(&parts, "name");
    let tail = parts.remove("tail");

    let report = tokio::task::spawn_blocking(move || {
        inspect(Sample {
            head: &head,
            tail: tail.as_deref(),
            total_size,
            schema,
            record_size,
            text_encoding,
            name_hint,
            preview_rows: 20,
        })
    })
    .await;
    match report {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(error)) => error_response(&error),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "inspection crashed",
            None,
        ),
    }
}
