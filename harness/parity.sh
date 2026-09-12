#!/usr/bin/env bash
#
# Byte parity between a routed server and an unrouted one.
#
#   harness/parity.sh --db <db> --password <admin pw> [--port N] [--models N]
#
# The shadow lane, the replay lane and the kernel sweep all compare a
# METHOD'S RESULT. None of them can see a difference in what the server
# SENDS, and on 2026-09-08 a routed `web_search_read` was dropping the
# response envelope's `version` key while all three reported agreement. This
# drives the same corpus over real HTTP against both and diffs the bytes.
#
# The OFF leg is recorded TWICE. A case whose two baseline passes disagree is
# nondeterministic -- a LIMIT window over equal sort keys, a timestamp in the
# payload -- and is reported separately rather than counted as a divergence,
# because an instrument that cannot tell the two apart is not an instrument.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${RUSTORM_WORKSPACE:-$(cd "$ROOT/.." && pwd)}"
ODOO="${RUSTORM_ODOO_ROOT:-${RUSTORM_ODOO:-$WORKSPACE/odoo}}"
PY="${RUSTORM_PYTHON:-$WORKSPACE/p314o19m/bin/python}"
CONF="${RUSTORM_ODOO_CONF:-$WORKSPACE/p314o19m.conf}"

DB=""; PASSWORD=""; PORT=8075; MODELS=0
OUT="${RUSTORM_PARITY_DIR:-$(mktemp -d -t rustorm-parity-XXXXXX)}"
while [ $# -gt 0 ]; do
  case "$1" in
    --db)       DB="$2"; shift 2 ;;
    --password) PASSWORD="$2"; shift 2 ;;
    --port)     PORT="$2"; shift 2 ;;
    --models)   MODELS="$2"; shift 2 ;;
    --out)      OUT="$2"; shift 2 ;;
    -h|--help)  sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$DB" ] && [ -n "$PASSWORD" ] || { echo "--db and --password are required" >&2; exit 2; }
[ -f "$ROOT/target/release/libengine_py.so" ] || {
  echo "no libengine_py.so; cargo build --release" >&2; exit 2; }

mkdir -p "$OUT/pymod"
cp "$ROOT/target/release/libengine_py.so" "$OUT/pymod/engine_py.so"
echo "parity on '$DB'   (artifacts in $OUT)"

"$PY" - "$CONF" "$OUT/base.conf" "$ROOT/addons" "$PORT" <<'PYEOF'
import re, sys
src, dst, addons, port = sys.argv[1:5]
conf = open(src).read()
conf = re.sub(r"(?m)^addons_path\s*=\s*(.*)$",
              lambda m: "addons_path = %s,%s" % (m.group(1), addons), conf)
# `workers = 0` on purpose: one process, so a leg's behaviour is one
# kernel's and not an average over however many workers happened to serve.
for key, value in (("http_port", port), ("workers", "0"), ("db_maxconn", "8")):
    if re.search(r"(?m)^%s\s*=" % key, conf):
        conf = re.sub(r"(?m)^%s\s*=.*$" % key, "%s = %s" % (key, value), conf)
    else:
        conf += "\n%s = %s" % (key, value)
open(dst, "w").write(conf)
PYEOF

SWM=$(sed -n 's/^server_wide_modules[[:space:]]*=[[:space:]]*//p' "$OUT/base.conf" | tail -1)
SWM="${SWM:-base,web}"
case ",$SWM," in *,rust_engine,*) ;; *) SWM="$SWM,rust_engine" ;; esac

stop() {
  for p in $(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u); do
    kill -TERM "$p" 2>/dev/null || true
  done
  sleep 3
}
trap stop EXIT

if RUSTORM_PARITY_OUT="$OUT/corpus.json" RUSTORM_PARITY_MODELS="$MODELS" \
   "$PY" "$ODOO/odoo-bin" shell -c "$CONF" -d "$DB" --no-http --db_maxconn=4 \
   <<< "exec(open('$ROOT/harness/parity_corpus.py').read())" > "$OUT/corpus.log" 2>&1
then
  echo "  $(grep -a '^PARITY CORPUS' "$OUT/corpus.log" | head -1)"
else
  echo "  corpus generation FAILED; see $OUT/corpus.log" >&2; exit 1
fi

