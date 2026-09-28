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

For the strongest guarantee, use the app: it runs the same page on your own machine and
your files never leave it.

## Running a public instance

Terminate TLS in front of the service. Keep its container read-only with no capabilities
(`deploy/docker-compose.yml`). A reverse proxy must not buffer request or response bodies
to disk (for nginx: `proxy_request_buffering off; proxy_buffering off;`), or it would
store what the service does not.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on this repository ("Security" tab,
"Report a vulnerability") rather than a public issue. Never attach a real export.
