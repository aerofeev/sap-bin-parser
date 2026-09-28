[Overview](../README.md) · [Web](web.md) · [App & CLI](cli.md) · [Python](python.md) · [Docker](docker.md) · [HTTP API](http-api.md) · **Privacy** · [Performance](performance.md) · [Export format](format.md)

English · [Русский](ru/privacy.md)

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

## Usage statistics

The service keeps running totals for its operator:
- how many conversions there were, and how they ended;
- the records, shards and bytes converted;
- the output formats, whether an export arrived as one file or several, whether it came
  from the page or the API, and the SAP table name;
- the same totals per day, for the last 90 days, plus page loads;
- the fastest and the largest conversion.

That is all. No IP address, user agent, file name, field name or value is in it, and
nothing ties a number to a person. A table name is counted only if it looks like one
(`BSIS`, `/BIC/AZSALES00`); anything else counts as `(other)`, so text typed into a schema
cannot end up there.

The statistics are private. They are served only with the operator's token
(`SAPBIN_STATS_TOKEN`); without a token, the statistics routes do not exist. They are
written to disk only if the operator names a file (`SAPBIN_STATS_FILE`), and that file is
the only thing the service ever writes. The app on your own computer writes nothing and
serves no statistics.

## Checked, not just claimed

- CI runs the server under `strace` and fails if it opens any file for writing during
  conversions: [`scripts/prove-stores-nothing.sh`](../scripts/prove-stores-nothing.sh).
  It runs a second time with statistics saved to a file. That file must be the only one
  written, and it must hold totals and nothing else.
- The Docker image is tested with a read-only root file system.
- The source is open. The page's scripts have no build step: what is in `rust/web/` is
  exactly what your browser runs. Only the stylesheet is compiled, from Tailwind CSS in
  `rust/web/styles/`, and CI checks that the committed result matches its source.

## The strongest guarantee

The [app](cli.md) runs the same page on your own computer. Your file never leaves it. It
answers only to `localhost`, which also shuts out DNS-rebinding attacks from other sites.

[SECURITY.md](../SECURITY.md) covers running a public instance and reporting a
vulnerability.
