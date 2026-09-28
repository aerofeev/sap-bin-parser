# Changelog

## 0.1.0

The first release: one parser for SAP binary table exports, as an app, a command line, a
web service and a library.

- **`sap-bin`, a Rust engine** that streams exports at several million records a second in
  bounded memory: a streaming zip reader (data descriptors, zip64, CRC checks),
  column-at-a-time decoding, and parallel shards with the output kept in order. It writes
  CSV, TSV, JSON Lines, Parquet and Arrow IPC.
- **The app.** `sap-bin` with no arguments opens the page in your browser, served from
  your own machine. There are downloads for Windows, macOS and Linux.
- **The web service**, `sap-bin serve`, which stores nothing: uploads stream through the
  converter and straight back, with no temporary files and no content in the logs. CI
  checks this by tracing the server's file system calls.
- **Conversion in the browser.** Where the browser allows it, the page converts the export
  itself, with the same engine compiled to WebAssembly, and uploads nothing: about 635,000
  records a second in Chromium, unaffected by a slow or flaky connection. Elsewhere it
  converts on the server, and `?convert=server` chooses that.
- **The page.**
  - It takes the delivered zip, the unzipped folder, or separate `DATA.N.BIN` files, and
    has a schema editor for files that came without one.
  - It describes an export in about a second from its first and last few megabytes, and
    suggests record sizes when the records do not line up.
  - It shows the equivalent CLI and curl command for every conversion.
  - It is styled with Tailwind CSS in the shadcn/ui manner, with Frappe UI's typeface and
    type scale.
- **Over the network:** text downloads go compressed (zstd or gzip). The page uploads in
  8 MiB chunks on separate requests, so proxies never stall.
- **Usage statistics for the operator.**
  - They cover conversions, records and bytes by format, input, client, SAP table and day.
  - They are private, behind a token, as a dashboard, JSON and Prometheus metrics.
  - They can optionally be kept in a single file across restarts.
  - Nothing in them identifies a person or a file.
- **Deployment.** Container images for amd64 and arm64 on `ghcr.io`, a GitLab mirror
  pipeline, a path prefix (`SAPBIN_BASE_PATH`), and files for `tools.eidox.io`: compose,
  Caddy and nginx.
- **The Python library** and `sap-bin-py`, the reference implementation. Each release
  carries it as `sap-bin.pyz`, one file needing only Python. A test suite holds Python and
  Rust to byte-identical CSV and value-identical Parquet.
- **Documentation** in English and Russian.
