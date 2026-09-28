# Changelog

## Unreleased

### Added

- **Conversion in the browser.** Where the browser allows it, the page converts the export
  itself, with the same engine compiled to WebAssembly in a worker, and uploads nothing.
  The output goes to the browser's private storage and then to your downloads, so memory
  stays flat. Over a network this is many times faster than uploading (about 635,000
  records a second in Chromium), and a flaky connection cannot interrupt it. The page
  falls back to the server where it cannot, and `?convert=server` chooses the server.
- **Explore records** in a [Perspective](https://perspective-dev.github.io) table viewer:
  sort, filter, group, pivot and chart up to millions of records in the page. It is served
  from the binary and runs in a frame of its own.
- **Arrow IPC output** (`-f arrow`, `format=arrow`), for pandas, Polars and DuckDB.
- The engine builds without the server (`default-features = false`), and converts in one
  thread on request (`Options::sequential`).
- `POST api/usage`: the page reports the totals of a browser conversion, so the usage
  statistics count it, under the client `browser`.

## 0.2.0

A product rather than a script: the same parser as an app, a command line, a web
service and a library.

### Added

- **`sap-bin`, a Rust engine** with the Python library's commands and flags, streaming
  exports at several million records a second in bounded memory: a streaming zip reader
  (data descriptors, zip64, CRC checks), column-at-a-time decoding, parallel shards with
  output kept in order. CSV, TSV, JSON Lines and Parquet.
- **The app.** `sap-bin` with no arguments opens a page in your browser, served from your
  own machine: drop an export, see its schema and first records, convert. Downloads for
  Windows, macOS and Linux on each release.
- **The web service**, `sap-bin serve`, which stores nothing: uploads stream through the
  converter and back as a download, with no temporary files and no content in logs.
  Checked in CI by tracing the server's file system calls. Container image on GHCR.
- **More ways in.** Unzipped export folders, separate `DATA.N.BIN` files, a zipped
  `DATA.0.zip` sidecar, and a schema typed or pasted into the page's editor.
- Serves under a path prefix (`SAPBIN_BASE_PATH`), with deployment files for
  `tools.eidox.io/sap-bin-parser`: compose, Caddy and nginx.
- The page converts through a job: the download and 8 MiB upload chunks travel on
  separate requests, so browsers and proxies that cannot send and receive on one request
  at once no longer stall on large exports. This also keeps every request under
  Cloudflare's upload cap.
- Container images for amd64 and arm64 on `ghcr.io`, for every merge and release; a
  `.gitlab-ci.yml` publishes to GitLab's registry from a mirror.
- Each release carries `sap-bin.pyz`, the Python implementation as one file needing only
  Python, and the Python wheel.
- `inspect` from the first and last few megabytes of a file: table, geometry, shard
  count, a record estimate, the first rows, and a record-size probe when they do not line
  up.
- `scripts/compare.py` checks a real export against both implementations without
  printing any of its data.
- A cross-implementation test suite holding Python and Rust to byte-identical CSV and
  value-identical Parquet.
- **Usage statistics for the operator:** conversions, records and bytes by output format,
  input, client, SAP table and day. They are private, behind a token
  (`SAPBIN_STATS_TOKEN`), with a dashboard at `stats`, JSON at `api/stats`, and Prometheus
  metrics at `metrics`. They can be kept across restarts in a file (`SAPBIN_STATS_FILE`),
  which is then the only thing the service writes. Nothing in them identifies a person or
  a file. The no-writes proof checks this too.
- **Compressed downloads:** CSV, TSV and JSON Lines go over the wire as zstd (or gzip)
  when the browser accepts it. Over the internet a conversion is limited by the network,
  and the CSV shrinks four to five times.
- The service stops cleanly on SIGTERM, as sent by `docker stop`, and saves its
  statistics on the way out.
- **The look:** the page is styled with Tailwind CSS in the shadcn/ui manner, with
  Frappe UI's typeface (the same Inter variable font) and type scale, served from the
  binary like everything else. The Content-Security-Policy allows fonts from the page's
  own origin only.

### Changed

- The Python command is now `sap-bin-py`, so it does not shadow the Rust `sap-bin`.
- Text is stripped of NUL and ASCII whitespace only, the same explicit set in both
  implementations. A non-breaking space is now kept as data.
- A null packed decimal carries its field's scale: `0.00`, not `0`, in a two-decimal
  field. A negatively signed zero is zero.
- Dates and times are formatted only when they are ASCII digits.
- Tab-separated shards follow the binary rules for dates (`0000-00-00` is null) and
  round amounts to their field's scale.
- Packed fields wider than 28 digits keep every digit.

## 0.1.0

The Python library and CLI: archives, schema sidecars, both delivery formats, CSV and
Parquet, and the record-size probe.
