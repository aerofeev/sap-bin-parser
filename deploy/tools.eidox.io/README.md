# Deploying to tools.eidox.io/sap-bin-parser

1. On the server behind `tools.eidox.io`, copy `docker-compose.yml` from this folder and
   run `docker compose up -d`. The service listens on `127.0.0.1:8080` and expects to be
   reached under `/sap-bin-parser/` (`SAPBIN_BASE_PATH`).
2. Add the route to the reverse proxy that serves the domain: `Caddyfile` for Caddy, or
   `nginx.conf` for nginx. Both stream bodies in both directions, so large exports never
   touch the proxy's disk.
3. Check it: `curl -fsS https://tools.eidox.io/sap-bin-parser/healthz`.

To update, `docker compose pull && docker compose up -d`. Images are published to
`ghcr.io/aerofeev/sap-bin-parser` for amd64 and arm64: `:latest` and `:0.2.0` on each
release, `:main` on each merge.

**Behind Cloudflare's proxy** the page works for exports of any size: it uploads in 8 MB
chunks, well under Cloudflare's 100 MB per-request cap. That cap does apply to the
single-request `api/convert` endpoint that curl users call. The service's own limit is
`SAPBIN_MAX_UPLOAD_MB`.

The first image push creates the package on GitHub as private. Make it public once under
the repository's Packages, Package settings, Change visibility, or log the server in with
`docker login ghcr.io` and a token that can read packages.
