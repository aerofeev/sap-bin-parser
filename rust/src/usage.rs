//! Usage statistics for the web service: running totals, never tied to a
//! person, an address or a file.
//!
//! What is counted is exactly what [`Snapshot`] holds: numbers of
//! conversions, records and bytes, broken down by output format, by input
//! shape, by SAP table name and by day. There is no IP address, user agent,
//! file name, field name or value anywhere in it. Table names are kept only
//! when they look like SAP table names, so a schema typed into the page
//! cannot smuggle free text in.
//!
//! The totals live in memory. With a statistics file configured, they are
//! loaded from it at start and saved to it (atomically, through a temporary
//! file and a rename) every minute and on shutdown; that file is the only
//! thing the service ever writes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Distinct table names tracked; later ones are counted as [`OTHER`].
pub const MAX_TABLES: usize = 1000;
/// Days of per-day totals kept.
pub const DAYS_KEPT: usize = 90;
/// Where a table name that is not kept is counted.
pub const OTHER: &str = "(other)";
/// Conversions shorter than this do not set the peak throughput: timing
/// noise would dominate.
const PEAK_MIN_SECONDS: f64 = 1.0;
const FILE_VERSION: u32 = 1;

/// Conversions and records, for one format, input shape or table.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counts {
    pub conversions: u64,
    pub records: u64,
}

/// One UTC day.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Day {
    pub conversions: u64,
    pub records: u64,
    pub bytes_in: u64,
    pub failures: u64,
    pub page_views: u64,
}

/// Everything since counting began.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Totals {
    /// Conversions that completed.
    pub conversions: u64,
    /// Conversions refused or stopped because of the input or the request
    /// (a wrong schema, a record that will not decode, too large).
    pub failed_input: u64,
    /// Conversions that failed on the server's side.
    pub failed_server: u64,
    /// Conversions turned away because the server was busy.
    pub busy: u64,
    /// Conversions cancelled by the user or abandoned.
    pub cancelled: u64,
    pub records: u64,
    pub shards: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Time spent converting, summed over conversions.
    pub seconds: f64,
    pub inspections: u64,
    pub samples: u64,
    pub page_views: u64,
}

/// The largest single conversion.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Largest {
    pub records: u64,
    pub bytes_in: u64,
    pub day: String,
}

/// All the statistics, as saved to the file and served by `api/stats`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub version: u32,
    /// The UTC day counting began.
    pub since: String,
    pub totals: Totals,
    /// By output format: csv, excel (CSV with a BOM), tsv, jsonl, parquet.
    pub formats: BTreeMap<String, Counts>,
    /// By input shape: `archive` (one file) or `files` (a folder or several
    /// files).
    pub inputs: BTreeMap<String, Counts>,
    /// By client: `page` (the chunked protocol the page uses) or `api`
    /// (a single request, as from curl).
    pub clients: BTreeMap<String, Counts>,
    /// By SAP table name.
    pub tables: BTreeMap<String, Counts>,
    /// By UTC day (`YYYY-MM-DD`), the last [`DAYS_KEPT`] days with activity.
    pub days: BTreeMap<String, Day>,
    pub largest: Largest,
    pub peak_records_per_second: f64,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            since: today(),
            totals: Totals::default(),
            formats: BTreeMap::new(),
            inputs: BTreeMap::new(),
            clients: BTreeMap::new(),
            tables: BTreeMap::new(),
            days: BTreeMap::new(),
            largest: Largest::default(),
            peak_records_per_second: 0.0,
        }
    }
}

/// A completed conversion, as the server reports it.
#[derive(Debug, Clone, Default)]
pub struct Conversion<'a> {
    pub format: &'a str,
    pub input: &'a str,
    pub client: &'a str,
    pub table: &'a str,
    pub records: u64,
    pub shards: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub seconds: f64,
}

/// How a conversion that did not complete ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Input,
    Server,
    Busy,
    Cancelled,
}

impl Failure {
    /// Classify by the HTTP status the client was given.
    pub fn from_status(status: u16) -> Self {
        match status {
            503 => Failure::Busy,
            499 => Failure::Cancelled,
            400..=499 => Failure::Input,
            _ => Failure::Server,
        }
    }
}

struct Inner {
    data: Snapshot,
    dirty: bool,
}

/// The service's running totals.
pub struct Usage {
    inner: Mutex<Inner>,
    started: Instant,
    /// Where to save; `None` when there is no file, or when the file could
    /// not be read (so history is never overwritten by an empty start).
    file: Option<PathBuf>,
}

impl Default for Usage {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl Usage {
    /// Totals kept in memory only; nothing is ever written.
    pub fn in_memory() -> Self {
        Self::from_snapshot(Snapshot::default(), None)
    }

