# sap-bin web service: a single static-ish binary on a distroless base.
#
#   docker build -t sap-bin .
#   docker run --rm --read-only -p 127.0.0.1:8080:8080 sap-bin
#
# The container needs no writable filesystem at all: conversions stream from
# the request to the response, and nothing is written anywhere. The one
# exception is opt-in: with SAPBIN_STATS_FILE=/data/usage.json and a volume
# at /data, the service keeps its usage totals there across restarts.
#
#   docker run --rm --read-only -p 127.0.0.1:8080:8080 -v sap-bin-usage:/data \
#     -e SAPBIN_STATS_FILE=/data/usage.json -e SAPBIN_STATS_TOKEN=... sap-bin

FROM rust:1.94-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY rust ./rust
RUN cargo build --release --locked -p sap-bin
# An empty /data owned by the runtime user, so a volume mounted there is
# writable by it.
RUN mkdir /data

FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="sap-bin" \
      org.opencontainers.image.description="SAP binary exports to CSV, Parquet or JSON Lines. Stores nothing." \
      org.opencontainers.image.vendor="eidox ai" \
      org.opencontainers.image.source="https://github.com/aerofeev/sap-bin-parser" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /src/target/release/sap-bin /usr/local/bin/sap-bin
COPY --from=build --chown=65532:65532 /data /data
USER nonroot:nonroot
EXPOSE 8080
ENV PORT=8080 \
    SAPBIN_MAX_UPLOAD_MB=20000 \
    SAPBIN_MAX_CONCURRENCY=4
ENTRYPOINT ["/usr/local/bin/sap-bin"]
CMD ["serve", "--host", "0.0.0.0"]
