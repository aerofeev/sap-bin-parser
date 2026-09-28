[Overview](../README.md) · [Web](web.md) · [App & CLI](cli.md) · [Python](python.md) · [Docker](docker.md) · [HTTP API](http-api.md) · **Privacy** · [Performance](performance.md) · [Export format](format.md)

English · [Русский](ru/privacy.md)

# Nothing is stored

The web service is built so that storing your data is not something it can do by
accident. In a current browser, your data does not even reach it.

## Converted in your browser

Where the browser allows it, the page converts the export itself, with the same engine
compiled to WebAssembly, and nothing is uploaded. The result is written to the browser's
private storage for this site (which no other site can read), handed to your downloads,
and removed from that storage on your next conversion or visit. Afterwards the page tells
the service the totals, for its [usage statistics](#usage-statistics): the number of
records and shards, the format, whether one file or several, the SAP table name and the
time taken. Never the file, a field name or a value. The browser suite checks, in a real
browser, that no request carries an export to the server when the page converts itself.

## Converted on the server

Browsers that cannot do this, and anyone who adds `?convert=server`, get the server path:

- An upload is read as a stream and converted as it arrives; the result streams straight
  back as a download. The page uploads in 8 MB chunks, each accepted only once the
  converter has taken it, so the server never holds more than a few chunks. There is no
  temporary file, no database and no cache.
- Logs record the method, path, status and duration of a request. Never a query string,
  a file name, an address or any content.
- To show progress, the server keeps a few counters under a random id your browser chose,
  and drops them five minutes after the conversion ends. An upload that goes quiet for ten
  minutes is cancelled.
- A failed conversion aborts the download, so a truncated file never looks complete.

## Either way

- The page loads nothing from any other site, and its Content-Security-Policy forbids it
  from contacting one. Cross-site requests are refused.
- The table viewer ([Perspective](https://perspective-dev.github.io)) is served from the
  same place. It loads parts of itself from `blob:` URLs, which the page's policy forbids,
  so it runs in a frame of its own with a policy that allows them. That frame, too, can
  reach nothing but this site, so Perspective's optional AI assistant and map tiles cannot
  contact anyone. The page, which holds your files, keeps the strict policy.

## Usage statistics

The service keeps running totals for its operator:
- how many conversions there were, and how they ended;
- the records, shards and bytes converted;
- the output formats, whether an export arrived as one file or several, whether it came
  from the page or the API, and the SAP table name;
- the same totals per day, for the last 90 days, plus page loads;
- the fastest and the largest conversion.

Conversions done in the browser are counted from the totals the page reports, under the
client `browser`, and are checked for sense.

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
