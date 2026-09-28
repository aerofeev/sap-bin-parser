//! The web service and the local app: the same page, the same API.
//!
//! Nothing is stored. An upload is read as a stream by the conversion
//! pipeline and the result streams straight back as a download; there is no
//! temporary file, no database, and no request body in any log. The only
//! per-request state is a set of counters (bytes read, records written) kept
//! in memory under a random id the browser chose, so the page can show
//! progress, and dropped a few minutes after the conversion ends.
//!
//! The service also keeps running totals for its operator (see
//! [`crate::usage`]): conversions, records and bytes by format, input, SAP
//! table and day, never tied to a person or a file. They are private, served
//! only with the operator's token, and written to disk only when a
//! statistics file is configured.
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
//! | `GET /stats`, `GET /api/stats`, `GET /metrics` | usage statistics, with the operator's token |
//! | `POST /api/usage` | the totals of a conversion the browser did itself |

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_stream::wrappers::ReceiverStream;

use crate::archive::schema_from_bytes;
use crate::convert::{convert, Format, Input, InputKind, Meta, Options, Output, Progress, Stats};
use crate::error::Error;
use crate::inspect::{inspect, Sample};
use crate::params::{output_name, ConvertParams};
use crate::usage::{Conversion, Failure, Usage};
use crate::zip::{Entries, EntryInfo};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const FAVICON: &str = include_str!("../web/favicon.svg");
const STATS_HTML: &str = include_str!("../web/stats.html");
const STATS_JS: &str = include_str!("../web/stats.js");
const INTER: &[u8] = include_bytes!("../web/fonts/Inter.var.woff2");
const WORKER_JS: &str = include_str!("../web/convert-worker.js");
const EXPLORER_HTML: &str = include_str!("../web/explorer.html");
const EXPLORER_JS: &str = include_str!("../web/explorer.js");

/// The page's optional parts, if they were built (see build.rs): the engine
/// compiled to WebAssembly, and the Perspective table viewer.
mod parts {
    include!(concat!(env!("OUT_DIR"), "/web_parts.rs"));
}

/// The page's Content-Security-Policy: nothing from anywhere but here, and
/// no connection to anywhere but here.
const CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; font-src 'self'; \
                   connect-src 'self'; form-action 'self'; frame-src 'self'; frame-ancestors 'self'; \
                   base-uri 'none'";

/// The table viewer's frame. Perspective loads parts of itself from `blob:`
/// URLs, as scripts and workers, which the page's own policy forbids; so the
/// viewer runs in a frame of its own with this policy, and the page, which
/// holds the files, keeps the strict one. It still reaches nothing but this
/// origin.
const VIEWER_CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval' blob:; \
                          worker-src 'self' blob:; style-src 'self' 'unsafe-inline'; \
                          img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' blob:; \
                          frame-ancestors 'self'; form-action 'none'; base-uri 'none'";

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
    /// Serve under this path prefix (`/sap-bin-parser`), for a reverse proxy
    /// that hosts several tools on one domain. Empty for the domain root.
    /// Normalise with [`normalise_base_path`].
    pub base_path: String,
    /// Save the usage totals here, so they survive restarts. Without it they
    /// are kept in memory only. Ignored in local mode.
    pub stats_file: Option<PathBuf>,
    /// The bearer token that unlocks the usage statistics. Without it, the
    /// statistics routes do not exist.
    pub stats_token: Option<String>,
}

/// The shortest statistics token accepted.
pub const MIN_TOKEN_LEN: usize = 24;

/// Normalise a base path: `sap-bin-parser/` and `/sap-bin-parser` both give
/// `/sap-bin-parser`; `` and `/` give ``.
pub fn normalise_base_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let valid = trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._~/".contains(&b))
        && !trimmed
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if !valid {
        return Err(format!(
            "base path '{raw}' may only hold letters, digits and - . _ ~ separated by /"
        ));
    }
    Ok(format!("/{trimmed}"))
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
    Done {
        warnings: Vec<String>,
        seconds: f64,
    },
    Failed {
        error: String,
        hint: Option<String>,
        #[serde(skip)]
        status: u16,
    },
}

impl JobState {
    fn failed(error: &Error) -> Self {
        JobState::Failed {
            error: error.to_string(),
            hint: error.hint().map(str::to_owned),
            status: error_status(error).as_u16(),
        }
    }
}

/// The two ends of a conversion that is fed and drained by separate requests:
/// `POST api/jobs/{id}/input` chunks go into `input`, and
/// `GET api/jobs/{id}/download` takes `output`. Browsers and many proxies do
/// not read a response while they are still sending the request, so a single
/// request cannot carry both directions of a large conversion.
/// A conversion's output stream and its readiness signal.
type Download = (mpsc::Receiver<io::Result<Bytes>>, oneshot::Receiver<Meta>);

struct Transfer {
    input: tokio::sync::Mutex<Option<mpsc::Sender<PieceResult>>>,
    output: Mutex<Option<Download>>,
    options: Options,
    multi: bool,
    encoding: Encoding,
    limit: u64,
    uploaded: AtomicU64,
    last_activity: Mutex<Instant>,
    download_connected: AtomicBool,
}

struct Job {
    progress: Arc<Progress>,
    started: Instant,
    state: Mutex<JobState>,
    finished: Mutex<Option<Instant>>,
    transfer: Option<Transfer>,
}

impl Job {
    fn finish(&self, state: JobState) {
        *self.state.lock().unwrap() = state;
        *self.finished.lock().unwrap() = Some(Instant::now());
    }

    fn is_running(&self) -> bool {
        matches!(*self.state.lock().unwrap(), JobState::Running)
    }