    /// Totals loaded from, and saved to, `path`. A missing file starts from
    /// zero. A file that cannot be read or understood also starts from zero,
    /// but is then left alone: saving stays off, and the returned message
    /// says why.
    pub fn open(path: &Path) -> (Self, Option<String>) {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Snapshot>(&bytes) {
                Ok(data) if data.version == FILE_VERSION => {
                    (Self::from_snapshot(data, Some(path.to_owned())), None)
                }
                Ok(data) => (
                    Self::in_memory(),
                    Some(format!(
                        "{} has statistics format {}, this version reads {FILE_VERSION}; \
                         counting in memory and leaving the file alone",
                        path.display(),
                        data.version
                    )),
                ),
                Err(e) => (
                    Self::in_memory(),
                    Some(format!(
                        "{} could not be read as statistics ({e}); \
                         counting in memory and leaving the file alone",
                        path.display()
                    )),
                ),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => (
                Self::from_snapshot(Snapshot::default(), Some(path.to_owned())),
                None,
            ),
            Err(e) => (
                Self::in_memory(),
                Some(format!(
                    "{} could not be opened ({e}); \
                     counting in memory and leaving the file alone",
                    path.display()
                )),
            ),
        }
    }

    fn from_snapshot(data: Snapshot, file: Option<PathBuf>) -> Self {
        Self {
            inner: Mutex::new(Inner { data, dirty: false }),
            started: Instant::now(),
            file,
        }
    }

    /// Whether totals are being saved to a file.
    pub fn is_saved(&self) -> bool {
        self.file.is_some()
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn update(&self, change: impl FnOnce(&mut Snapshot, &str)) {
        let day = today();
        let mut inner = self.inner.lock().unwrap();
        change(&mut inner.data, &day);
        prune_days(&mut inner.data.days);
        inner.dirty = true;
    }

    pub fn conversion(&self, c: &Conversion<'_>) {
        self.update(|data, day| {
            let t = &mut data.totals;
            t.conversions += 1;
            t.records += c.records;
            t.shards += c.shards;
            t.bytes_in += c.bytes_in;
            t.bytes_out += c.bytes_out;
            t.seconds += c.seconds;
            for (map, key) in [
                (&mut data.formats, c.format),
                (&mut data.inputs, c.input),
                (&mut data.clients, c.client),
            ] {
                let counts = map.entry(key.to_owned()).or_default();
                counts.conversions += 1;
                counts.records += c.records;
            }
            let table = table_key(&data.tables, c.table);
            let counts = data.tables.entry(table).or_default();
            counts.conversions += 1;
            counts.records += c.records;
            let d = data.days.entry(day.to_owned()).or_default();
            d.conversions += 1;
            d.records += c.records;
            d.bytes_in += c.bytes_in;
            if c.records > data.largest.records {
                data.largest = Largest {
                    records: c.records,
                    bytes_in: c.bytes_in,
                    day: day.to_owned(),
                };
            }
            if c.seconds >= PEAK_MIN_SECONDS {
                let rate = c.records as f64 / c.seconds;
                if rate > data.peak_records_per_second {
                    data.peak_records_per_second = rate;
                }
            }
        });
    }

    pub fn failure(&self, failure: Failure) {
        self.update(|data, day| {
            let t = &mut data.totals;
            match failure {
                Failure::Input => t.failed_input += 1,
                Failure::Server => t.failed_server += 1,
                Failure::Busy => t.busy += 1,
                Failure::Cancelled => t.cancelled += 1,
            }
            if failure != Failure::Cancelled {
                data.days.entry(day.to_owned()).or_default().failures += 1;
            }
        });
    }

    pub fn inspection(&self) {
        self.update(|data, _| data.totals.inspections += 1);
    }

    pub fn sample(&self) {
        self.update(|data, _| data.totals.samples += 1);
    }

    pub fn page_view(&self) {
        self.update(|data, day| {
            data.totals.page_views += 1;
            data.days.entry(day.to_owned()).or_default().page_views += 1;
        });
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap().data.clone()
    }

    /// Save to the statistics file if anything changed since the last save.
    /// Blocking; returns whether a file was written.
    pub fn save(&self) -> io::Result<bool> {
        let Some(path) = &self.file else {
            return Ok(false);
        };
        let data = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.dirty {
                return Ok(false);
            }
            inner.dirty = false;
            inner.data.clone()
        };
        let result = write_atomically(path, &serde_json::to_vec_pretty(&data)?);
        if result.is_err() {
            self.inner.lock().unwrap().dirty = true;
        }
        result.map(|()| true)
    }

