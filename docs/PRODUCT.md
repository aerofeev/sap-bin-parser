# sap-bin as a product

This is the working document behind turning a pet parser into something people find,
trust and use. It records the decisions, not just the plan, so a reader can disagree with
a specific one.

## The one-paragraph pitch

SAP hands you a zip of zips full of `DATA.N.BIN` files: no delimiters, no header, packed
decimals, UTF-16, and a one-byte alignment trap. **sap-bin** turns that into CSV or
Parquet. There is a command line for pipelines and a web page for everyone else. The web
page does the whole conversion inside your browser: nothing is uploaded, nothing is
stored, and you can prove it from the network tab.

## Who it is for

| Person | Situation | What they need |
|---|---|---|
| The finance analyst or auditor | Was sent a `BSIS.QUERY.zip`, has Excel and a browser, cannot install software | Drop the file, see the numbers, get a CSV. No account, no upload. |
| The data engineer | Gets 300 shards a month from SAP into a lake | A CLI or library that streams to Parquet with exact decimals and runs in a container. |
| The consultant or student | Needs to understand the format | A readable explanation and readable source. |

The first person is the one everybody else builds for last, and the one who decides
whether a link gets forwarded inside a company. The web app exists for them.

## Decision: the "web service" is a static page that converts in the browser

The request was a high-security web service that stores nothing. The strongest possible
answer to "where does my data go?" is "nowhere", so the conversion runs client side:

- **Nothing to trust.** The page is static files on GitHub Pages, built from this
  repository by a public workflow. A `Content-Security-Policy` in the page forbids every
  outbound connection (`connect-src 'none'`), so even a bug cannot exfiltrate. The
  network tab stays empty after the page loads.
- **Nothing to run.** No backend means no servers to secure, patch, pay for or scale. A
  hundred users converting 100 million records each cost the project nothing.
- **Nothing to lose.** SAP ledger exports are financial records. Most companies forbid
  uploading them to third-party services, so a server-side converter would be unusable by
  the exact people it is for. A page that provably keeps the data local passes that test.
- **Fast enough.** The browser decodes the same bytes with typed arrays, inflates the zip
  with the native `DecompressionStream`, and streams the result straight to disk with the
  File System Access API. Modern JavaScript engines handle this kind of byte transcoding
  at hundreds of megabytes per second per core, and shards are independent, so they can
  be worked on in parallel Web Workers.

A server-side API is deliberately not offered. Anyone who needs automation has the CLI,
which is the same code path a server would run. Adding an upload endpoint later would
weaken the privacy claim for everyone; if it is ever wanted, it should be a separate,
self-hosted deployment of the CLI, not part of this site.

Consequences accepted with this decision:

- Streaming to disk needs the File System Access API (Chrome and Edge, desktop). Firefox
  and Safari get an in-memory download, which is fine up to roughly a gigabyte of output;
  beyond that the page says so and points at the CLI.
- No analytics. Growth is measured with GitHub stars, clones and referrers, all of which
  GitHub reports without touching users.

## Performance targets and how they are met

Target for the start: one million records per second, low memory, 100 million records
end to end.

| Layer | Approach | Why it is fast |
|---|---|---|
| Zip | Central directory parsed from the tail of the file; each shard is a byte range that is inflated as a stream. Zip64 supported. | Nothing is unpacked to disk; inflate is native code. |
| Record decode (browser) | One tight loop over a `Uint8Array` block writes UTF-8 CSV bytes directly; packed decimals are decoded nibble by nibble into digits. | No intermediate strings, no per-field function calls, no garbage. |
| Record decode (Python) | Blocks are viewed as a NumPy `(records, record_size)` matrix; every field is decoded for the whole block at once. Decimals become Arrow `decimal128` from raw int64 buffers. | Per-block, not per-record, Python overhead. |
| Output | CSV chunks are streamed to the file sink with backpressure; Parquet is written one row group at a time. | Memory is bounded by a few row groups regardless of input size. |

Correctness stays the priority over speed: a wrong record size is still an error, not a
warning. The fast paths are checked against the reference row decoder in tests.

## Convenience: the flow is drop, glance, convert

1. **Drop** the delivered zip (or a loose `.BIN` plus its `DATA.0.TXT`). No account, no
   form, no settings first.
2. **Glance.** Within a second the page shows the table name, the fields with their types
   and offsets, the record size and why it is what it is, the shard count and an estimate
   of the record count, and the first rows in a table. If the schema does not line up, the
   page runs the record-size probe and suggests the size that decodes cleanly.
3. **Convert.** Pick CSV or Parquet, keep the defaults, press one button, choose where to
   save. A progress bar shows records per second and time remaining, and can be cancelled.

Small things that matter:

- A "Load sample export" button generates a synthetic archive in the browser, so a
  visitor with no SAP file can see what the tool does in ten seconds.
- Every conversion shows the equivalent `sap-bin` command and Python snippet, one click
  to copy. The page is the on-ramp to the library.
- The format explanation lives on the page too, short and honest, for the person who
  wants to know why the record is 126 bytes and not 125.
- Works with the keyboard, in dark and light mode, on a phone (for the small file that
  arrived by email).

## Trust and code quality as the marketing

The audience is technical and sceptical, so the credible promotion is the artefact itself:

- The privacy claim is verifiable in three ways: the network tab, the CSP in the page
  source, and the repository the page is built from.
- The web app has no build step and no dependencies. What you read in `web/` is what the
  browser runs. That is unusual and a talking point.
- The Python package is on PyPI with a trusted-publishing workflow, typed, linted, and
  tested on four Python versions with and without the optional extras.
- Cross-implementation tests: the browser decoder and the Python decoder are fed the
  same synthetic export and must produce identical output. Parquet written by the browser
  is read back by pyarrow in CI.
- The README leads with the format notes, which is the content people search for.

## Launch plan

1. Publish `0.2.0` to PyPI, enable GitHub Pages, put the live link at the top of the
   README with a screenshot.
2. One long-form post: "What is inside a SAP binary export, and why record 2 is shifted
   by one byte". The format notes are the article; the tool is the call to action.
   Cross-post to the SAP Community, r/SAP, r/dataengineering, Hacker News (Show HN).
3. Answer the existing questions. There are years of forum threads asking how to read
   these files; a short, helpful reply with the link in each is the highest-yield
   marketing available.
4. Keep the repository welcoming: issue templates, a `CONTRIBUTING.md`, good first issues
   (new field types, another output format, translations of the page).

## Naming

"sap-bin" is what people will type into a search box, so it stays as the package, CLI and
repository name. If a brand is wanted for the page ("Enigma" was floated), use it as a
tagline rather than the name: the word is heavily taken and says nothing to a search
engine, and the page has to be findable by someone who knows only that they have a
`.BIN` from SAP.

## Roadmap after this

- **Parquet in the browser** is included, with a minimal zero-dependency writer (PLAIN
  encoding, gzip pages via the native `CompressionStream`). Snappy or zstd would need a
  WebAssembly codec; add it only if a user asks.
- **A shared Rust core** compiled to both WebAssembly and a Python extension would remove
  the two-implementation upkeep and add another 5 to 10x. It is the obvious next step once
  the format coverage stabilises, and a good showcase project in itself.
- **More SAP field types** (`X`, `I`, `F`, `b`, `s`) as real exports show up with them.
- **A `sap-bin serve` command** that hosts the same web page on localhost, for teams that
  cannot reach GitHub Pages from inside their network.
