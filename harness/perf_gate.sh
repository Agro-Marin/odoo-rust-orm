#!/usr/bin/env bash

set -uo pipefail

usage() {
  cat <<'USAGE'
usage: harness/perf_gate.sh --db NAME [--rounds N] [--control] [--floors FILE]
                            [--write-floors FILE] [--workload FILE] [--out DIR]

The performance A/B gate. Every other gate here asks whether a routed read is
EXACT; this one asks whether it is worth routing. It replays a workload
through traffic_bench.py in one process with routing off and on, interleaved
A/B/B/A over N pairs, and harness/perf_gate.py judges every routed
(model, method, shape): python time / routed time with a 99% interval for the
median from the pairs themselves. A shape FAILS only when the whole interval
is below 1.0x; a shape the noise cannot place is "inconclusive", not a failure.
The suspects are then re-timed on their own, and a failure that pass does not
reproduce is "unconfirmed": hundreds of shapes at 99% leave a few false
failures on a shared machine, and an independent repeat lets them go.

  --db           the database (mail,contacts is what the gate's probe has)
  --rounds       pairs to time (default 16; 8 is the least a 99% interval
                 needs, and an even count runs each leg first equally often)
  --control      also time python against python under the same protocol:
                 every shape it calls slower is a false failure, so it is the
                 instrument's own error rate on this machine at this load
  --floors       shape -> minimum speedup; a shape whose upper bound falls
                 under its floor is "regressed" and fails
  --write-floors record each faster shape's lower bound as its floor
  --workload     a capture (JSONL) to replay instead of the generated corpus

The default workload is harness/parity_corpus.py on the database: the cases
the byte-parity stage sends, every model, each of the six routed methods.
Exit 0 when no routed shape is measurably slower, 1 when one is, 2 on a
setup error, 3 when nothing could be timed (the shim did not load).
USAGE
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${RUSTORM_WORKSPACE:-$(cd "$ROOT/.." && pwd)}"
ODOO="${RUSTORM_ODOO_ROOT:-$WORKSPACE/odoo}"
PY="${RUSTORM_PYTHON:-$WORKSPACE/p314o19m/bin/python}"
CONF="${RUSTORM_ODOO_CONF:-$WORKSPACE/p314o19m.conf}"

DB=""; ROUNDS=16; CONTROL=0; FLOORS=""; WRITE_FLOORS=""; WORKLOAD=""
OUT="${RUSTORM_PERF_OUT:-$(mktemp -d -t rustorm-perf-XXXXXX)}"
while [ $# -gt 0 ]; do
  case "$1" in
    --db)           DB="$2"; shift 2 ;;
    --rounds)       ROUNDS="$2"; shift 2 ;;
    --control)      CONTROL=1; shift ;;
    --floors)       FLOORS="$2"; shift 2 ;;
    --write-floors) WRITE_FLOORS="$2"; shift 2 ;;
    --workload)     WORKLOAD="$2"; shift 2 ;;
    --out)          OUT="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$DB" ] || { usage >&2; exit 2; }
mkdir -p "$OUT"
echo "perf gate on '$DB', $ROUNDS pairs   (artifacts in $OUT)"
echo "  $(uptime)"

# the freshly built extension when there is one, as verify.sh and parity.sh do
if [ -f "$ROOT/target/release/libengine_py.so" ]; then
  mkdir -p "$OUT/pymod"
  cp "$ROOT/target/release/libengine_py.so" "$OUT/pymod/engine_py.so"
  export PYTHONPATH="$OUT/pymod${PYTHONPATH:+:$PYTHONPATH}"
fi

# Cron threads and background workers commit between legs; a leg that waits on
# one reads as the kernel's cost.
{
  grep -vE '^(addons_path|server_wide_modules|rust_engine_[a-z_]+|max_cron_threads|workers|db_maxconn) *=' "$CONF"
  printf 'addons_path = %s,%s/addons\n' "$(sed -nE 's/^addons_path *= *//p' "$CONF" | tail -1)" "$ROOT"
  swm=$(sed -nE 's/^server_wide_modules *= *//p' "$CONF" | tail -1)
  swm=$(printf '%s' "${swm:-base,web}" | tr ',' '\n' | sed 's/^ *//; s/ *$//' | grep -vx rust_engine | grep -v '^$' | paste -sd, -)
  printf 'server_wide_modules = %s,rust_engine\n' "${swm:-base,web}"
  printf 'rust_engine_db = %s\nrust_engine_mode = on\nrust_engine_verify_sample = 0\n' "$DB"
  printf 'max_cron_threads = 0\nworkers = 0\ndb_maxconn = 8\n'
} > "$OUT/perf.conf"

