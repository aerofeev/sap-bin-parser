#!/usr/bin/env bash
# Run the web service under strace, put conversions through it, and fail if
# the server opened any file for writing, or created, renamed or deleted one.
#
# Twice: as shipped, when it must write nothing at all; and with usage
# statistics saved to a file, when that file (through its .tmp twin) is the
# only thing it may write, and must hold totals and nothing else.
#
#   scripts/prove-stores-nothing.sh [path/to/sap-bin]
set -euo pipefail
BIN=${1:-target/release/sap-bin}
WORK=$(mktemp -d)
PORT=0
trap 'pkill -f "^$BIN serve --port $PORT" 2>/dev/null || true; rm -rf "$WORK"' EXIT

# Start the server under strace with extra arguments, run the conversions,
# stop it, and leave the trace in $WORK/trace.
exercise() {
  PORT=$((20000 + RANDOM % 5000))
  strace -f -qq -e trace=openat,open,creat,mkdir,rename,renameat,renameat2,unlink,unlinkat \
    -o "$WORK/trace" "$BIN" serve --port "$PORT" "$@" >/dev/null 2>&1 &
  local pid=$!
  for _ in $(seq 50); do curl -fsS "localhost:$PORT/healthz" >/dev/null 2>&1 && break; sleep 0.1; done

  curl -fsS "localhost:$PORT/" -o /dev/null
  curl -fsS "localhost:$PORT/api/sample?records=50000&shards=3" -o "$WORK/sample.zip"
  curl -fsS --data-binary @"$WORK/sample.zip" "localhost:$PORT/api/convert?format=parquet" -o /dev/null
  curl -fsS -F file=@"$WORK/sample.zip" "localhost:$PORT/api/convert?format=csv&split=true" -o /dev/null
  curl -fsS -F head=@"$WORK/sample.zip" "localhost:$PORT/api/inspect" -o /dev/null
  # The page's protocol: create a job, stream the download, upload in chunks.
  local job="proof$RANDOM$RANDOM"
  curl -fsS -X POST "localhost:$PORT/api/jobs?job=$job&format=parquet" -o /dev/null
  curl -fsS "localhost:$PORT/api/jobs/$job/download" -o /dev/null & local download=$!
  curl -fsS -X POST --data-binary @"$WORK/sample.zip" "localhost:$PORT/api/jobs/$job/input" -o /dev/null
  curl -fsS -X POST "localhost:$PORT/api/jobs/$job/input?end=true" -o /dev/null
  wait "$download"
  if [[ -n "${TOKEN:-}" ]]; then
    sleep 0.2 # the last outcome is recorded just after its download ends
    curl -fsS -H "Authorization: Bearer $TOKEN" "localhost:$PORT/api/stats" -o "$WORK/live.json"
  fi
  # Stop the server itself (not strace), so it shuts down and strace exits.
  pkill -INT -f "^$BIN serve --port $PORT" || true
  wait "$pid" 2>/dev/null || true
}

writes() {
  grep -E 'O_WRONLY|O_RDWR|O_CREAT|creat\(|mkdir|rename|unlink' "$WORK/trace" | grep -vE '"/dev/(null|tty|pts)' || true
}

exercise
if [[ -n "$(writes)" ]]; then
  writes
  echo "FAIL: the server touched the filesystem for writing (above)" >&2
  exit 1
fi
echo "ok: three conversions and an inspection, and the server wrote nothing to disk"

mkdir "$WORK/stats"
STATS="$WORK/stats/usage.json"
TOKEN=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
exercise --stats-file "$STATS" --stats-token "$TOKEN"
if writes | grep -vF -e "\"$STATS\"" -e "\"$STATS.tmp\""; then
  echo "FAIL: with statistics on, the server wrote something besides $STATS (above)" >&2
  exit 1
fi
python3 - "$STATS" "$WORK/live.json" <<'EOF'
import json, sys
saved, live = (json.load(open(p)) for p in sys.argv[1:])
assert saved["totals"]["conversions"] == 3, saved["totals"]
assert saved["totals"]["records"] == 3 * 150_000, saved["totals"]
assert saved["totals"] == live["totals"], "the file holds what the service reported"
assert set(saved["tables"]) == {"BSIS"}, saved["tables"]
allowed = {"version", "since", "totals", "formats", "inputs", "clients", "tables",
           "days", "largest", "peak_records_per_second"}
assert set(saved) == allowed, set(saved) ^ allowed
EOF
echo "ok: with statistics on, the only file written was $(basename "$STATS"), and it holds totals only"