boot() {
  local mode="$1"
  stop
  { grep -vE '^(rust_engine_|server_wide_modules[[:space:]]*=)' "$OUT/base.conf"
    echo "server_wide_modules = $SWM"
    echo "rust_engine_db = $DB"
    echo "rust_engine_mode = $mode"
    echo "rust_engine_verify_sample = 0"
    # The OFF leg is Python answering 1,387 realistic RPC calls, which is
    # exactly what the replay lane wants and what it has never had. Capturing
    # it here costs one config line and no extra boot; `verify.sh` picks the
    # file up if nobody supplied one of their own.
    [ "$mode" = off ] && echo "rust_engine_capture = $OUT/capture.jsonl"
    # Short, because the ONLY evidence that the `on` leg routed anything is
    # this report. A parity run whose routed count is zero compared python
    # with python and its OK means nothing -- the denominator-of-zero trap,
    # and the guard below is the whole reason the number is asked for.
    echo "rust_engine_report_seconds = 5"
  } > "$OUT/$mode.conf"
  PYTHONPATH="$OUT/pymod" setsid nohup "$PY" "$ODOO/odoo-bin" -c "$OUT/$mode.conf" -d "$DB" \
      > "$OUT/$mode.log" 2>&1 < /dev/null &
  for _ in $(seq 1 120); do ss -ltn | grep -q ":$PORT " && break; sleep 1; done
  sleep 5
}

record() {
  "$PY" "$ROOT/harness/http_parity.py" --port "$PORT" --db "$DB" \
      --password "$PASSWORD" --corpus "$OUT/corpus.json" --out "$1" \
      >> "$OUT/record.log" 2>&1 \
    || { echo "  recording to $1 FAILED; see $OUT/record.log" >&2; exit 1; }
}

boot off
record "$OUT/off_a.json"
record "$OUT/off_b.json"
grep -a 'routing mode' "$OUT/off.log" | tail -1 | sed 's/^/  /'
boot on
record "$OUT/on.json"
grep -a 'routing mode' "$OUT/on.log" | tail -1 | sed 's/^/  /'
sleep 7   # let at least one report tick land before the log stops growing
ROUTED=$(grep -a 'rust kernel: mode=on' "$OUT/on.log" | tail -1 \
         | sed -n 's/.*[ =]routed=\([0-9][0-9]*\).*/\1/p')
ROUTED=${ROUTED:-0}
echo "  on leg routed $ROUTED reads"
stop

if [ "$ROUTED" -lt 1 ]; then
  echo
  echo "PARITY VACUOUS  the 'on' leg routed nothing; it compared python with python"
  exit 1
fi

"$PY" - "$OUT/corpus.json" "$OUT/off_a.json" "$OUT/off_b.json" "$OUT/on.json" <<'PYEOF'
import json, sys, collections
corpus, a_path, b_path, on_path = sys.argv[1:5]
cases = {c["id"]: c for c in json.load(open(corpus))}
a, b, on = (json.load(open(p)) for p in (a_path, b_path, on_path))

unstable = sorted(k for k in a if a[k] != b.get(k))
stable = [k for k in a if k not in set(unstable)]
diverged = sorted(k for k in stable if a[k] != on.get(k))

by_shape = collections.Counter(
    "%s.%s" % (cases[k]["model"], cases[k]["method"]) for k in diverged)
unstable_shape = collections.Counter(
    "%s.%s" % (cases[k]["model"], cases[k]["method"]) for k in unstable)

# How much of the corpus actually exercised a read, so an OK is readable.
# A case that raises AccessError on both legs agrees about the access
# decision and nothing else; a corpus of nothing but those would pass this
# gate while proving no answer was ever compared.
def _answered(body):
    try:
        payload = json.loads(body)
    except ValueError:
        return False          # a transport line or a non-JSON error body
    return isinstance(payload, dict) and "error" not in payload

answered = sum(1 for k in stable if _answered(a[k]))
print("  cases %d   stable %d   nondeterministic %d   answered %d (rest raised on both legs)   DIVERGED %d"
      % (len(a), len(stable), len(unstable), answered, len(diverged)))
for shape, n in unstable_shape.most_common(5):
    print("    nondeterministic: %-45s x%d" % (shape, n))
for shape, n in by_shape.most_common(10):
    print("    DIVERGED: %-45s x%d" % (shape, n))
for k in diverged[:3]:
    off_s, on_s = a[k], on.get(k, "")
    i = next((i for i, (x, y) in enumerate(zip(off_s, on_s)) if x != y), min(len(off_s), len(on_s)))
    print("    %s %s.%s first diff at byte %d"
          % (k, cases[k]["model"], cases[k]["method"], i))
    print("      off: %s" % off_s[max(0, i - 60):i + 90])
    print("      on : %s" % on_s[max(0, i - 60):i + 90])

print()
print("PARITY OK" if not diverged else "PARITY FAILED  %d diverged" % len(diverged))
sys.exit(0 if not diverged else 1)
PYEOF