    /// The totals in the Prometheus text exposition format. `running` is the
    /// number of conversions in progress.
    pub fn prometheus(&self, running: usize) -> String {
        let data = self.snapshot();
        let t = &data.totals;
        let mut out = String::new();
        let mut metric = |name: &str, kind: &str, help: &str, samples: &[(String, f64)]| {
            let _ = writeln!(out, "# HELP sapbin_{name} {help}");
            let _ = writeln!(out, "# TYPE sapbin_{name} {kind}");
            for (labels, value) in samples {
                let _ = writeln!(out, "sapbin_{name}{labels} {value}");
            }
        };
        let one = |value: f64| vec![(String::new(), value)];
        let by = |label: &str, map: &BTreeMap<String, Counts>, pick: fn(&Counts) -> u64| {
            map.iter()
                .map(|(key, counts)| {
                    (
                        format!("{{{label}=\"{}\"}}", escape_label(key)),
                        pick(counts) as f64,
                    )
                })
                .collect::<Vec<_>>()
        };
        let results = [
            ("done", t.conversions),
            ("failed_input", t.failed_input),
            ("failed_server", t.failed_server),
            ("busy", t.busy),
            ("cancelled", t.cancelled),
        ]
        .iter()
        .map(|(result, n)| (format!("{{result=\"{result}\"}}"), *n as f64))
        .collect::<Vec<_>>();
        metric(
            "conversions_total",
            "counter",
            "Conversions by how they ended.",
            &results,
        );
        metric(
            "records_total",
            "counter",
            "Records converted.",
            &one(t.records as f64),
        );
        metric(
            "shards_total",
            "counter",
            "Shards converted.",
            &one(t.shards as f64),
        );
        metric(
            "bytes_in_total",
            "counter",
            "Bytes of exports read.",
            &one(t.bytes_in as f64),
        );
        metric(
            "bytes_out_total",
            "counter",
            "Bytes of output written.",
            &one(t.bytes_out as f64),
        );
        metric(
            "conversion_seconds_total",
            "counter",
            "Time spent converting.",
            &one(t.seconds),
        );
        metric(
            "inspections_total",
            "counter",
            "Exports inspected.",
            &one(t.inspections as f64),
        );
        metric(
            "samples_total",
            "counter",
            "Sample exports downloaded.",
            &one(t.samples as f64),
        );
        metric(
            "page_views_total",
            "counter",
            "Loads of the page.",
            &one(t.page_views as f64),
        );
        for (label, map) in [
            ("format", &data.formats),
            ("input", &data.inputs),
            ("client", &data.clients),
            ("table", &data.tables),
        ] {
            metric(
                &format!("{label}_conversions_total"),
                "counter",
                &format!("Completed conversions by {label}."),
                &by(label, map, |c| c.conversions),
            );
            metric(
                &format!("{label}_records_total"),
                "counter",
                &format!("Records converted by {label}."),
                &by(label, map, |c| c.records),
            );
        }
        metric(
            "peak_records_per_second",
            "gauge",
            "Fastest conversion of a second or more.",
            &one(data.peak_records_per_second),
        );
        metric(
            "running_conversions",
            "gauge",
            "Conversions in progress.",
            &one(running as f64),
        );
        metric(
            "uptime_seconds",
            "gauge",
            "Seconds since the service started.",
            &one(self.uptime_seconds() as f64),
        );
        out
    }
}

/// The key a table is counted under: its SAP name, upper-cased, or
/// [`OTHER`] when it does not look like one or too many are tracked.
fn table_key(tables: &BTreeMap<String, Counts>, name: &str) -> String {
    let name = name.trim().to_ascii_uppercase();
    let valid = (1..=30).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'/');
    if !valid || (!tables.contains_key(&name) && tables.len() >= MAX_TABLES) {
        OTHER.to_owned()
    } else {
        name
    }
}

