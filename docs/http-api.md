[Overview](../README.md) · [Web](web.md) · [App & CLI](cli.md) · [Python](python.md) · [Docker](docker.md) · **HTTP API** · [Privacy](privacy.md) · [Performance](performance.md) · [Export format](format.md)

English · [Русский](ru/http-api.md)

# HTTP API

The web service's own page uses these endpoints, and scripts can too. Paths are relative
to the service root, e.g. `https://tools.eidox.io/sap-bin-parser/`.

## Convert in one request

```bash
curl -fsS --data-binary @BSIS.QUERY.zip 'http://localhost:8080/api/convert?format=parquet' -o bsis.parquet

curl -fsS -F schema=@DATA.0.TXT -F file=@DATA.1.BIN -F file=@DATA.2.BIN \
  'http://localhost:8080/api/convert?multi=true&format=csv' -o bsis.csv
```

The body is the export itself, or a multipart form with an optional `schema` part followed
by one `file` part (several with `multi=true`). The response is the converted file. Errors
found before any output (a bad schema, a wrong record size) come back as JSON with status
400, 413 or 422. A failure after output has started aborts the response, so a truncated
file never looks complete.

One request sends and receives at the same time. curl does that, but browsers and many
proxies do not, and a large conversion through them stalls. Through a proxy, use the
chunked jobs below.

## Convert as a job, in chunks

This is what the page does. Neither side has to send and receive on one request, and no
request is larger than 8 MB, which also keeps under proxy upload caps.

```bash
base=http://localhost:8080 id=my-job-0001
curl -fsS -X POST --data-binary @DATA.0.TXT "$base/api/jobs?job=$id&format=csv"  # body: schema, or empty
curl -fsS "$base/api/jobs/$id/download" -o bsis.csv &                             # the output
split -b 8m BSIS.QUERY.zip part.                                                  # the input, in order
for p in part.*; do curl -fsS -X POST --data-binary @"$p" "$base/api/jobs/$id/input"; done
curl -fsS -X POST "$base/api/jobs/$id/input?end=true"
wait
```

Each `input` request is answered once the converter has taken the chunk, so an upload goes
no faster than the conversion and memory stays bounded. For a multi-file job
(`multi=true`), the first chunk of each file carries `?start=true&name=DATA.1.BIN`. A job
whose upload goes quiet for ten minutes is cancelled.

## Endpoints

| | |
|---|---|
| `POST api/convert` | convert in one request |
| `POST api/jobs?job=ID` | create a chunked job (the ID is yours: 8 to 64 letters, digits or `-`) |
| `POST api/jobs/{id}/input` | the next chunk; `start`, `name`, `end` as above |
| `GET api/jobs/{id}/download` | the output, streamed |
| `GET api/jobs/{id}` | progress: state, bytes read, records written, shards done |
| `POST api/jobs/{id}/cancel` | stop it |
| `POST api/inspect` | describe an export from a multipart `head` (its first bytes), and optionally `tail`, `size`, `schema`, `record_size` |
| `GET api/sample` | a synthetic export (`records`, `shards`) |
| `GET api/config`, `GET healthz` | version and limits |

## Parameters

`api/convert` and `api/jobs` take the same query parameters, mirroring the
[command-line flags](cli.md#convert-options):

| | |
|---|---|
| `format` | `csv`, `tsv`, `jsonl`, `parquet` |
| `split` | `true`: a zip with one file per shard |
| `limit` | stop after this many records |
| `record_size` | override the schema's |
| `decimals` | `float` for float64 amounts |
| `on_error` | `skip` to leave undecodable values empty |
| `delimiter`, `bom` | CSV delimiter; `bom=true` for Excel |
| `compression` | Parquet codec |
| `text_encoding` | encoding of `.TXT` shards |
| `multi` | `true`: each uploaded file is one member of the export |
| `name` | the export's name, for naming the output |
| `job` | a progress id; required for `api/jobs` |
