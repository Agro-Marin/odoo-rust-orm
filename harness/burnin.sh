#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ODOO="${RUSTPOC_ODOO:-$(cd "$ROOT/../odoo" && pwd)}"
PY="${RUSTPOC_PYTHON:-$(cd "$ROOT/.." && pwd)/p314o19m/bin/python}"
CONF="${RUSTPOC_ODOO_CONF:-$(cd "$ROOT/.." && pwd)/p314o19m.conf}"

DB=""; PASSWORD=""; THREADS=16; SECONDS_=300; WORKERS=4; SAMPLE=0.05; PORT=8073
while [ $# -gt 0 ]; do
  case "$1" in
    --db)       DB="$2"; shift 2 ;;
    --password) PASSWORD="$2"; shift 2 ;;
    --threads)  THREADS="$2"; shift 2 ;;
    --seconds)  SECONDS_="$2"; shift 2 ;;
    --workers)  WORKERS="$2"; shift 2 ;;
    --sample)   SAMPLE="$2"; shift 2 ;;
    --port)     PORT="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$DB" ] && [ -n "$PASSWORD" ] || { echo "--db and --password are required" >&2; exit 2; }
[ -f "$ROOT/target/release/libengine_py.so" ] || {
  echo "no libengine_py.so; cargo build --release" >&2; exit 2; }

OUT="$(mktemp -d /tmp/rustpoc-burnin-XXXXXX)"
echo "burn-in on '$DB'   (artifacts in $OUT)"
mkdir -p "$OUT/pymod"
cp "$ROOT/target/release/libengine_py.so" "$OUT/pymod/engine_py.so"

"$PY" - "$CONF" "$OUT/burnin.conf" "$ROOT/addons" "$PORT" "$WORKERS" <<'PYEOF'
import re, sys
src, dst, addons, port, workers = sys.argv[1:6]
conf = open(src).read()
conf = re.sub(r"(?m)^addons_path\s*=\s*(.*)$", lambda m: "addons_path = %s,%s" % (m.group(1), addons), conf)
for key, value in (("http_port", port), ("workers", workers), ("db_maxconn", "16")):
    if re.search(r"(?m)^%s\s*=" % key, conf):
        conf = re.sub(r"(?m)^%s\s*=.*$" % key, "%s = %s" % (key, value), conf)
    else:
        conf += "\n%s = %s" % (key, value)
open(dst, "w").write(conf)
PYEOF

stop() {
  for p in $(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u); do
    kill -TERM -"$(ps -o pgid= -p "$p" | tr -d ' ')" 2>/dev/null || true
  done
  sleep 4
}
trap stop EXIT