fn prune_days(days: &mut BTreeMap<String, Day>) {
    while days.len() > DAYS_KEPT {
        days.pop_first();
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Today's UTC date as `YYYY-MM-DD`.
pub fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (year, month, day) = crate::zip::civil_from_days(secs.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Write `bytes` to `path` through `path.tmp` and a rename, so a crash
/// mid-write never leaves a half-written file behind.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bsis(records: u64) -> Conversion<'static> {
        Conversion {
            format: "parquet",
            input: "archive",
            client: "page",
            table: "bsis",
            records,
            shards: 2,
            bytes_in: records * 126,
            bytes_out: records * 20,
            seconds: 2.0,
        }
    }

    #[test]
    fn counts_conversions_by_every_dimension() {
        let usage = Usage::in_memory();
        usage.conversion(&bsis(1000));
        usage.conversion(&Conversion {
            format: "csv",
            ..bsis(10)
        });
        usage.failure(Failure::from_status(422));
        usage.failure(Failure::from_status(503));
        usage.failure(Failure::from_status(499));
        usage.failure(Failure::from_status(500));
        usage.page_view();
        usage.inspection();
        usage.sample();

        let s = usage.snapshot();
        assert_eq!(s.totals.conversions, 2);
        assert_eq!(s.totals.records, 1010);
        assert_eq!(s.totals.shards, 4);
        assert_eq!(s.totals.bytes_in, 1010 * 126);
        assert_eq!(
            (
                s.totals.failed_input,
                s.totals.busy,
                s.totals.cancelled,
                s.totals.failed_server
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(
            (s.totals.page_views, s.totals.inspections, s.totals.samples),
            (1, 1, 1)
        );
        assert_eq!(
            s.formats["parquet"],
            Counts {
                conversions: 1,
                records: 1000
            }
        );
        assert_eq!(s.formats["csv"].records, 10);
        assert_eq!(s.inputs["archive"].conversions, 2);
        assert_eq!(s.clients["page"].conversions, 2);
        assert_eq!(s.tables["BSIS"].records, 1010);
        let day = &s.days[&today()];
        assert_eq!(
            (day.conversions, day.records, day.failures, day.page_views),
            (2, 1010, 3, 1)
        );
        assert_eq!(s.largest.records, 1000);
        assert_eq!(s.peak_records_per_second, 500.0);
    }

    #[test]
    fn keeps_only_sap_like_table_names_and_caps_them() {
        let mut tables = BTreeMap::new();
        assert_eq!(table_key(&tables, " bkpf "), "BKPF");
        assert_eq!(table_key(&tables, "/BIC/AZSALES00"), "/BIC/AZSALES00");
        assert_eq!(table_key(&tables, "my ledger.xlsx"), OTHER);
        assert_eq!(table_key(&tables, ""), OTHER);
        assert_eq!(table_key(&tables, &"A".repeat(31)), OTHER);
        assert_eq!(table_key(&tables, "Ülk"), OTHER);
        for i in 0..MAX_TABLES {
            tables.insert(format!("T{i}"), Counts::default());
        }
        assert_eq!(table_key(&tables, "T5"), "T5");
        assert_eq!(table_key(&tables, "BSEG"), OTHER);
    }

    #[test]
    fn keeps_a_window_of_days() {
        let mut days = BTreeMap::new();
        for i in 0..DAYS_KEPT + 5 {
            days.insert(
                format!("2026-{:02}-{:02}", 1 + i / 28, 1 + i % 28),
                Day::default(),
            );
        }
        prune_days(&mut days);
        assert_eq!(days.len(), DAYS_KEPT);
        assert!(!days.contains_key("2026-01-01"));
    }

    #[test]
    fn saves_and_loads_the_same_totals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let (usage, warning) = Usage::open(&path);
        assert!(warning.is_none() && usage.is_saved());
        assert!(!usage.save().unwrap(), "nothing to save yet");
        assert!(!path.exists());
        usage.conversion(&bsis(42));
        assert!(usage.save().unwrap());
        assert!(!usage.save().unwrap(), "saved already");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["usage.json"], "no temporary file is left behind");

        let (reloaded, warning) = Usage::open(&path);
        assert!(warning.is_none());
        assert_eq!(reloaded.snapshot(), usage.snapshot());
    }

    #[test]
    fn never_overwrites_a_file_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        std::fs::write(&path, "not json").unwrap();
        let (usage, warning) = Usage::open(&path);
        assert!(warning.unwrap().contains("leaving the file alone"));
        assert!(!usage.is_saved());
        usage.conversion(&bsis(1));
        assert!(!usage.save().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn exposes_prometheus_metrics() {
        let usage = Usage::in_memory();
        usage.conversion(&bsis(7));
        let text = usage.prometheus(3);
        assert!(text.contains("# TYPE sapbin_records_total counter\nsapbin_records_total 7\n"));
        assert!(text.contains("sapbin_conversions_total{result=\"done\"} 1\n"));
        assert!(text.contains("sapbin_table_records_total{table=\"BSIS\"} 7\n"));
        assert!(text.contains("sapbin_running_conversions 3\n"));
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let (name, value) = line.rsplit_once(' ').unwrap();
            assert!(name.starts_with("sapbin_"), "{line}");
            assert!(value.parse::<f64>().is_ok(), "{line}");
        }
        assert_eq!(escape_label("a\"b\\c"), "a\\\"b\\\\c");
    }
}
