# sap-bin

English · [Русский](https://github.com/aerofeev/sap-bin-parser/blob/main/README.ru.md)

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

![Inspecting an export: table, record geometry and the first records](https://raw.githubusercontent.com/aerofeev/sap-bin-parser/main/docs/images/inspect.png)

## Three ways to use it

**On the web.** Open **[tools.eidox.io/sap-bin-parser](https://tools.eidox.io/sap-bin-parser/)**,
drop the export, look at the first records, convert. In a current browser the conversion
runs in the page itself, so the file is never uploaded. It takes the delivered `.zip`, the unzipped folder, or separate `.BIN` files, and
has a schema editor for files that came without one. [More about the page](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/web.md)

**Download the app** for [Windows](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-windows-x64.zip),
[macOS (Apple silicon)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-arm64.tar.gz),
[macOS (Intel)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-x64.tar.gz) or [Linux](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-linux-x64.tar.gz), and
double-click it: the same page, on your own computer, with no size limit. It is also a
command-line tool. [The app and the CLI](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/cli.md)

```bash
sap-bin convert BSIS.QUERY.zip -o bsis.parquet
```

Prefer Python? [`sap-bin.pyz`](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin.pyz) is one file that needs nothing but Python
3.10, and the package is a library for your own pipelines. [Python](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/python.md)

**Run the container** on your own server:

```bash
docker run --rm --read-only -p 127.0.0.1:8080:8080 ghcr.io/aerofeev/sap-bin-parser
```

[Docker and self-hosting](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/docker.md)

## Documentation

| | |
|---|---|
| [The web page](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/web.md) | inputs, the schema editor, options |
| [The app and the CLI](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/cli.md) | downloads, commands and flags |
| [Python](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/python.md) | the single-file script and the library |
| [Docker and self-hosting](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/docker.md) | the image, settings, reverse proxies |
| [HTTP API](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/http-api.md) | converting from scripts, in one request or in chunks |
| [Nothing is stored](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/privacy.md) | what the service keeps, and how that is checked |
| [Performance](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/performance.md) | measured throughput and memory |
| [The export format](https://github.com/aerofeev/sap-bin-parser/blob/main/docs/format.md) | packed decimals, UTF-16, and the one-byte padding trap |

[Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Changelog](CHANGELOG.md) ·
[Why it is built this way](docs/PRODUCT.md)

## Licence

MIT. Made by eidox ai.
