[Overview](../README.md) · [Web](web.md) · [App & CLI](cli.md) · [Python](python.md) · [Docker](docker.md) · [HTTP API](http-api.md) · [Privacy](privacy.md) · **Performance** · [Export format](format.md)

English · [Русский](ru/performance.md)

# Performance

Measured with `sap-bin bench` on a 4-vCPU Intel Xeon at 2.1 GHz, on the 126-byte BSIS
layout:

| | records/s |
|---|---:|
| Loose `.BIN` to CSV, 1 thread | 2.8 million |
| Loose `.BIN` to CSV, 4 threads | 7.8 million |
| Loose `.BIN` to Parquet (zstd), 4 threads | 3.5 million |
| Zipped archive to CSV, 4 threads | 3.6 million |

Ten million records (1.26 GB):

| | time | peak memory |
|---|---:|---:|
| through stdin to CSV | 1.3 s | 118 MB |
| through stdin to Parquet | 2.9 s | 166 MB |
| uploaded to the web service, Parquet back | 2.8 s | 118 MB |
| through the page in Chromium, 1 million records to CSV | 2.4 s | |

Memory does not grow with the size of the input. Shards are decoded in parallel on a
worker pool, and each has its own small bounded channel to a writer that keeps the output
in order, so a fast worker waits rather than piling up output.

The Python library converts the same data about 15 to 25 times slower, with identical
output.

Run `sap-bin bench` to measure your own machine.

## In the browser

The page converts in the browser where it can, with the engine compiled to WebAssembly, in
one thread. In Chromium, 1.6 million records convert at about 635,000 records a second to
CSV and 780,000 to Parquet, byte-identical to the native engine. There is no upload and no
download, so the network plays no part.

## Over the internet

A conversion over the network is limited by the network, not by the engine. The CSV
coming back is typically four times the size of the zip going up. So the service
compresses text downloads (zstd, or gzip for browsers without it), which the browser undoes
as it saves. On the synthetic sample, zstd makes the CSV 4.5 times smaller for no
measurable cost: 1 million records in 0.35 s rather than 0.34 s. Real exports, with their
repeated company codes and padded fields, usually shrink more. On a slow link, Parquet is
smaller still. For no network at all, run the [app](cli.md).