if [ -z "$WORKLOAD" ]; then
  if ! RUSTORM_PARITY_OUT="$OUT/corpus.json" "$PY" "$ODOO/odoo-bin" shell -c "$CONF" \
       -d "$DB" --no-http --db_maxconn=4 \
       <<< "exec(open('$ROOT/harness/parity_corpus.py').read())" > "$OUT/corpus.log" 2>&1; then
    echo "PERF SETUP FAILED: corpus generation; see $OUT/corpus.log"; exit 2
  fi
  # the context an HTTP client sends, so the replay reads what parity reads
  "$PY" - "$OUT/corpus.json" "$OUT/workload.jsonl" <<'PYEOF'
import json, sys
cases = json.load(open(sys.argv[1]))
with open(sys.argv[2], "w") as fh:
    for case in cases:
        case = dict(case, uid=2, context={"lang": "en_US", "tz": False, "uid": 2})
        fh.write(json.dumps(case) + "\n")
print("  workload: %d corpus cases" % len(cases))
PYEOF
  WORKLOAD="$OUT/workload.jsonl"
fi

bench() {
  local name="$1" control="$2" workload="$3"
  RUSTORM_REPLAY="$workload" RUSTORM_TRAFFIC_ROUNDS="$ROUNDS" \
  RUSTORM_TRAFFIC_JSON="$OUT/$name.timings.json" RUSTORM_TRAFFIC_CONTROL="$control" \
    "$PY" "$ODOO/odoo-bin" shell -c "$OUT/perf.conf" -d "$DB" --no-http --db_maxconn=8 \
      --log-level=warn \
    <<< "import runpy, sys; sys.path.insert(0, '$ROOT/harness'); runpy.run_path('$ROOT/harness/traffic_bench.py', init_globals={'env': env}, run_name='__main__')" \
    > "$OUT/$name.log" 2>&1
  local rc=$?
  grep -aE '^TRAFFIC [0-9]+ (of|pairs)' "$OUT/$name.log" | sed 's/^/  /'
  if [ "$rc" != 0 ]; then
    echo "PERF SETUP FAILED: traffic_bench ($name) exited $rc; see $OUT/$name.log"; exit 2
  fi
  if [ ! -s "$OUT/$name.timings.json" ]; then
    echo "PERF SKIP: $(grep -a '^TRAFFIC SKIP' "$OUT/$name.log" | head -1 | cut -c15-)"; exit 3
  fi
}

judge_args=(--confidence "${RUSTORM_PERF_CONFIDENCE:-0.99}")
[ -n "$FLOORS" ] && judge_args+=(--floors "$FLOORS")

# one leg pair, judged twice: first to name the suspects, then with the
# suspects re-timed on their own
gate() {
  local name="$1" control="$2"; shift 2
  bench "$name" "$control" "$WORKLOAD"
  "$PY" "$ROOT/harness/perf_gate.py" "$OUT/$name.timings.json" "${judge_args[@]}" \
    --suspects "$OUT/$name.suspects.jsonl" > /dev/null
  local confirm=()
  if [ -s "$OUT/$name.suspects.jsonl" ]; then
    echo "  re-timing $(wc -l < "$OUT/$name.suspects.jsonl") suspect calls"
    bench "$name.confirm" "$control" "$OUT/$name.suspects.jsonl"
    confirm=(--confirm "$OUT/$name.confirm.timings.json")
  fi
  "$PY" "$ROOT/harness/perf_gate.py" "$OUT/$name.timings.json" "${judge_args[@]}" \
    "${confirm[@]}" --json "$OUT/$name.json" "$@" | tee "$OUT/$name.txt"
  return "${PIPESTATUS[0]}"
}

if [ "$CONTROL" = 1 ]; then
  gate control 1
fi
if [ -n "$WRITE_FLOORS" ]; then
  gate perf 0 --write-floors "$WRITE_FLOORS"
else
  gate perf 0
fi
rc=$?
echo "  $(uptime)"
exit "$rc"
