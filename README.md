# sap-bin

**Convert SAP binary table exports (`.BIN`) to CSV, Parquet or JSON Lines.** Fast enough
for a hundred million records, simple enough to drop a file on a web page, and built so
that nothing you convert is ever stored.

[![CI](https://github.com/aerofeev/sap-bin-parser/actions/workflows/ci.yml/badge.svg)](https://github.com/aerofeev/sap-bin-parser/actions/workflows/ci.yml)
[![MIT licence](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)

When you download a table or query result out of SAP as a binary export, you get a zip of
per-shard zips holding `DATA.N.BIN` files. There is no delimiter, no header row, and no
obvious record boundary: amounts are packed decimal (COMP-3) and text is UTF-16
big-endian. Opening one in a text editor shows interleaved null bytes and nothing useful.

This reads them.

## Use it

Three ways, all the same converter and the same page.

### 1. On the web

**[tools.eidox.io/sap-bin-parser](https://tools.eidox.io/sap-bin-parser/)**: drop an
export, look at its first records, pick a format, convert. The service stores nothing (see
[below](#nothing-is-stored)).

![Inspecting an export: table, record geometry and the first records](docs/images/inspect.png)

The page takes an export in any of the forms it turns up in:

- **the delivered `.zip`**, which carries its own schema (`DATA.0.TXT`);
- **the unzipped export folder**, or **separate `DATA.N.BIN` files** with their
  `DATA.0.TXT`, converted together as one export;
- **a `.BIN` with no schema at all**: type the fields in, paste them from SE11 or a
  spreadsheet, or fix a sidecar that is wrong. The edited schema can be downloaded as a
  `DATA.0.TXT` for next time.

If the records do not line up with the schema, the page says so, shows which record sizes
decode cleanly, and applies the right one with a click.

### 2. Download it

**The app**, for
[Windows](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-windows-x64.zip),
[macOS (Apple silicon)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-arm64.tar.gz),
[macOS (Intel)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-x64.tar.gz) or
[Linux](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-linux-x64.tar.gz).
Unpack it and double-click `sap-bin`: the same page opens in your browser, served from your
own machine. Nothing leaves the computer and there is no size limit. It is also a
command-line tool:

```bash
sap-bin info BSIS.QUERY.zip --fields          # what is in this export?
sap-bin head BSIS.QUERY.zip -n 3              # the first three records, decoded
sap-bin convert BSIS.QUERY.zip -o bsis.parquet
sap-bin convert BSIS.QUERY/ -o bsis.csv       # an unzipped export folder
sap-bin convert DATA.1.BIN DATA.2.BIN --schema DATA.0.TXT -o bsis.csv
sap-bin convert BSIS.QUERY.zip -o shards/ --split -f parquet
cat BSIS.QUERY.zip | sap-bin convert - -o - > bsis.csv
sap-bin probe DATA.1.BIN --schema DATA.0.TXT  # when records will not line up
```

`-f` picks `csv`, `tsv`, `jsonl` or `parquet`. `--encoding utf-8-sig` writes the BOM that
makes Excel read Cyrillic correctly. `sap-bin <command> --help` has the rest.

<a id="python"></a>**The Python script.**
[`sap-bin.pyz`](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin.pyz)
is the whole Python implementation in one 60 KB file. It needs Python 3.10 or later and
nothing else:

```bash
python sap-bin.pyz convert BSIS.QUERY.zip -o bsis.csv
python sap-bin.pyz info BSIS.QUERY.zip --fields
```

For Parquet, `pip install pyarrow` first. To use it as a library in your own code:

```bash
pip install 'sap-bin-parser[parquet] @ git+https://github.com/aerofeev/sap-bin-parser'
```

```python
from sap_bin_parser import SapArchive, BinReader, write_parquet

with SapArchive("BSIS.QUERY.zip") as archive:
    schema = archive.schema()
    for shard, stream in archive.iter_shard_streams():
        write_parquet(BinReader(stream, schema), f"{shard.name}.parquet", schema)
```

A loose `.BIN` with its sidecar, or a schema declared in code:

```python
from sap_bin_parser import BinReader, load_schema, schema_from_tuples

schema = load_schema("DATA.0.TXT")
for row in BinReader("DATA.1.BIN", schema):
    print(row["BELNR"], row["DMBTR"])

schema = schema_from_tuples([("BUKRS", "C", 4, 0, 8), ("DMBTR", "P", 7, 2, 7)])
```

Installed with pip, the command is `sap-bin-py`. Amounts decode to `Decimal` and reach
Parquet as `decimal128`, so a value SAP wrote as `0.07` stays `0.07`. The Python version is
the reference implementation; the Rust app is 15 to 25 times faster and produces identical
output.

<a id="docker"></a>
### 3. Run the container

Images for amd64 and arm64 are on GitHub's container registry:

```bash
docker run --rm --read-only -p 127.0.0.1:8080:8080 ghcr.io/aerofeev/sap-bin-parser
```

Then open <http://localhost:8080/>. Tags: `latest` and `0.2.0` for releases, `main` for the
newest merge. The container needs no writable file system.

| Variable | Default | |
|---|---|---|
| `SAPBIN_BASE_PATH` | (none) | serve under a path, e.g. `/sap-bin-parser` |
| `SAPBIN_MAX_UPLOAD_MB` | 20000 | largest export accepted |
| `SAPBIN_MAX_CONCURRENCY` | 4 | conversions at once; more get 503 |
| `PORT` | 8080 | |

[`deploy/tools.eidox.io/`](deploy/tools.eidox.io/) has the compose file and the Caddy and
nginx configuration behind the public instance; [`deploy/`](deploy/) has a Fly.io
configuration. The repository also carries a `.gitlab-ci.yml`: mirror it to GitLab and the
same image is published to that project's GitLab registry.

**Over HTTP.** Besides the page, the service converts in one request, for scripts:

```bash
curl -fsS --data-binary @BSIS.QUERY.zip 'http://localhost:8080/api/convert?format=parquet' -o bsis.parquet
curl -fsS -F schema=@DATA.0.TXT -F file=@DATA.1.BIN -F file=@DATA.2.BIN \
  'http://localhost:8080/api/convert?multi=true&format=csv' -o bsis.csv
```

Query parameters mirror the command-line flags: `format`, `split`, `limit`,
`record_size`, `decimals=float`, `on_error=skip`, `delimiter`, `bom`, `compression`. One
request sends and receives at the same time, which curl does but many proxies do not, so
for large exports through a proxy use the page (which uploads in 8 MB chunks on separate
requests) or the app.

## Nothing is stored

The service is built so that storing your data is not something it can do by accident:

- An upload is read as a stream and converted as it arrives; the result streams straight
  back as a download. The page uploads in 8 MB chunks, and each is accepted only once the
  converter has taken it, so the server never holds more than a few chunks. There is no
  temporary file, no database and no cache.
- Logs record the method, path, status and duration of a request. Never a query string,
  a file name or any content.
- To show progress, the server keeps a few counters under a random id the browser chose,
  and drops them five minutes after the conversion ends. An upload that goes quiet for ten
  minutes is cancelled.
- The page loads nothing from any other site, and its Content-Security-Policy forbids it
  from contacting one.
- A failed conversion aborts the download, so a truncated file never looks complete.

This is checked, not just claimed. CI runs the server under `strace` and fails if it opens
any file for writing during conversions
([`scripts/prove-stores-nothing.sh`](scripts/prove-stores-nothing.sh)), and the Docker
image is tested with a read-only root file system. The strongest guarantee is the app,
which never sends your file anywhere.

## Performance

Measured with `sap-bin bench` on a 4-vCPU Intel Xeon at 2.1 GHz, on the 126-byte BSIS
layout:

| | records/s |
|---|---:|
| Loose `.BIN` to CSV, 1 thread | 2.8 million |
| Loose `.BIN` to CSV, 4 threads | 7.8 million |
| Loose `.BIN` to Parquet (zstd), 4 threads | 3.5 million |
| Zipped archive to CSV, 4 threads | 3.6 million |

Streaming 10 million records (1.26 GB) through stdin took 1.3 s to CSV and 2.9 s to
Parquet. Uploading the same 1.26 GB to the web service and downloading Parquet took 2.8 s.
Peak memory stayed between 118 and 166 MB throughout, and does not grow with the size of
the input: shards are decoded in parallel, but a bounded pipeline holds only a few of them
at a time. The Python library converts the same data about 15 to 25 times slower.

Run `sap-bin bench` to measure your own machine.

## The export format

Worked out against real exports; this is the part that is genuinely fiddly.

**The export describes itself.** Every archive ships a `DATA.0.TXT` sidecar giving each
field's name, type, length and byte size:

```
NAME     TABLE  TYPE  LENG  DEC  SIZE  ROLL     KEY
BUKRS           C     4     0    8     BUKRS
HKONT           C     10    0    20    HKONT
DMBTR           P     7     2    7     DMBTR
```

**Five field types, two encodings.** `C` (character), `N` (numeric text), `D` (date,
`YYYYMMDD`) and `T` (time, `HHMMSS`) are all UTF-16BE, two bytes per character, so
`SIZE` is always `LENG * 2`. `P` is packed decimal, where `SIZE` is the raw byte count.

**Packed decimal.** Each byte holds two digits, except the last, which holds one digit
plus a sign nibble: `0xB`/`0xD` negative, `0xA`/`0xC`/`0xE` positive, `0xF` unsigned.
`DEC` gives the implied decimal places; the digits carry no point. A field of all zero
bytes is SAP's null, not a valid COMP-3 value, and is decoded as zero.

**Records are padded to an even boundary.** This is the one that costs people an
afternoon. The BSIS layout above sums to **125** bytes, but records sit at **126**: SAP
pads odd-width records by one byte so each record starts on a two-byte boundary and its
UTF-16 fields stay aligned. Get this wrong and every record after the first is shifted:
you still get output, it still looks tabular, and the amounts are wrong.

So the rule is `record_size = payload + (payload % 2)`, which both implementations apply.
If an export still does not line up, `sap-bin probe` ranks candidate sizes by how many
records decode cleanly and whether the data divides evenly.

**A wrong record size is an error, not a warning.** Misalignment reliably produces
invalid sign nibbles and undecodable UTF-16, and the parser raises on both rather than
writing plausible-looking garbage. `--on-error skip` leaves the bad values empty, counts
them, and carries on.

**The same table ships in two different formats.** Some exports contain fixed-width
`DATA.N.BIN` shards; others contain tab-separated `DATA.N.TXT` shards with a header row
(windows-1251, and a trailing minus for credits), under an identical sidecar and with
no indication on the outside of the archive. Every command picks the right reader on its
own. `DATA.0.TXT` is always the schema sidecar, while `DATA.1.TXT` and up are data.

**Text padding.** Values are stripped of NUL and ASCII whitespace only. A non-breaking
space is data, and survives.

## Verified against

The format notes were derived from real SAP ECC exports: the ledger tables `BSIS` and
`BSIM`, plus a 61-column custom table exercising `T` time fields and four packed-decimal
scales in one record. A 250,000-record `BSIS` shard decoded end to end in strict mode, and
the resulting Parquet summed to the same exact `Decimal` total as a raw scan of the bytes.

No data is included here. Every test fixture is synthetic, built by the libraries' own
encoders. To check a real export of your own, run

```bash
python scripts/compare.py YOUR.QUERY.zip --parquet
```

which converts it with both implementations in a temporary directory, compares the output
byte for byte, reports timings, and deletes everything. It never prints a field value, so
its output is safe to paste into an issue.

## Development

```
rust/        the engine, CLI, web service and page (web/, embedded in the binary)
src/         the Python library, the reference implementation
tests/       Python tests, including the cross-implementation suite
scripts/     compare.py for real exports; prove-stores-nothing.sh
```

```bash
cargo test && cargo build --release                  # Rust
pip install -e '.[dev]' && pytest                     # Python, incl. Rust-vs-Python parity
cd rust/web/tests && npm install && npm test          # the page, in Chromium
```

The Python and Rust implementations are held to identical output in CI: byte-identical
CSV and value-identical Parquet on synthetic exports built to hit every awkward corner. The
page has no build step and no dependencies; what is in `rust/web/` is what the browser
runs. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT. Made by eidox ai.