    /// Stop feeding the conversion with an error, so it fails rather than
    /// treating the end of input as a (shorter) complete export.
    fn abort_input(&self, reason: &str) {
        self.progress.cancel();
        if let Some(transfer) = &self.transfer {
            if let Ok(mut input) = transfer.input.try_lock() {
                if let Some(tx) = input.take() {
                    let _ = tx.try_send(Err(io::Error::other(reason.to_owned())));
                }
            }
        }
    }
}

struct AppState {
    config: Config,
    permits: Arc<Semaphore>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    usage: Arc<Usage>,
}

type Shared = Arc<AppState>;

/// How long a finished job's counters stay readable.
const JOB_TTL: Duration = Duration::from_secs(300);
const MAX_JOBS: usize = 10_000;
/// A chunked upload that sends nothing for this long is cancelled.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// The chunk size the page uses; requests may carry up to twice this.
const CHUNK_BYTES: usize = 8 << 20;

impl AppState {
    /// Conversions in progress.
    fn running(&self) -> usize {
        self.config.max_concurrency.max(1) - self.permits.available_permits()
    }

    fn register(
        &self,
        id: &str,
        progress: Arc<Progress>,
        transfer: Option<Transfer>,
    ) -> Option<Arc<Job>> {
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
            progress,
            started: Instant::now(),
            state: Mutex::new(JobState::Running),
            finished: Mutex::new(None),
            transfer,
        });
        jobs.insert(id.to_owned(), job.clone());
        Some(job)
    }
}

/// Build the router.
///
/// With a base path, everything is served under it (`/sap-bin-parser/`,
/// `/sap-bin-parser/api/...`) and the bare prefix redirects to it with a
/// trailing slash, since the page uses relative URLs. The same routes also
/// answer at the root, so the service works whether or not a reverse proxy
/// strips the prefix.
pub fn app(config: Config) -> Router {
    let usage = Arc::new(open_usage(&config));
    router(config, usage)
}

/// The usage totals for this configuration: from the statistics file if
/// there is one, in memory otherwise.
fn open_usage(config: &Config) -> Usage {
    match config.stats_file.as_deref().filter(|_| !config.local) {
        None => Usage::in_memory(),
        Some(path) => {
            let (usage, warning) = Usage::open(path);
            if let Some(warning) = warning {
                tracing::warn!("usage statistics: {warning}");
            }
            usage
        }
    }
}

fn router(config: Config, usage: Arc<Usage>) -> Router {
    let base = config.base_path.clone();
    let state = Arc::new(AppState {
        permits: Arc::new(Semaphore::new(config.max_concurrency.max(1))),
        jobs: Mutex::new(HashMap::new()),
        config,
        usage,
    });
    let routes: Router<Shared> = Router::new()
        .route(
            "/assets/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/assets/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route("/fonts/Inter.var.woff2", get(|| async { font(INTER) }))
        .route(
            "/assets/convert-worker.js",
            get(|| async { asset("text/javascript; charset=utf-8", WORKER_JS) }),
        )
        .route("/assets/wasm/sap_bin_wasm.js", get(wasm_glue))
        .route("/assets/wasm/sap_bin_wasm_bg.wasm", get(wasm_module))
        .route("/assets/perspective/{*path}", get(perspective_file))
        .route("/explorer", get(explorer_page))
        .route(
            "/assets/explorer.js",
            get(|| async { asset("text/javascript; charset=utf-8", EXPLORER_JS) }),
        )
        .route(
            "/api/usage",
            post(usage_report).layer(DefaultBodyLimit::max(4096)),
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
        .route("/api/jobs", post(create_job))
        .route(
            "/api/jobs/{id}/input",
            post(job_input).layer(DefaultBodyLimit::max(2 * CHUNK_BYTES)),
        )
        .route("/api/jobs/{id}/download", get(job_download))
        .route("/api/jobs/{id}", get(job_status))
        .route("/api/jobs/{id}/cancel", post(job_cancel))
        .route("/stats", get(stats_page))
        .route("/assets/stats.js", get(stats_script))
        .route("/api/stats", get(stats_json))
        .route("/metrics", get(metrics));

    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(reap_idle_jobs(state.clone()));
    }
    let mut router = Router::new().route("/", get(index)).merge(routes.clone());
    if !base.is_empty() {
        let slashed = format!("{base}/");
        let target = slashed.clone();
        router = router
            .route(
                &base,
                get(move || async move { Redirect::permanent(&target) }),
            )
            .route(&slashed, get(index))
            .nest(&base, routes);
    }
    router
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
    let url = format!("http://{host}:{}{}/", addr.port(), config.base_path);
    let config = Config { addr, ..config };
    if config.local {
        eprintln!(
            "sap-bin {} (made by eidox ai) is running on this computer at {url}",
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
    let usage = Arc::new(open_usage(&config));
    if !config.local {
        match (&config.stats_token, usage.is_saved()) {
            (None, _) => {}
            (Some(_), true) => eprintln!(
                "Usage statistics at {url}stats, saved to {}",
                config
                    .stats_file
                    .as_deref()
                    .unwrap_or_else(|| std::path::Path::new(""))
                    .display()
            ),
            (Some(_), false) => eprintln!("Usage statistics at {url}stats, kept in memory"),
        }
    }
    if usage.is_saved() {
        tokio::spawn(save_usage_every_minute(usage.clone()));
    }
    let on_signal = usage.clone();
    let result = axum::serve(
        listener,
        router(config, usage.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        // Save straight away: open downloads may keep the server alive past
        // the grace period a container runtime allows.
        save_usage(on_signal).await;
    })
    .await;
    save_usage(usage).await;
    result
}

/// Ctrl+C, or SIGTERM from a container runtime or service manager.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn save_usage(usage: Arc<Usage>) {
    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || usage.save()).await {
        tracing::warn!("could not save usage statistics: {e}");
    }
}

async fn save_usage_every_minute(usage: Arc<Usage>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.tick().await;
    loop {
        tick.tick().await;
        save_usage(usage.clone()).await;
    }
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

/// The page's typeface: Inter (SIL Open Font License), the same variable
/// font file Frappe UI uses, served from the binary like everything else.
fn font(body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=604800"),
        ],
        body,
    )
        .into_response()
}

