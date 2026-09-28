# sap-bin web service: a single static-ish binary on a distroless base.
#
#   docker build -t sap-bin .
#   docker run --rm --read-only -p 127.0.0.1:8080:8080 sap-bin
#
# The container needs no writable filesystem at all: conversions stream from
# the request to the response, and nothing is written anywhere.

FROM rust:1.94-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY rust ./rust
RUN cargo build --release --locked -p sap-bin

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/sap-bin /usr/local/bin/sap-bin
USER nonroot:nonroot
EXPOSE 8080
ENV PORT=8080 \
    SAPBIN_MAX_UPLOAD_MB=20000 \
    SAPBIN_MAX_CONCURRENCY=4
ENTRYPOINT ["/usr/local/bin/sap-bin"]
CMD ["serve", "--host", "0.0.0.0"]
