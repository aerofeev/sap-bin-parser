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
| `rust/web/` | The page and the usage dashboard: plain HTML and JavaScript, embedded in the binary, with no build step and no runtime dependencies. The stylesheet `app.css` is compiled from Tailwind CSS in `rust/web/styles/`; after changing the markup or the styles, run `npm install && npm run build:css` in `rust/web` and commit the result |
| `rust/wasm/` | The engine's WebAssembly bindings, for converting in the browser. `scripts/build-wasm.sh` builds them into `rust/web/wasm/`, and `npm run perspective` in `rust/web` copies the Perspective viewer into `rust/web/perspective/`. The binary embeds both when they are there (`rust/build.rs`); without them the page converts on the server and has no viewer |
| `tests/` | Python tests, including the cross-implementation suite |
| `rust/web/tests/` | Browser end-to-end test (Playwright) |
| `scripts/` | `compare.py` for real exports, `prove-stores-nothing.sh` |
| `docs/`, `docs/ru/` | The user documentation, in English and Russian, linked from `README.md` and `README.ru.md`. A change to one language goes into the other, and the navigation lines at the top of each page stay in step |

## Checks

```bash
pip install -e '.[dev]'
ruff check src tests scripts && ruff format --check src tests scripts
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release && pytest               # includes Rust-vs-Python parity
scripts/prove-stores-nothing.sh               # Linux, needs strace
cd rust/web && npm install && npm run build:css  # after changing the page's markup or styles
scripts/build-wasm.sh                            # the engine for the browser (needs clang)
cd rust/web && npm run perspective               # the table viewer
cargo clippy -p sap-bin-wasm --target wasm32-unknown-unknown -- -D warnings
cd rust/web/tests && npm install && npm test  # the page in Chromium
```

## Releasing

Bump the version in `rust/Cargo.toml` and `src/sap_bin_parser/__init__.py`, add a
`CHANGELOG.md` entry, and push a tag `vX.Y.Z`. The release workflow builds the apps for
Windows, macOS and Linux and publishes them with the Python wheel and `sap-bin.pyz` as a
GitHub release, the image goes to GHCR, and the Python package to PyPI once that is on.

Publishing to PyPI is off until the maintainer sets it up once: add a trusted publisher on
PyPI for this repository (workflow `release.yml`, environment `pypi`), create the `pypi`
environment under Settings, Environments, and set the repository variable `PYPI_PUBLISH` to
`true` under Settings, Secrets and variables, Actions.
