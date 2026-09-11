#!/usr/bin/env bash
#
# Odoo's own cursor suites, run against the RUST cursor and against psycopg,
# diffed test by test.
#
#   harness/cursor_parity.sh --db <db>
#
# `phase2_tests` cannot do this. It differences a baseline leg against a
# routed leg and what it toggles is ORM ROUTING -- the db shim is installed in
# both, so the cursor is rust-backed on both sides and a cursor regression
# lands in the "pre-existing in both modes" bucket the gate ignores. Measured:
# adding `test_db_cursor` there gives `baseline 44/379 -> routed 44/379`,
# which reads as clean.
#
# So the axis here is the CURSOR, not the routing, and the comparison is by
# test NAME rather than by count -- a count reads "one fixed, one new" as no
# change. Failures that are artifacts of driving Odoo's suites with a bare
# `unittest` runner appear on both sides and cancel.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${RUSTORM_WORKSPACE:-$(cd "$ROOT/.." && pwd)}"
ODOO="${RUSTORM_ODOO:-$WORKSPACE/odoo}"
PY="${RUSTORM_PYTHON:-$WORKSPACE/p314o19m/bin/python}"
CONF="${RUSTORM_ODOO_CONF:-$WORKSPACE/p314o19m.conf}"

DB="${RUSTORM_DB:-rustorm_probe}"
OUT="${RUSTORM_CURSOR_DIR:-$(mktemp -d -t rustorm-cursor-XXXXXX)}"
while [ $# -gt 0 ]; do
  case "$1" in
    --db)  DB="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -f "$ROOT/target/release/libengine_py.so" ] || {
  echo "no libengine_py.so; cargo build --release" >&2; exit 2; }

mkdir -p "$OUT/pymod"
cp "$ROOT/target/release/libengine_py.so" "$OUT/pymod/engine_py.so"
echo "cursor parity on '$DB'   (artifacts in $OUT)"

leg() {
  local mode="$1"
  RUSTORM_CURSOR="$mode" RUSTORM_CURSOR_OUT="$OUT/$mode.json" RUSTORM_DB="$DB" \
    PYTHONPATH="$OUT/pymod" \
    "$PY" "$ODOO/odoo-bin" shell -c "$CONF" -d "$DB" --no-http --db_maxconn=8 \
    <<< "exec(open('$ROOT/harness/cursor_suite.py').read())" > "$OUT/$mode.log" 2>&1
  local line
  line=$(grep -a '^CURSOR SUITE' "$OUT/$mode.log" | head -1)
  if [ -z "$line" ]; then
    echo "  $mode leg produced no result; see $OUT/$mode.log" >&2
    return 1
  fi
  echo "  $line"
}

leg psycopg || exit 1
leg rust    || exit 1

"$PY" - "$OUT/psycopg.json" "$OUT/rust.json" "$ROOT/harness/cursor_parity_baseline.json" <<'PYEOF'
import json, sys
psy, rust = (json.load(open(p)) for p in sys.argv[1:3])
ran_p = sum(v for k, v in psy.items() if k.startswith("__ran__"))
ran_r = sum(v for k, v in rust.items() if k.startswith("__ran__"))
names = {
    k for k in set(psy) | set(rust)
    if not k.startswith("__")
}
# Second half of the vacuity guard: the suite refuses to write a leg that ran
# on psycopg, and this refuses to SCORE one whose marker is missing -- an old
# artifact, or a leg that never reached the check.
if rust.get("__cursor__") != "FakeConnection" or not rust.get("__connects__"):
    print(
        "  VACUOUS: the rust leg reports cursor=%r connects=%r; it did not run "
        "on the rust cursor and this comparison proves nothing"
        % (rust.get("__cursor__"), rust.get("__connects__"))
    )
    sys.exit(1)
detail = rust.get("__detail__", {})
excluded = sorted({n for v in rust.values() if isinstance(v, list) for n in v})

def bad(d, k):
    return d.get(k) in ("fail", "error")

only_rust = sorted(k for k in names if bad(rust, k) and not bad(psy, k))
only_psy  = sorted(k for k in names if bad(psy, k) and not bad(rust, k))
both      = sorted(k for k in names if bad(psy, k) and bad(rust, k))

print("  ran psycopg=%d rust=%d   not-ok both=%d   ONLY-RUST=%d   only-psycopg=%d"
      % (ran_p, ran_r, len(both), len(only_rust), len(only_psy)))
if excluded:
    print("  excluded from both legs: %s" % ", ".join(excluded))
import collections
shapes = collections.Counter(
    detail.get(k, "?").split(":")[0][:60] for k in only_rust
)
for shape, n in shapes.most_common(10):
    print("    ONLY-RUST x%-3d %s" % (n, shape))
for k in only_rust[:6]:
    print("      %s\n        %s" % (k.split(".")[-1], detail.get(k, "?")[:150]))
for k in only_psy[:5]:
    print("    only under psycopg: %s (%s)" % (k, psy.get(k)))
print()
if ran_r == 0 or ran_p == 0:
    print("CURSOR PARITY VACUOUS  a leg ran no tests; it compared nothing")
    sys.exit(1)
# The baseline file owns the remaining differences; this gate requires an
# exact match, as the repo's other ratchets do, so fixing
# one is expected to lower the baseline in the same commit rather than bank
# slack for later.
import os
base_path = os.environ.get("RUSTORM_CURSOR_BASELINE", sys.argv[3])
try:
    baseline = json.load(open(base_path))["only_rust"]
except Exception as exc:
    print("CURSOR PARITY FAILED  no baseline at %s (%s)" % (base_path, exc))
    sys.exit(1)

if len(only_rust) > baseline:
    print("CURSOR PARITY FAILED  %d test(s) fail only under the rust cursor, "
          "baseline %d -- %d NEW" % (len(only_rust), baseline, len(only_rust) - baseline))
    sys.exit(1)
if len(only_rust) < baseline:
    print("CURSOR PARITY FAILED  %d fail only under the rust cursor, baseline "
          "%d: lower the baseline in the same commit that fixed them"
          % (len(only_rust), baseline))
    sys.exit(1)
print("CURSOR PARITY OK   (%d tests; %d fail only under the rust cursor, at the "
      "baseline; %d fail identically on both)" % (ran_r, len(only_rust), len(both)))
PYEOF
