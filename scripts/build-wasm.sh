#!/usr/bin/env bash
# Build the engine for the browser: rust/web/wasm/sap_bin_wasm.js and
# sap_bin_wasm_bg.wasm, which the binary embeds when they exist. Without
# them the page converts on the server, as before.
#
#   scripts/build-wasm.sh
#
# Needs the wasm32-unknown-unknown target (rustup target add
# wasm32-unknown-unknown), clang for the zstd library Parquet uses, and
# wasm-bindgen-cli at the version in Cargo.lock. wasm-opt from binaryen, if
# installed, makes the module smaller and faster.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=rust/web/wasm

cargo build --release --locked -p sap-bin-wasm --target wasm32-unknown-unknown
want=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print }' Cargo.lock)
have=$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }' || true)
if [[ "$have" != "$want" ]]; then
  echo "wasm-bindgen $want is needed (found: ${have:-none}):" >&2
  echo "  cargo install wasm-bindgen-cli --version $want --locked" >&2
  exit 1
fi
rm -rf "$OUT"
wasm-bindgen --target web --no-typescript --out-dir "$OUT" \
  target/wasm32-unknown-unknown/release/sap_bin_wasm.wasm
if command -v wasm-opt >/dev/null; then
  wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int \
    "$OUT/sap_bin_wasm_bg.wasm" -o "$OUT/sap_bin_wasm_bg.wasm"
fi
ls -l "$OUT"