async fn index(State(state): State<Shared>) -> Response {
    state.usage.page_view();
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
        // The page may convert in the browser itself, and show a table viewer.
        "browser": !parts::WASM.is_empty(),
        "viewer": !parts::PERSPECTIVE.is_empty(),
    }))
}

/// `GET assets/wasm/sap_bin_wasm.js`: the JavaScript half of the engine's
/// WebAssembly build.
async fn wasm_glue() -> Response {
    if parts::JS.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset("text/javascript; charset=utf-8", parts::JS)
}

/// `GET assets/wasm/sap_bin_wasm_bg.wasm`: the engine for the browser.
async fn wasm_module(headers: HeaderMap) -> Response {
    if parts::WASM.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    static_file(&headers, "application/wasm", parts::WASM).await
}

/// `GET explorer`: the frame the page shows its table viewer in.
async fn explorer_page() -> Response {
    if parts::PERSPECTIVE.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut response = asset("text/html; charset=utf-8", EXPLORER_HTML);
    response.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_static(VIEWER_CSP),
    );
    response
}

/// `GET assets/perspective/...`: the Perspective table viewer.
async fn perspective_file(Path(path): Path<String>, headers: HeaderMap) -> Response {
    match parts::PERSPECTIVE.iter().find(|(name, _, _)| *name == path) {
        Some((_, kind, body)) => static_file(&headers, kind, body).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A large embedded file: sent compressed (compressed once, on first
/// request) and revalidated by ETag rather than downloaded again.
async fn static_file(headers: &HeaderMap, kind: &'static str, body: &'static [u8]) -> Response {
    type Cache = Mutex<HashMap<(usize, Encoding), &'static [u8]>>;
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    let etag = format!("\"{}-{:x}\"", crate::VERSION, crc32fast::hash(body));
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut response = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let encoding = [Encoding::Zstd, Encoding::Gzip]
            .into_iter()
            .find(|e| e.accepted(headers))
            .unwrap_or(Encoding::Identity);
        let key = (body.as_ptr() as usize, encoding);
        let cached = CACHE
            .get_or_init(Cache::default)
            .lock()
            .unwrap()
            .get(&key)
            .copied();
        let sent = match (cached, encoding) {
            (Some(bytes), _) => bytes,
            (None, Encoding::Identity) => body,
            (None, _) => {
                let bytes = tokio::task::spawn_blocking(move || compress(body, encoding))
                    .await
                    .unwrap_or_default();
                // Kept for the life of the process: a handful of files.
                let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                CACHE
                    .get_or_init(Cache::default)
                    .lock()
                    .unwrap()
                    .insert(key, bytes);
                bytes
            }
        };
        let mut response = ([(header::CONTENT_TYPE, kind)], sent).into_response();
        if let Some(coding) = encoding.header() {
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
        }
        response
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    response
}

fn compress(body: &[u8], encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Zstd => zstd::encode_all(body, 9).unwrap_or_default(),
        Encoding::Gzip => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            let _ = encoder.write_all(body);
            encoder.finish().unwrap_or_default()
        }
        Encoding::Identity => body.to_vec(),
    }
}

/// What the page reports after converting in the browser: totals only, the
/// same ones the service counts for its own conversions.
#[derive(Deserialize)]
struct UsageReport {
    event: String,
    format: Option<String>,
    input: Option<String>,
    table: Option<String>,
    records: Option<u64>,
    shards: Option<u64>,
    bytes_in: Option<u64>,
    bytes_out: Option<u64>,
    seconds: Option<f64>,
    failure: Option<String>,
}

/// `POST api/usage`: count a conversion the browser did itself. The numbers
/// are the browser's word, so they are checked for sense and kept apart
/// under the client `browser`.
async fn usage_report(State(state): State<Shared>, Json(report): Json<UsageReport>) -> Response {
    const FORMATS: [&str; 5] = ["csv", "excel", "tsv", "jsonl", "parquet"];
    let bad = || problem(StatusCode::BAD_REQUEST, "not a usage report", None);
    match report.event.as_str() {
        "conversion" => {
            let Some(format) = report
                .format
                .as_deref()
                .and_then(|f| FORMATS.iter().find(|&&k| k == f))
            else {
                return bad();
            };
            let input = match report.input.as_deref() {
                Some("files") => "files",
                Some("archive") | None => "archive",
                Some(_) => return bad(),
            };
            let seconds = report.seconds.unwrap_or(0.0);
            let records = report.records.unwrap_or(0);
            if !(0.0..1e6).contains(&seconds) || records > 100_000_000_000 {
                return bad();
            }
            state.usage.conversion(&Conversion {
                format,
                input,
                client: "browser",
                table: report.table.as_deref().unwrap_or(""),
                records,
                shards: report.shards.unwrap_or(0).min(1_000_000),
                bytes_in: report.bytes_in.unwrap_or(0).min(1 << 50),
                bytes_out: report.bytes_out.unwrap_or(0).min(1 << 50),
                seconds,
            });
        }
        "inspection" => state.usage.inspection(),
        "sample" => state.usage.sample(),
        "failure" => state.usage.failure(match report.failure.as_deref() {
            Some("cancelled") => Failure::Cancelled,
            Some("server") => Failure::Server,
            _ => Failure::Input,
        }),
        _ => return bad(),
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct SampleParams {
    records: Option<usize>,
    shards: Option<usize>,
}

async fn sample(State(state): State<Shared>, Query(params): Query<SampleParams>) -> Response {
    state.usage.sample();
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
    if !headers.contains_key("content-security-policy") {
        headers.insert("content-security-policy", HeaderValue::from_static(CSP));
    }
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
    problem(error_status(error), &error.to_string(), error.hint())
}

fn error_status(error: &Error) -> StatusCode {
    match error {
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
            progress.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
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
/// How a download is compressed on the wire (`Content-Encoding`). Over the
/// internet a conversion is limited by the network rather than the engine,
/// and CSV shrinks several times over, so compressing makes it that much
/// faster. Browsers decompress as they save: the file on disk is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Encoding {
    Identity,
    Gzip,
    Zstd,
}

impl Encoding {
    /// The best encoding the request accepts, when the output is worth
    /// compressing: text formats (Parquet is compressed already, and so is a
    /// zip of shards), and not on the user's own machine, where it would only
    /// cost time.
    fn choose(config: &Config, options: &Options, headers: &HeaderMap) -> Self {
        if config.local || options.split || options.format == Format::Parquet {
            return Encoding::Identity;
        }
        [Encoding::Zstd, Encoding::Gzip]
            .into_iter()
            .find(|e| e.accepted(headers))
            .unwrap_or(Encoding::Identity)
    }

    /// Whether the request's `Accept-Encoding` allows this encoding.
    fn accepted(self, headers: &HeaderMap) -> bool {
        let Some(name) = self.header() else {
            return true;
        };
        headers
            .get_all(header::ACCEPT_ENCODING)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|item| {
                let mut parts = item.split(';').map(str::trim);
                let coding = parts.next().unwrap_or("");
                let refused = parts.any(|p| {
                    p.strip_prefix("q=")
                        .and_then(|q| q.parse::<f32>().ok())
                        .is_some_and(|q| q <= 0.0)
                });
                coding.eq_ignore_ascii_case(name) && !refused
            })
    }

    fn header(self) -> Option<&'static str> {
        match self {
            Encoding::Identity => None,
            Encoding::Gzip => Some("gzip"),
            Encoding::Zstd => Some("zstd"),
        }
    }
}

enum Encoder {
    Identity,
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl Encoder {
    fn new(encoding: Encoding) -> Self {
        match encoding {
            Encoding::Identity => Encoder::Identity,
            // The fastest levels: they still shrink CSV several times over,
            // and keep compression well ahead of any network.
            Encoding::Gzip => Encoder::Gzip(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::fast(),
            )),
            Encoding::Zstd => match zstd::stream::write::Encoder::new(Vec::new(), 1) {
                Ok(encoder) => Encoder::Zstd(encoder),
                Err(_) => Encoder::Identity,
            },
        }
    }
}

/// Blocking writer into the response body, in 256 KiB chunks, compressed
/// if the client asked for it.
struct ChannelWriter {
    tx: mpsc::Sender<io::Result<Bytes>>,
    buffer: Vec<u8>,
    encoder: Encoder,
    finished: bool,
    sent: Arc<AtomicU64>,
}

const OUT_CHUNK: usize = 256 * 1024;

impl ChannelWriter {
    fn new(tx: mpsc::Sender<io::Result<Bytes>>, encoding: Encoding, sent: Arc<AtomicU64>) -> Self {
        Self {
            tx,
            buffer: Vec::with_capacity(OUT_CHUNK),
            encoder: Encoder::new(encoding),
            finished: false,
            sent,
        }
    }

    /// Send what is buffered; with `finish`, end the compressed stream.
    fn send(&mut self, finish: bool) -> io::Result<()> {
        if self.finished || (self.buffer.is_empty() && !finish) {
            return Ok(());
        }
        let raw = std::mem::replace(&mut self.buffer, Vec::with_capacity(OUT_CHUNK));
        let chunk = match &mut self.encoder {
            Encoder::Identity => raw,
            Encoder::Gzip(encoder) => {
                encoder.write_all(&raw)?;
                if finish {
                    encoder.try_finish()?;
                }
                std::mem::take(encoder.get_mut())
            }
            Encoder::Zstd(encoder) => {
                encoder.write_all(&raw)?;
                if finish {
                    encoder.do_finish()?;
                }
                std::mem::take(encoder.get_mut())
            }
        };
        self.finished = finish;
        if chunk.is_empty() {
            return Ok(());
        }
        self.sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        self.tx
            .blocking_send(Ok(Bytes::from(chunk)))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the client went away"))
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        if self.buffer.len() >= OUT_CHUNK {
            self.send(false)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send(false)
    }
}

impl Drop for ChannelWriter {
    /// The end of the output: the pipeline drops its writer when it is done
    /// (on its blocking thread), which ends the compressed stream. After a
    /// failure the response is aborted anyway, so a truncated stream is never
    /// mistaken for a complete one.
    fn drop(&mut self) {
        let _ = self.send(true);
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

async fn convert_upload(
    State(state): State<Shared>,
    Query(params): Query<ConvertParams>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let job = params
        .job
        .as_deref()
        .and_then(|id| state.register(id, Arc::new(Progress::new()), None));
    let Ok(permit) = state.permits.clone().try_acquire_owned() else {
        state.usage.failure(Failure::Busy);
        let message = BUSY;
        if let Some(job) = &job {
            job.finish(JobState::Failed {
                error: message.into(),
                hint: None,
                status: 503,
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
            state.usage.failure(Failure::Input);
            if let Some(job) = &job {
                job.finish(JobState::Failed {
                    error: message.clone(),
                    hint: None,
                    status: 400,
                });
            }
            return problem(StatusCode::BAD_REQUEST, &message, None);
        }
    };
    let progress = job
        .as_ref()
        .map_or_else(|| Arc::new(Progress::new()), |j| j.progress.clone());
    let fail = |job: &Option<Arc<Job>>, error: &Error| {
        state.usage.failure(failure_kind(error));
        if let Some(job) = job {
            job.finish(JobState::failed(error));
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

    let encoding = Encoding::choose(&state.config, &options, &headers);
    let tally = Tally::new(&options, multi, "api", encoding, progress);
    let (out_rx, ready_rx, handle) = start_pipeline(options.clone(), &tally, multi, permit, in_rx);

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

    tokio::spawn(record_outcome(state.clone(), job, handle, tally));
    download_response(&meta, &options, encoding, out_rx)
}

const BUSY: &str =
    "The server is busy with other conversions. Try again in a minute, or run sap-bin on your own computer.";

type Outcome = tokio::task::JoinHandle<crate::Result<Stats>>;

/// What the usage statistics record about a conversion, besides its
/// [`Stats`]: never a name, a field or a value.
struct Tally {
    format: &'static str,
    input: &'static str,
    client: &'static str,
    encoding: Encoding,
    progress: Arc<Progress>,
    bytes_out: Arc<AtomicU64>,
}

impl Tally {
    fn new(
        options: &Options,
        multi: bool,
        client: &'static str,
        encoding: Encoding,
        progress: Arc<Progress>,
    ) -> Self {
        let format = match options.format {
            Format::Csv if options.bom => "excel",
            format => format.extension(),
        };
        Self {
            format,
            input: if multi { "files" } else { "archive" },
            client,
            encoding,
            progress,
            bytes_out: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// How a failed conversion counts in the usage statistics.
fn failure_kind(error: &Error) -> Failure {
    match error {
        Error::Io(e) if e.kind() == io::ErrorKind::BrokenPipe => Failure::Cancelled,
        other => Failure::from_status(error_status(other).as_u16()),
    }
}

/// Start a conversion on the blocking pool, reading pieces from `input`.
/// Returns the output stream, the readiness signal (sent once the first
/// block has decoded) and the conversion's outcome.
fn start_pipeline(
    options: Options,
    tally: &Tally,
    multi: bool,
    permit: tokio::sync::OwnedSemaphorePermit,
    input: mpsc::Receiver<PieceResult>,
) -> (
    mpsc::Receiver<io::Result<Bytes>>,
    oneshot::Receiver<Meta>,
    Outcome,
) {
    let (out_tx, out_rx) = mpsc::channel::<io::Result<Bytes>>(8);
    let (ready_tx, ready_rx) = oneshot::channel::<Meta>();
    let progress = tally.progress.clone();
    let sent = tally.bytes_out.clone();
    let encoding = tally.encoding;
    let handle = tokio::task::spawn_blocking(move || {
        let reader = ChannelReader::new(input, progress.clone(), multi);
        let input = if multi {
            Input::Files(Box::new(reader))
        } else {
            Input::Reader(Box::new(reader))
        };
        let writer = ChannelWriter::new(out_tx.clone(), encoding, sent);
        let result = convert(
            input,
            Output::Writer(Box::new(writer)),
            &options,
            &progress,
            |meta| {
                let _ = ready_tx.send(meta.clone());
            },
        );
        if let Err(error) = &result {
            // Abort the response so a failed conversion never looks complete.
            let _ = out_tx.blocking_send(Err(io::Error::other(error.to_string())));
        }
        drop(permit);
        result.map_err(unwrap_limit)
    });
    (out_rx, ready_rx, handle)
}

/// Record a conversion's outcome on its job, for the page's progress poll,
/// and in the usage statistics.
async fn record_outcome(state: Shared, job: Option<Arc<Job>>, handle: Outcome, tally: Tally) {
    let outcome = handle.await;
    match &outcome {
        Ok(Ok(stats)) => state.usage.conversion(&Conversion {
            format: tally.format,
            input: tally.input,
            client: tally.client,
            table: &stats.table,
            records: stats.records,
            shards: stats.shards,
            bytes_in: tally.progress.bytes_in.load(Ordering::Relaxed),
            bytes_out: tally.bytes_out.load(Ordering::Relaxed),
            seconds: stats.elapsed.as_secs_f64(),
        }),
        Ok(Err(error)) => state.usage.failure(failure_kind(error)),
        Err(_) => state.usage.failure(Failure::Server),
    }
    if let Some(job) = job {
        job.finish(match outcome {
            Ok(Ok(stats)) => done_state(&stats),
            Ok(Err(error)) => JobState::failed(&error),
            Err(_) => JobState::Failed {
                error: "the conversion crashed".into(),
                hint: None,
                status: 500,
            },
        });
    }
}

/// The streamed attachment for a conversion that has started producing.
fn download_response(
    meta: &Meta,
    options: &Options,
    encoding: Encoding,
    output: mpsc::Receiver<io::Result<Bytes>>,
) -> Response {
    let filename = output_name(&meta.table, options);
    let content_type = if options.split {
        "application/zip"
    } else {
        options.format.content_type()
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::VARY, "accept-encoding");
    if let Some(coding) = encoding.header() {
        builder = builder.header(header::CONTENT_ENCODING, coding);
    }
    builder
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header("x-sap-bin-table", meta.table.as_str())
        .body(Body::from_stream(ReceiverStream::new(output)))
        .unwrap_or_else(|_| {
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not start the download",
                None,
            )
        })
}

fn job_of(state: &AppState, id: &str) -> Option<Arc<Job>> {
    state.jobs.lock().unwrap().get(id).cloned()
}

/// The failure a job ended with, waiting briefly for it to be recorded.
async fn failure_of(job: &Job) -> Response {
    for _ in 0..100 {
        if let JobState::Failed {
            error,
            hint,
            status,
        } = &*job.state.lock().unwrap()
        {
            let code = StatusCode::from_u16(*status).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY);
            return problem(code, error, hint.as_deref());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    problem(
        StatusCode::CONFLICT,
        "the conversion is no longer running",
        None,
    )
}

/// `POST api/jobs?job=ID&...`: start a conversion fed by chunked uploads.
/// The body, if any, is the schema sidecar. Query parameters are those of
/// `api/convert`.
async fn create_job(
    State(state): State<Shared>,
    Query(params): Query<ConvertParams>,
    headers: HeaderMap,
    schema: Bytes,
) -> Response {
    let Some(id) = params.job.clone() else {
        return problem(
            StatusCode::BAD_REQUEST,
            "a job id is required (?job=...)",
            None,
        );
    };
    let mut options = match params.options(state.config.threads_per_job()) {
        Ok(options) => options,
        Err(message) => {
            state.usage.failure(Failure::Input);
            return problem(StatusCode::BAD_REQUEST, &message, None);
        }
    };
    if !schema.is_empty() {
        match schema_from_bytes(&schema, None) {
            Ok(parsed) => options.schema = Some(parsed),
            Err(error) => {
                state.usage.failure(Failure::Input);
                return error_response(&error);
            }
        }
    }
    let Ok(permit) = state.permits.clone().try_acquire_owned() else {
        state.usage.failure(Failure::Busy);
        let mut response = problem(StatusCode::SERVICE_UNAVAILABLE, BUSY, None);
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
        return response;
    };
    let multi = params.multi.unwrap_or(false);
    let (in_tx, in_rx) = mpsc::channel(4);
    let progress = Arc::new(Progress::new());
    // Chosen now, from this request: the pipeline starts before the download
    // is requested. Both come from the same browser; the download checks.
    let encoding = Encoding::choose(&state.config, &options, &headers);
    let tally = Tally::new(&options, multi, "page", encoding, progress.clone());
    let (output, ready, handle) = start_pipeline(options.clone(), &tally, multi, permit, in_rx);
    let transfer = Transfer {
        input: tokio::sync::Mutex::new(Some(in_tx)),
        output: Mutex::new(Some((output, ready))),
        options,
        multi,
        encoding,
        limit: if state.config.local {
            0
        } else {
            state.config.max_upload
        },
        uploaded: AtomicU64::new(0),
        last_activity: Mutex::new(Instant::now()),
        download_connected: AtomicBool::new(false),
    };
    let Some(job) = state.register(&id, progress, Some(transfer)) else {
        handle.abort();
        return problem(
            StatusCode::CONFLICT,
            "that job id is taken or invalid",
            None,
        );
    };
    tokio::spawn(record_outcome(state.clone(), Some(job), handle, tally));
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "chunk_bytes": CHUNK_BYTES })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct InputParams {
    /// First chunk of a file: its name (multi-file jobs).
    name: Option<String>,
    start: Option<bool>,
    /// No more input.
    end: Option<bool>,
}

/// `POST api/jobs/{id}/input`: the next chunk of the upload, in order.
/// Answers once the converter has taken the chunk, which is what keeps
/// memory bounded: a client uploads no faster than the conversion runs.
async fn job_input(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(params): Query<InputParams>,
    chunk: Bytes,
) -> Response {
    let Some(job) = job_of(&state, &id) else {
        return problem(StatusCode::NOT_FOUND, "no such job", None);
    };
    let Some(transfer) = &job.transfer else {
        return problem(
            StatusCode::CONFLICT,
            "this job does not take chunked input",
            None,
        );
    };
    let mut input = transfer.input.lock().await;
    let Some(tx) = input.as_ref() else {
        return if job.is_running() {
            problem(
                StatusCode::CONFLICT,
                "the input of this job is already complete",
                None,
            )
        } else {
            failure_of(&job).await
        };
    };
    *transfer.last_activity.lock().unwrap() = Instant::now();
    let uploaded = transfer
        .uploaded
        .fetch_add(chunk.len() as u64, Ordering::Relaxed)
        + chunk.len() as u64;
    if transfer.limit > 0 && uploaded > transfer.limit {
        let error = Error::Limit(limit_error(transfer.limit).to_string());
        let _ = tx.send(Err(limit_error(transfer.limit))).await;
        input.take();
        return error_response(&error);
    }
    let mut delivered = true;
    if transfer.multi && params.start.unwrap_or(false) {
        let name = params.name.clone().unwrap_or_else(|| "DATA".into());
        delivered &= tx.send(Ok(Piece::Begin(name))).await.is_ok();
    }
    if delivered && !chunk.is_empty() {
        delivered &= tx.send(Ok(Piece::Chunk(chunk))).await.is_ok();
    }
    *transfer.last_activity.lock().unwrap() = Instant::now();
    if !delivered {
        input.take();
        return failure_of(&job).await;
    }
    if params.end.unwrap_or(false) {
        input.take();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `GET api/jobs/{id}/download`: the converted output, streamed.
async fn job_download(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(job) = job_of(&state, &id) else {
        return problem(StatusCode::NOT_FOUND, "no such job", None);
    };
    let Some(transfer) = &job.transfer else {
        return problem(
            StatusCode::CONFLICT,
            "this job has no separate download",
            None,
        );
    };
    let expected = transfer.encoding;
    if !expected.accepted(&headers) {
        job.abort_input("the download did not accept the job's compression");
        return problem(
            StatusCode::NOT_ACCEPTABLE,
            "this download is compressed, but the request does not accept compression",
            Some("send the same Accept-Encoding when creating the job and downloading it"),
        );
    }
    let Some((output, ready)) = transfer.output.lock().unwrap().take() else {
        return problem(
            StatusCode::CONFLICT,
            "the download has already started",
            None,
        );
    };
    transfer.download_connected.store(true, Ordering::Relaxed);
    match ready.await {
        Ok(meta) => download_response(&meta, &transfer.options, expected, output),
        Err(_) => failure_of(&job).await,
    }
}

/// Cancel chunked uploads that have gone quiet.
async fn reap_idle_jobs(state: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let jobs: Vec<Arc<Job>> = state.jobs.lock().unwrap().values().cloned().collect();
        for job in jobs {
            let Some(transfer) = &job.transfer else {
                continue;
            };
            let idle = transfer.last_activity.lock().unwrap().elapsed();
            if job.is_running() && idle > IDLE_TIMEOUT {
                job.abort_input(
                    "the upload stopped for ten minutes, so the conversion was cancelled",
                );
            }
        }
    }
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
    /// Bytes received from a chunked upload (the pipeline's `bytes_in` is
    /// what it has consumed).
    uploaded: u64,
    /// Whether the browser has opened the download of a chunked job.
    download_connected: bool,
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
            let (uploaded, download_connected) = job.transfer.as_ref().map_or((0, false), |t| {
                (
                    t.uploaded.load(Ordering::Relaxed),
                    t.download_connected.load(Ordering::Relaxed),
                )
            });
            let status = JobStatus {
                state: job.state.lock().unwrap().clone(),
                uploaded,
                download_connected,
                bytes_in: progress.bytes_in.load(Ordering::Relaxed),
                records: progress.records.load(Ordering::Relaxed),
                shards: progress.shards.load(Ordering::Relaxed),
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
            job.abort_input("the conversion was cancelled");
            StatusCode::NO_CONTENT.into_response()
        }
        None => problem(StatusCode::NOT_FOUND, "no such job", None),
    }
}

/// `/api/inspect`: multipart with `head` (required), and optionally `tail`,
/// `size`, `schema`, `name`, `record_size`, `text_encoding`.
async fn inspect_upload(State(state): State<Shared>, headers: HeaderMap, body: Body) -> Response {
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
        Ok(Ok(report)) => {
            state.usage.inspection();
            Json(report).into_response()
        }
        Ok(Err(error)) => error_response(&error),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "inspection crashed",
            None,
        ),
    }
}

/// The refusal for a statistics request without the operator's token.
/// Without a token configured the statistics routes do not exist (404); a
/// missing or wrong token gets 401.
fn refuse_stats(config: &Config, headers: &HeaderMap) -> Option<Response> {
    let Some(token) = config.stats_token.as_deref() else {
        return Some(StatusCode::NOT_FOUND.into_response());
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if same_secret(given.trim().as_bytes(), token.as_bytes()) {
        None
    } else {
        let mut response = problem(StatusCode::UNAUTHORIZED, "unauthorized", None);
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        Some(response)
    }
}

/// Compare secrets in time that depends only on their length.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `GET stats`: the operator's dashboard. The page itself holds no numbers;
/// it asks for the token and reads `api/stats`.
async fn stats_page(State(state): State<Shared>) -> Response {
    if state.config.stats_token.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset("text/html; charset=utf-8", STATS_HTML)
}

async fn stats_script(State(state): State<Shared>) -> Response {
    if state.config.stats_token.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset("text/javascript; charset=utf-8", STATS_JS)
}

/// `GET api/stats`: every total, as JSON.
async fn stats_json(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(response) = refuse_stats(&state.config, &headers) {
        return response;
    }
    let mut body = serde_json::to_value(state.usage.snapshot()).unwrap_or_default();
    if let Some(object) = body.as_object_mut() {
        object.insert("service_version".into(), json!(crate::VERSION));
        object.insert("today".into(), json!(crate::usage::today()));
        object.insert("uptime_seconds".into(), json!(state.usage.uptime_seconds()));
        object.insert("running".into(), json!(state.running()));
        object.insert("saved".into(), json!(state.usage.is_saved()));
    }
    Json(body).into_response()
}

/// `GET metrics`: the totals for Prometheus.
async fn metrics(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(response) = refuse_stats(&state.config, &headers) {
        return response;
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.usage.prometheus(state.running()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn config(base: &str) -> Config {
        Config {
            addr: "127.0.0.1:0".parse().unwrap(),
            local: false,
            max_upload: 0,
            max_concurrency: 1,
            threads: 1,
            open_browser: false,
            base_path: normalise_base_path(base).unwrap(),
            stats_file: None,
            stats_token: None,
        }
    }

    async fn get(app: &Router, path: &str) -> (StatusCode, Option<String>) {
        let request = HttpRequest::builder()
            .uri(path)
            .header(header::HOST, "tools.example.com")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        (response.status(), location)
    }

    #[test]
    fn normalises_base_paths() {
        assert_eq!(normalise_base_path("").unwrap(), "");
        assert_eq!(normalise_base_path("/").unwrap(), "");
        assert_eq!(
            normalise_base_path("sap-bin-parser/").unwrap(),
            "/sap-bin-parser"
        );
        assert_eq!(
            normalise_base_path("/tools/sap-bin").unwrap(),
            "/tools/sap-bin"
        );
        assert!(normalise_base_path("/a b").is_err());
        assert!(normalise_base_path("/a/../b").is_err());
        assert!(normalise_base_path("//a").is_err() || normalise_base_path("//a").unwrap() == "/a");
    }

    #[tokio::test]
    async fn serves_under_a_base_path_and_at_the_root() {
        let app = app(config("/sap-bin-parser"));
        assert_eq!(
            get(&app, "/sap-bin-parser").await,
            (
                StatusCode::PERMANENT_REDIRECT,
                Some("/sap-bin-parser/".into())
            )
        );
        for path in [
            "/sap-bin-parser/",
            "/sap-bin-parser/assets/app.js",
            "/sap-bin-parser/api/config",
            "/sap-bin-parser/healthz",
            // A proxy that strips the prefix reaches the same routes.
            "/",
            "/assets/app.js",
            "/api/config",
        ] {
            assert_eq!(get(&app, path).await.0, StatusCode::OK, "{path}");
        }
        assert_eq!(get(&app, "/elsewhere/").await.0, StatusCode::NOT_FOUND);
    }

    const TOKEN: &str = "test-token-0123456789abcdef";

    async fn send(app: &Router, request: HttpRequest<Body>) -> (StatusCode, Bytes) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body)
    }

    fn stats_request(path: &str, token: Option<&str>) -> HttpRequest<Body> {
        let mut request = HttpRequest::builder()
            .uri(path)
            .header(header::HOST, "tools.example.com");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        request.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn statistics_do_not_exist_without_a_token() {
        let app = app(config(""));
        for path in ["/stats", "/assets/stats.js", "/api/stats", "/metrics"] {
            let (status, _) = send(&app, stats_request(path, Some(TOKEN))).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn statistics_need_the_token_and_count_conversions() {
        let app = app(Config {
            stats_token: Some(TOKEN.into()),
            ..config("/sap-bin-parser")
        });
        for path in ["/sap-bin-parser/api/stats", "/metrics"] {
            assert_eq!(
                send(&app, stats_request(path, None)).await.0,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                send(
                    &app,
                    stats_request(path, Some("test-token-0123456789abcdeX"))
                )
                .await
                .0,
                StatusCode::UNAUTHORIZED
            );
        }
        // The dashboard shell holds no numbers, so it loads without the token.
        assert_eq!(
            send(&app, stats_request("/sap-bin-parser/stats", None))
                .await
                .0,
            StatusCode::OK
        );

        let archive = crate::sample::sample_archive(1234, 1);
        let convert = HttpRequest::builder()
            .method(Method::POST)
            .uri("/sap-bin-parser/api/convert?format=csv&bom=true")
            .header(header::HOST, "tools.example.com")
            .body(Body::from(archive))
            .unwrap();
        let (status, csv) = send(&app, convert).await;
        assert_eq!(status, StatusCode::OK);

        let mut stats = serde_json::Value::Null;
        for _ in 0..100 {
            let (status, body) = send(
                &app,
                stats_request("/sap-bin-parser/api/stats", Some(TOKEN)),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            stats = serde_json::from_slice(&body).unwrap();
            if stats["totals"]["conversions"] == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(stats["totals"]["conversions"], 1);
        assert_eq!(stats["totals"]["records"], 1234);
        assert_eq!(stats["totals"]["bytes_out"], csv.len() as u64);
        assert_eq!(stats["formats"]["excel"]["records"], 1234);
        assert_eq!(stats["clients"]["api"]["conversions"], 1);
        assert_eq!(stats["inputs"]["archive"]["conversions"], 1);
        assert_eq!(stats["tables"]["BSIS"]["records"], 1234);
        assert_eq!(stats["saved"], false);

        let (status, text) = send(&app, stats_request("/metrics", Some(TOKEN))).await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(text.to_vec()).unwrap();
        assert!(text.contains("sapbin_records_total 1234\n"), "{text}");
    }

    #[tokio::test]
    async fn counts_what_the_browser_converted() {
        let app = app(Config {
            stats_token: Some(TOKEN.into()),
            ..config("")
        });
        let post = |body: &str| {
            HttpRequest::builder()
                .method(Method::POST)
                .uri("/api/usage")
                .header(header::HOST, "tools.example.com")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap()
        };
        let conversion = r#"{"event":"conversion","format":"excel","input":"files","table":"bkpf",
            "records":5000,"shards":2,"bytes_in":630000,"bytes_out":900000,"seconds":0.02}"#;
        assert_eq!(send(&app, post(conversion)).await.0, StatusCode::NO_CONTENT);
        assert_eq!(
            send(&app, post(r#"{"event":"inspection"}"#)).await.0,
            StatusCode::NO_CONTENT
        );
        for nonsense in [
            r#"{"event":"conversion","format":"exe","records":1}"#,
            r#"{"event":"conversion","format":"csv","records":1e300}"#,
            r#"{"event":"conversion","format":"csv","input":"disk"}"#,
            r#"{"event":"party"}"#,
        ] {
            let status = send(&app, post(nonsense)).await.0;
            assert!(status.is_client_error(), "{nonsense}: {status}");
        }
        let (_, body) = send(&app, stats_request("/api/stats", Some(TOKEN))).await;
        let stats: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stats["totals"]["conversions"], 1);
        assert_eq!(stats["totals"]["inspections"], 1);
        assert_eq!(stats["clients"]["browser"]["records"], 5000);
        assert_eq!(stats["formats"]["excel"]["conversions"], 1);
        assert_eq!(stats["tables"]["BKPF"]["records"], 5000);
    }

    #[test]
    fn compresses_text_downloads_when_the_client_accepts_it() {
        let accept = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
            headers
        };
        let server = config("");
        let csv = Options::default();
        let choose = |config: &Config, options: &Options, value: &str| {
            Encoding::choose(config, options, &accept(value))
        };
        assert_eq!(
            choose(&server, &csv, "gzip, deflate, br, zstd"),
            Encoding::Zstd
        );
        assert_eq!(choose(&server, &csv, "gzip, deflate, br"), Encoding::Gzip);
        assert_eq!(
            choose(&server, &csv, "zstd;q=0, GZIP;q=0.5"),
            Encoding::Gzip
        );
        assert_eq!(choose(&server, &csv, "identity"), Encoding::Identity);
        assert_eq!(
            Encoding::choose(&server, &csv, &HeaderMap::new()),
            Encoding::Identity
        );
        let parquet = Options {
            format: Format::Parquet,
            ..Options::default()
        };
        assert_eq!(choose(&server, &parquet, "zstd"), Encoding::Identity);
        let split = Options {
            split: true,
            ..Options::default()
        };
        assert_eq!(choose(&server, &split, "zstd"), Encoding::Identity);
        let local = Config {
            local: true,
            ..config("")
        };
        assert_eq!(choose(&local, &csv, "zstd"), Encoding::Identity);
        assert!(Encoding::Zstd.accepted(&accept("br, zstd")));
        assert!(!Encoding::Zstd.accepted(&accept("gzip")));
        assert!(Encoding::Identity.accepted(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn serves_at_the_root_without_a_base_path() {
        let app = app(config(""));
        assert_eq!(get(&app, "/").await.0, StatusCode::OK);
        assert_eq!(get(&app, "/api/config").await.0, StatusCode::OK);
    }
}
