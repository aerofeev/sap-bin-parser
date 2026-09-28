# Changelog

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
- `inspect` from the first and last few megabytes of a file: table, geometry, shard
  count, a record estimate, the first rows, and a record-size probe when they do not line
  up.
- `scripts/compare.py` checks a real export against both implementations without
  printing any of its data.
- A cross-implementation test suite holding Python and Rust to byte-identical CSV and
  value-identical Parquet.

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
