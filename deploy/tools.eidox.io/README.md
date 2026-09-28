# Deploying to tools.eidox.io/sap-bin-parser

1. On the server behind `tools.eidox.io`, copy `docker-compose.yml` from this folder and
   run `docker compose up -d`. The service listens on `127.0.0.1:8080` and expects to be
   reached under `/sap-bin-parser/` (`SAPBIN_BASE_PATH`).
2. Add the route to the reverse proxy that serves the domain: `Caddyfile` for Caddy, or
   `nginx.conf` for nginx. Both stream bodies in both directions, so large exports never
   touch the proxy's disk.
3. Check it: `curl -fsS https://tools.eidox.io/sap-bin-parser/healthz`.

## Usage statistics

The service counts, for its operator only, how it is used: conversions, records and bytes,
by output format, input, SAP table and day. Nothing in it identifies a person or a file.

1. Next to `docker-compose.yml`, create the token once, then restart:

   ```bash
   echo "SAPBIN_STATS_TOKEN=$(openssl rand -hex 24)" > .env
   docker compose up -d
   ```

2. Open `https://tools.eidox.io/sap-bin-parser/stats` and paste the token (it is in `.env`).
   The page keeps it for that browser tab only.
3. For Prometheus, scrape `/sap-bin-parser/metrics` with the token:

   ```yaml
   - job_name: sap-bin
     scheme: https
     metrics_path: /sap-bin-parser/metrics
     authorization: { credentials: "<the token>" }
     static_configs: [{ targets: ["tools.eidox.io"] }]
   ```

The totals are saved to the `usage` volume every minute and when the container stops, so
they survive updates. `docker compose down -v` deletes them.

To update, `docker compose pull && docker compose up -d`. Images are published to
`ghcr.io/aerofeev/sap-bin-parser` for amd64 and arm64: `:main` on each merge, `:latest`
and the version (`:0.1.0`) on each release. This instance follows `:main`, so a merge is
live after the next pull. To run releases only, change the image to `:latest` or a
version.

**Behind Cloudflare's proxy** the page works for exports of any size: it uploads in 8 MB
chunks, well under Cloudflare's 100 MB per-request cap. That cap does apply to the
single-request `api/convert` endpoint that curl users call. The service's own limit is
`SAPBIN_MAX_UPLOAD_MB`.

The first image push creates the package on GitHub as private. Make it public once under
the repository's Packages, Package settings, Change visibility, or log the server in with
`docker login ghcr.io` and a token that can read packages.
