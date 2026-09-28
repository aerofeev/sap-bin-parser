# sap-bin as a product

The working document behind turning a pet parser into something people find, trust and
use. It records decisions and the reasons for them, so a reader can disagree with a
specific one.

## The pitch

SAP hands you a zip of zips full of `DATA.N.BIN` files: no delimiters, no header, packed
decimals, UTF-16, and a one-byte alignment trap. **sap-bin** turns that into CSV, Parquet
or JSON Lines. Double-click the app, drop the file, get a spreadsheet. Nothing is stored,
and you can check that yourself.

## Who it is for

| Person | Situation | What they need |
|---|---|---|
| Finance analyst, auditor | Sent a `BSIS.QUERY.zip`; has Excel and a browser | Drop the file, see the numbers, get a file Excel opens correctly. No account, no upload to strangers. |
| Data engineer | Hundreds of shards a month into a lake | A CLI or container that streams to Parquet with exact decimals, fast, in bounded memory. |
| Consultant, student | Needs to understand the format | A clear explanation and readable source. |

The first person decides whether a link gets forwarded inside a company, so the default
experience is built for them.

## Decisions

### One engine, three front doors

A single Rust binary is the command line, the local app (`sap-bin` with no arguments)
and the web service (`sap-bin serve`). They share every option and every line of
decoding code, so what works in one works in all.

Rust was chosen over the Python original for three reasons: a single self-contained
file per operating system is the easiest thing to hand a non-technical user; decoding
untrusted uploads in a memory-safe language is the right default for a public service;
and it is fast enough that throughput is never the question. The Python package remains
as the reference implementation and a library for Python pipelines.

### The web service stores nothing, by construction

The owner asked for a server-side service. It is built so that storing data is not
something it can do by accident: uploads are parsed as a stream (the zip reader walks
local headers front to back, never seeking), decoded in bounded memory, and streamed back
as the response. There is no temporary file anywhere in the code path, logs never see
names or contents, and the only per-request state is a set of counters.

Claims about privacy are cheap, so this one is tested: CI runs the server under `strace`
and fails on any file opened for writing, and the container runs with a read-only root
file system. The strongest answer, for companies that will not upload ledger data
anywhere, is the local app, which is the same page served from their own machine.

### Convenience: drop, glance, convert

1. **Drop** whatever arrived: the delivered zip, the unzipped folder, separate `.BIN`
   files with their sidecar, or a `.BIN` alone.
2. **Glance.** Within a second, from the first 4 MiB and last 256 KiB of the file only,
   the page shows the table, shard count, record count, record geometry (and why 125
   bytes of fields make a 126-byte record), and the first 20 records. If they do not line
   up, it says so in words, ranks the record sizes that do, and applies one with a click.
3. **Convert.** Pick a format, press the button. The browser's own download manager
   saves the result, so a 20 GB CSV streams to disk in any browser with no memory limit,
   and the page shows records per second and time remaining.

When there is no schema, or the sidecar is wrong, the page has an editor: type fields,
paste them from SE11 or a spreadsheet, and save the result as a `DATA.0.TXT`. Every
conversion also shows the equivalent CLI and curl command, which is how a one-off user
becomes an automated one.

### Speed and resources

Target: a million records a second, low resource use, a hundred million records.
Measured: 2.8 million records a second on one core, 7.8 million on four, and a 1.26 GB
upload converted in under three seconds with the server's memory peaking at 118 MB.
Memory does not grow with input size: shards decode in parallel on a worker pool, each
with its own small bounded channel to an ordered writer, so a fast worker waits rather
than buffering.

### Correctness over plausibility

A wrong record size produces output that looks right and is not. The engine refuses:
invalid sign nibbles and broken UTF-16 are errors, the web service reports them as a
clear error before any download starts, and a failure mid-stream aborts the download so a
truncated file is never mistaken for a complete one. The Python and Rust implementations
are held to byte-identical output in CI on exports built to hit every awkward corner.

## Naming

"sap-bin" is what people type into a search box, so it stays as the package, command and
repository name. If a brand is wanted ("Enigma" was floated), use it as a tagline, not
the name: the word is heavily taken and tells a search engine nothing about SAP exports.

## Launch plan

1. Tag `v0.2.0`: the release workflow publishes the apps, the PyPI package and the image.
2. Put a public instance up (Fly.io configuration in `deploy/`), and its link at the top
   of the README, with the screenshots.
3. One long-form post: "What is inside a SAP binary export, and why record 2 is shifted by
   one byte". The format notes are the article; the app is the call to action.
   Cross-post to the SAP Community, r/SAP, r/dataengineering and Show HN.
4. Answer the existing forum threads asking how to read these files, briefly and
   helpfully, with the link.
5. Keep the repository welcoming: issue templates that protect client data, a
   contributing guide, and good first issues (new field types, more output formats,
   translations of the page).

## Next

- **More SAP field types** (`X`, `I`, `F`, `b`, `s`) as real exports show up with them.
- **Code signing** for the Windows and macOS apps, to remove the first-run warnings.
- **A WebAssembly build** of the engine, so the hosted page could also convert entirely
  in the browser for users who prefer that.
- **Parallel Parquet encoding** across row groups, the one stage still single-threaded.