leg() {
  local mode="$1" log="$OUT/$1.log"
  stop
  {
    grep -v '^rust_engine_' "$OUT/burnin.conf"
    echo "server_wide_modules = base,web,rust_engine"
    echo "rust_engine_db = $DB"
    echo "rust_engine_mode = $mode"
    echo "rust_engine_verify_sample = $SAMPLE"
    echo "rust_engine_report_seconds = 20"
  } > "$OUT/leg.conf"
  PYTHONPATH="$OUT/pymod" setsid nohup "$PY" "$ODOO/odoo-bin" -c "$OUT/leg.conf" -d "$DB" \
      > "$log" 2>&1 < /dev/null &
  for _ in $(seq 1 120); do ss -ltn | grep -q ":$PORT " && break; sleep 1; done
  sleep 6

  "$PY" "$ROOT/harness/http_bench.py" --port "$PORT" --db "$DB" --password "$PASSWORD" \
      --threads "$THREADS" --seconds 30 --warmup 5 --label "warm-$mode" > /dev/null 2>&1 || true

  local workers_pids
  workers_pids=$(grep -a "werkzeug: 127.0.0.1" "$log" | awk '{print $3}' | sort -u | awk -v n="$WORKERS" 'NR<=n' | paste -sd' ')
  rss() { local t=0 v; for p in $workers_pids; do
            v=$(awk '/VmRSS/{print $2}' "/proc/$p/status" 2>/dev/null); t=$((t + ${v:-0})); done; echo "$t"; }
  conns() { psql -U "${USER:-marin}" -d "$DB" -tAc \
              "select count(*) from pg_stat_activity where datname='$DB'" 2>/dev/null || echo "?"; }

  local rss_before conns_before
  rss_before=$(rss); conns_before=$(conns)
  local result
  result=$("$PY" "$ROOT/harness/http_bench.py" --port "$PORT" --db "$DB" --password "$PASSWORD" \
      --threads "$THREADS" --seconds "$SECONDS_" --warmup 5 --label "$mode" 2>"$OUT/bench_$mode.err" | tail -1) || true
  local rss_after conns_after
  rss_after=$(rss); conns_after=$(conns)

  local routed=0 verified=0 diff=0 errors=0 reporting=0
  local line field
  for pid in $workers_pids; do
    line=$(grep -a "rust kernel: mode" "$log" | grep " $pid " | tail -1 || true)
    [ -n "$line" ] || continue
    reporting=$((reporting + 1))
    for field in routed verified diff error; do
      local v
      v=$(sed -n "s/.*[ =]$field=\([0-9][0-9]*\).*/\1/p" <<<"$line" | head -1)
      v=${v:-0}
      case "$field" in
        routed)   routed=$((routed + v)) ;;
        verified) verified=$((verified + v)) ;;
        diff)     diff=$((diff + v)) ;;
        error)    errors=$((errors + v)) ;;
      esac
    done
  done
  echo "  $mode: $result"
  if grep -q 'error:' "$OUT/bench_$mode.err" 2>/dev/null; then
    echo "  $mode: bench errors by kind:"
    sed -n 's/^  error: //p' "$OUT/bench_$mode.err" | sed 's/[0-9]\{3,\}/N/g' | sort | uniq -c | sort -rn | awk 'NR<=5' | sed 's/^/    /'
  fi
  printf '  %s: routed=%d verified=%d divergences=%d errors=%d (%d/%d workers reporting)  rss %d -> %d KB (%+d)  conns %s -> %s\n' \
    "$mode" "$routed" "$verified" "$diff" "$errors" "$reporting" "$WORKERS" \
    "$rss_before" "$rss_after" "$((rss_after - rss_before))" "$conns_before" "$conns_after"
  local fivehundred bugs divlines
  fivehundred=$(grep -ac 'Exception during request' "$log" || true)
  bugs=$(grep -ac 'routing path raised' "$log" || true)
  divlines=$(grep -ac 'SHADOW DIVERGENCE' "$log" || true)
  echo "  $mode: 500s=$fivehundred routing-bugs=$bugs divergence-lines=$divlines"
  BURNIN_DIFF=$((${BURNIN_DIFF:-0} + diff + divlines))
  BURNIN_BUGS=$((${BURNIN_BUGS:-0} + fivehundred + bugs))
  BURNIN_ERRORS=$((${BURNIN_ERRORS:-0} + errors))

  if [ "$mode" = "on" ]; then BURNIN_ROUTED=$routed; fi
}

BURNIN_DIFF=0
BURNIN_ERRORS=0
BURNIN_BUGS=0
leg off
leg on
stop

echo
if [ "$(echo "$SAMPLE > 0" | bc -l 2>/dev/null || echo 0)" = "1" ]; then
  echo "note: --sample $SAMPLE makes the 'on' leg answer that fraction of routed"
  echo "      reads TWICE, so the req/s difference above is not the routing gain."
  echo "      Use --sample 0 for a speed comparison."
fi
if [ "${BURNIN_ROUTED:-0}" -lt 1 ]; then
  echo "BURNIN FAILED  the 'on' leg routed nothing; it measured python twice"
  exit 1
elif [ "$BURNIN_DIFF" -eq 0 ] && [ "$BURNIN_BUGS" -eq 0 ]; then
  # A fallback (kernel declined, python answered) is the designed path, not a
  # failure -- the whole posture is failing closed per call. What must be zero
  # is a WRONG answer (divergence) or a broken request (a 500, a routing-path
  # bug); those are fatal. Fallbacks are reported so a spike is visible.
  echo "BURNIN OK   ($DB, ${THREADS} threads, ${SECONDS_}s per leg, ${WORKERS} workers,"\
       "${BURNIN_ROUTED} reads routed, ${BURNIN_ERRORS} fell back to python)"
else
  echo "BURNIN FAILED  divergences=$BURNIN_DIFF request-bugs=$BURNIN_BUGS (fallbacks=$BURNIN_ERRORS, not fatal)"
  exit 1
fi
