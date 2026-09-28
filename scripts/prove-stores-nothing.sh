#!/usr/bin/env bash
# Run the web service under strace, put conversions through it, and fail if
# the server opened any file for writing, or created, renamed or deleted one.
#
#   scripts/prove-stores-nothing.sh [path/to/sap-bin]
set -euo pipefail
BIN=${1:-target/release/sap-bin}
PORT=$((20000 + RANDOM % 5000))
WORK=$(mktemp -d)
trap 'pkill -f "^$BIN serve --port $PORT" 2>/dev/null || true; rm -rf "$WORK"' EXIT

strace -f -qq -e trace=openat,open,creat,mkdir,rename,renameat,renameat2,unlink,unlinkat \
  -o "$WORK/trace" "$BIN" serve --port "$PORT" >/dev/null 2>&1 &
PID=$!
for _ in $(seq 50); do curl -fsS "localhost:$PORT/healthz" >/dev/null 2>&1 && break; sleep 0.1; done

curl -fsS "localhost:$PORT/api/sample?records=50000&shards=3" -o "$WORK/sample.zip"
curl -fsS --data-binary @"$WORK/sample.zip" "localhost:$PORT/api/convert?format=parquet" -o /dev/null
curl -fsS -F file=@"$WORK/sample.zip" "localhost:$PORT/api/convert?format=csv&split=true" -o /dev/null
curl -fsS -F head=@"$WORK/sample.zip" "localhost:$PORT/api/inspect" -o /dev/null
# Stop the server itself (not strace), so it shuts down and strace exits.
pkill -INT -f "^$BIN serve --port $PORT" || true
wait "$PID" 2>/dev/null || true

if grep -E 'O_WRONLY|O_RDWR|O_CREAT|creat\(|mkdir|rename|unlink' "$WORK/trace" | grep -vE '"/dev/(null|tty|pts)'; then
  echo "FAIL: the server touched the filesystem for writing (above)" >&2
  exit 1
fi
echo "ok: three conversions and an inspection, and the server wrote nothing to disk"
