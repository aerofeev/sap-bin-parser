# Contributing

Thanks for helping. Two things matter more than anything else here.

**Never share client data.** Not in issues, not in tests, not in screenshots. Describe an
export with `sap-bin info --fields` and `scripts/compare.py`, which print structure and
counts but no values. Build test fixtures with the synthetic encoders
(`sap_bin_parser.testing` in Python, `sap_bin::sample` in Rust).

**Keep the two implementations in step.** The Python library is the reference; the Rust
engine must produce byte-identical CSV and value-identical Parquet. A change to decoding
rules goes into both, with a case in `tests/test_cross_implementation.py`.

## Layout

| Path | What |
|---|---|
| `src/sap_bin_parser/` | Python library and `sap-bin-py` CLI |
| `rust/src/` | Rust engine, CLI, local app and web service |
| `rust/web/` | The page: plain HTML, CSS and JavaScript, embedded in the binary. No build step, no dependencies |
| `tests/` | Python tests, including the cross-implementation suite |
| `rust/web/tests/` | Browser end-to-end test (Playwright) |
| `scripts/` | `compare.py` for real exports, `prove-stores-nothing.sh` |
| `docs/` | The user documentation linked from the README; keep the navigation line at the top of each page in step |

## Checks

```bash
pip install -e '.[dev]'
ruff check src tests scripts && ruff format --check src tests scripts
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release && pytest               # includes Rust-vs-Python parity
scripts/prove-stores-nothing.sh               # Linux, needs strace
cd rust/web/tests && npm install && npm test  # the page in Chromium
```

## Releasing

Bump the version in `rust/Cargo.toml` and `src/sap_bin_parser/__init__.py`, add a
`CHANGELOG.md` entry, and push a tag `vX.Y.Z`. The release workflow builds the apps for
Windows, macOS and Linux, publishes the Python package to PyPI and the image to GHCR.

One-time setup for the maintainer: add a trusted publisher on PyPI for this repository
(workflow `release.yml`, environment `pypi`), and create the `pypi` environment under
Settings, Environments.
