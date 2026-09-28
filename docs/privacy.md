[Overview](../README.md) · [Web](web.md) · [App & CLI](cli.md) · [Python](python.md) · [Docker](docker.md) · [HTTP API](http-api.md) · **Privacy** · [Performance](performance.md) · [Export format](format.md)

# Nothing is stored

The web service is built so that storing your data is not something it can do by
accident.

- An upload is read as a stream and converted as it arrives; the result streams straight
  back as a download. The page uploads in 8 MB chunks, each accepted only once the
  converter has taken it, so the server never holds more than a few chunks. There is no
  temporary file, no database and no cache.
- Logs record the method, path, status and duration of a request. Never a query string,
  a file name, an address or any content.
- To show progress, the server keeps a few counters under a random id your browser chose,
  and drops them five minutes after the conversion ends. An upload that goes quiet for ten
  minutes is cancelled.
- The page loads nothing from any other site, and its Content-Security-Policy forbids it
  from contacting one. Cross-site requests are refused.
- A failed conversion aborts the download, so a truncated file never looks complete.

## Checked, not just claimed

- CI runs the server under `strace` and fails if it opens any file for writing during
  conversions: [`scripts/prove-stores-nothing.sh`](../scripts/prove-stores-nothing.sh).
- The Docker image is tested with a read-only root file system.
- The source is open, and the page has no build step: what is in `rust/web/` is exactly
  what your browser runs.

## The strongest guarantee

The [app](cli.md) runs the same page on your own computer. Your file never leaves it. It
answers only to `localhost`, which also shuts out DNS-rebinding attacks from other sites.

[SECURITY.md](../SECURITY.md) covers running a public instance and reporting a
vulnerability.
