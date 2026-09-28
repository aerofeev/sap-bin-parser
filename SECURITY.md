# Security

## What the web service promises

sap-bin converts exports of financial records, so the service is designed to keep
nothing:

- Uploads are read as a stream and converted as they arrive. The result streams back as
  the response. The page sends a file in 8 MiB chunks on separate requests, each answered
  only once the converter has taken it, so memory stays bounded to a few chunks. No file
  is written: not a temporary file, not a cache, not a log of the content. CI proves this on every change by running the server under `strace` and
  failing on any file opened for writing (`scripts/prove-stores-nothing.sh`).
- Access logs contain the method, path, status and duration of each request, and nothing
  else: no query string, no file name, no client address, no content.
- Progress counters (bytes read, records written) are held in memory under a random id
  chosen by the browser, and removed five minutes after the conversion finishes. If a
  conversion fails, the error text (which can quote a few bytes of the failing record) is
  held the same way, readable only with that id. A chunked upload that sends nothing for
  ten minutes is cancelled with an error, never finished as if the file had ended.
- The page loads no third-party resources, and its Content-Security-Policy allows
  connections only to its own origin. Cross-site POSTs are refused. The local app answers
  only to `localhost`, which defeats DNS-rebinding attacks.
- Uploads are limited in size (`SAPBIN_MAX_UPLOAD_MB`) and conversions in number
  (`SAPBIN_MAX_CONCURRENCY`); excess requests get 413 and 503.
- Usage statistics are aggregate totals only (conversions, records and bytes by format,
  input, SAP table name and day), with no address, file name, field or value. They are
  served only with the operator's token, compared in constant time; without a token the
  routes do not exist. With `SAPBIN_STATS_FILE` set, that one file is saved (through a
  temporary file and a rename). It is the only thing the service ever writes, and the
  `strace` proof checks exactly that.

- In a current browser the page converts the export itself, with the engine compiled to
  WebAssembly, and uploads nothing. It then reports the totals (records, shards, format,
  SAP table name, time) to `api/usage` for the statistics; the server checks them for
  sense and counts them under the client `browser`.
- The table viewer (Perspective) needs `blob:` scripts and workers, so it runs in a
  same-origin frame with its own policy (`VIEWER_CSP` in `rust/src/server.rs`). The page
  itself keeps the strict policy, which admits `'wasm-unsafe-eval'` for the engine and
  nothing else new. Both policies allow connections to the service's own origin only.

For the strongest guarantee, use the app: it runs the same page on your own machine and
your files never leave it.

## Running a public instance

Terminate TLS in front of the service. Keep its container read-only with no capabilities
(`deploy/docker-compose.yml`). A reverse proxy must not buffer request or response bodies
to disk (for nginx: `proxy_request_buffering off; proxy_buffering off;`), or it would
store what the service does not.

If you turn on usage statistics, generate the token randomly (`openssl rand -hex 24`), keep
it out of version control (`deploy/tools.eidox.io/` reads it from an `.env` file), and give
the statistics file a volume of its own, so the rest of the container stays read-only.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on this repository ("Security" tab,
"Report a vulnerability") rather than a public issue. Never attach a real export.
