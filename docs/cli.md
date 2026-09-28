[Overview](../README.md) · [Web](web.md) · **App & CLI** · [Python](python.md) · [Docker](docker.md) · [HTTP API](http-api.md) · [Privacy](privacy.md) · [Performance](performance.md) · [Export format](format.md)

English · [Русский](ru/cli.md)

# The app and the command line

One program is both. Download it for
[Windows](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-windows-x64.zip),
[macOS (Apple silicon)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-arm64.tar.gz),
[macOS (Intel)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-x64.tar.gz) or
[Linux](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-linux-x64.tar.gz).

## The app

Unpack the download and double-click `sap-bin`. The [web page](web.md) opens in your
browser, served from your own machine: nothing leaves the computer, there is no size
limit, and it uses every core you have. Close the window, or press Ctrl+C, to stop it.

The first time, macOS may refuse an app from the internet: right-click `sap-bin` and choose
*Open*. On Windows, if SmartScreen warns, choose *More info*, then *Run anyway*.

## Commands

```bash
sap-bin info BSIS.QUERY.zip --fields          # what is in this export?
sap-bin head BSIS.QUERY.zip -n 3              # the first three records, decoded
sap-bin convert BSIS.QUERY.zip -o bsis.parquet
sap-bin convert BSIS.QUERY/ -o bsis.csv       # an unzipped export folder
sap-bin convert DATA.1.BIN DATA.2.BIN --schema DATA.0.TXT -o bsis.csv
sap-bin convert BSIS.QUERY.zip -o shards/ --split -f parquet
cat BSIS.QUERY.zip | sap-bin convert - -o - > bsis.csv
sap-bin probe DATA.1.BIN --schema DATA.0.TXT  # when records will not line up
sap-bin bench                                 # measure this machine
```

| Command | |
|---|---|
| `sap-bin` or `sap-bin app` | open the app in your browser |
| `sap-bin serve` | run the web service ([Docker](docker.md) has the settings) |
| `sap-bin info PATH` | schema, record geometry, shard count; `--fields` lists every field with its offset |
| `sap-bin head PATH` | decode and print the first records (`-n`) |
| `sap-bin probe PATH` | rank candidate record sizes for data that will not line up |
| `sap-bin convert PATH… -o OUT` | convert; `PATH` is a `.zip`, a folder, one or more `.BIN`/`.TXT` files, or `-` for stdin |
| `sap-bin bench` | throughput on synthetic data |

## Convert options

| Flag | |
|---|---|
| `-f`, `--format` | `csv` (default), `tsv`, `jsonl`, `parquet` or `arrow` (an Arrow IPC stream, for pandas, Polars or DuckDB) |
| `-o`, `--output` | a file, `-` for stdout, or a directory with `--split` |
| `--schema DATA.0.TXT` | use this schema instead of the export's own (a `DATA.0.zip` works too) |
| `--split` | one output file per shard |
| `--limit N` | stop after N records (per shard with `--split`) |
| `--record-size N` | override the size the schema implies |
| `--float-decimals` | amounts as float64 instead of exact decimals |
| `--on-error skip` | leave values that will not decode empty, count them, and carry on |
| `--delimiter`, `--encoding` | CSV delimiter; `--encoding utf-8-sig` adds the BOM Excel needs for Cyrillic |
| `--compression` | Parquet codec: `zstd` (default), `snappy`, `gzip`, `none` |
| `--text-encoding` | encoding of tab-separated `.TXT` shards (default windows-1251) |
| `--threads N` | worker threads (default: one per CPU) |

`sap-bin <command> --help` lists everything. Exit codes: 0 on success, 1 when a record will
not decode, 2 for a bad schema, archive or argument.
