#!/usr/bin/env bash
# Build the engine for the browser: rust/web/wasm/sap_bin_wasm.js and
# sap_bin_wasm_bg.wasm, which the binary embeds when they exist. Without
# them the page converts on the server, as before.
#
#   scripts/build-wasm.sh
#
# Needs the wasm32-unknown-unknown target (rustup target add
# wasm32-unknown-unknown) and clang, for the zstd library Parquet uses.
# wasm-bindgen must match the version in Cargo.lock; when it is not on the
# PATH, its release binary is fetched into target/tools/. wasm-opt from
# binaryen, if installed, makes the module smaller and faster.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=rust/web/wasm

want=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print }' Cargo.lock)
bindgen=wasm-bindgen
if [[ "$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }')" != "$want" ]]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=x86_64-unknown-linux-musl ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Darwin-arm64) target=aarch64-apple-darwin ;;
    *)
      echo "wasm-bindgen $want is needed: cargo install wasm-bindgen-cli --version $want --locked" >&2
      exit 1
      ;;
  esac
  dir=target/tools/wasm-bindgen-$want-$target
  if [[ ! -x "$dir/wasm-bindgen" ]]; then
    echo "Fetching wasm-bindgen $want for $target"
    mkdir -p target/tools
    curl -fsSL "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/$want/wasm-bindgen-$want-$target.tar.gz" |
      tar -xz -C target/tools
  fi
  bindgen=$dir/wasm-bindgen
fi

cargo build --release --locked -p sap-bin-wasm --target wasm32-unknown-unknown
rm -rf "$OUT"
"$bindgen" --target web --no-typescript --out-dir "$OUT" \
  target/wasm32-unknown-unknown/release/sap_bin_wasm.wasm
if command -v wasm-opt >/dev/null; then
  wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int \
    "$OUT/sap_bin_wasm_bg.wasm" -o "$OUT/sap_bin_wasm_bg.wasm"
fi
ls -l "$OUT"
