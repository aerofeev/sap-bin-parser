[Overview](../README.md) · [Web](web.md) · [App & CLI](cli.md) · [Python](python.md) · **Docker** · [HTTP API](http-api.md) · [Privacy](privacy.md) · [Performance](performance.md) · [Export format](format.md)

English · [Русский](ru/docker.md)

# Docker and self-hosting

The web service is the same program as the [app](cli.md), run with `sap-bin serve`. The
container image is the easiest way to host it.

## Run the image

Images for amd64 and arm64 are on GitHub's container registry:

```bash
docker run --rm --read-only -p 127.0.0.1:8080:8080 ghcr.io/aerofeev/sap-bin-parser
```

Then open <http://localhost:8080/>. The container needs no writable file system.

| Tag | |
|---|---|
| `latest` | the newest release |
| `0.2.0`, `0.2` | a release |
| `main` | the newest merge to `main` |

## Settings

| Variable | Flag | Default | |
|---|---|---|---|
| `SAPBIN_BASE_PATH` | `--base-path` | (none) | serve under a path, e.g. `/sap-bin-parser` |
| `SAPBIN_MAX_UPLOAD_MB` | `--max-upload-mb` | 20000 | the largest export accepted |
| `SAPBIN_MAX_CONCURRENCY` | `--max-concurrency` | 4 | conversions at once; more get 503 with `Retry-After` |
| `SAPBIN_THREADS` | `--threads` | CPUs ÷ concurrency | worker threads per conversion |
| `PORT` | `--port` | 8080 | |
| `SAPBIN_LOG` | | `info` | log level |

Each conversion needs about 150 MB of memory, whatever the size of the export.

## Behind a reverse proxy

[`deploy/tools.eidox.io/`](../deploy/tools.eidox.io/) holds the setup of the public
instance: a hardened compose file, and the matching Caddy and nginx configuration for
serving it under `/sap-bin-parser/`. The proxy must pass request and response bodies
through as they stream; with nginx, `proxy_request_buffering off` and
`proxy_buffering off`, or it spools uploads to disk and stores what the service is built
never to store. [`deploy/`](../deploy/) also has a generic compose file and a Fly.io
configuration.

## Other registries

The repository carries a `.gitlab-ci.yml`: mirror it to GitLab, and the same image is
published to that project's GitLab container registry.

## Build it yourself

```bash
docker build -t sap-bin .
```
