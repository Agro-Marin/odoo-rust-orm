#!/usr/bin/env bash
# usage: harness/ab_diff.sh [--modules LIST] [--keep]
#
# The A/B database differential: one demo snapshot, three clones, the same
# committed business workload on each -- A with the engine on (routing and
# the persistence port), B on Python, C on Python as the control -- then
# every table of A compared with B cell by cell, and B with C to show what
# the workload itself does not reproduce. Exit 0 only when A equals B and
# B equals C.
#
#   --modules LIST   the snapshot's modules (default below); demo data is on
#   --keep           keep the four databases
#
# environment: RUSTORM_AB_PREFIX (default rustorm_ab), RUSTORM_AB_OUT,
#   RUSTORM_WORKSPACE / RUSTORM_ODOO_ROOT / RUSTORM_PYTHON / RUSTORM_ODOO_CONF
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${RUSTORM_WORKSPACE:-$(cd "$ROOT/.." && pwd)}"
ODOO="${RUSTORM_ODOO_ROOT:-$WORKSPACE/odoo}"
PY="${RUSTORM_PYTHON:-$WORKSPACE/p314o19m/bin/python}"
CONF="${RUSTORM_ODOO_CONF:-$WORKSPACE/p314o19m.conf}"
PREFIX="${RUSTORM_AB_PREFIX:-rustorm_ab}"
OUT="${RUSTORM_AB_OUT:-$(mktemp -d -t rustorm-ab-XXXXXX)}"
MODULES="base,mail,contacts,account,sale,purchase,stock,crm,project,calendar"
KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --modules) MODULES="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
BASE="${PREFIX}_base"
echo "A/B differential   (artifacts in $OUT)"
for db in "$BASE" "${PREFIX}_a" "${PREFIX}_b" "${PREFIX}_c"; do
  if psql -U marin -lqt 2>/dev/null | cut -d'|' -f1 | sed 's/ //g' | grep -qx "$db"; then
    echo "refusing: database '$db' exists; drop it or set RUSTORM_AB_PREFIX" >&2; exit 2
  fi
done
drop_all() {
  [ "$KEEP" = 1 ] && return
  for db in "${PREFIX}_a" "${PREFIX}_b" "${PREFIX}_c" "$BASE"; do
    psql -U marin -d postgres -Atc "select pg_terminate_backend(pid) from pg_stat_activity where datname='$db' and pid<>pg_backend_pid()" >/dev/null 2>&1
    psql -U marin -d postgres -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1
  done
}
trap drop_all EXIT

"$PY" "$ODOO/odoo-bin" -c "$CONF" -d "$BASE" -i "$MODULES" --with-demo \
    --stop-after-init --no-http --db_maxconn=8 > "$OUT/snapshot.log" 2>&1 \
  || { echo "AB DIFF FAILED  snapshot install failed; see $OUT/snapshot.log"; exit 1; }
psql -U marin -d postgres -Atc "select pg_terminate_backend(pid) from pg_stat_activity where datname='$BASE' and pid<>pg_backend_pid()" >/dev/null
for x in a b c; do
  psql -U marin -d postgres -c "CREATE DATABASE ${PREFIX}_$x TEMPLATE $BASE" >/dev/null || { echo "clone $x failed"; exit 1; }
done

# the engine leg's conf: the workspace conf with the addon, armed for A,
# routing on and the port on; the python legs use the conf as given
{
  grep -vE '^(addons_path|server_wide_modules|rust_engine_[a-z_]*)[[:space:]]*=' "$CONF"
  addons=$(sed -nE 's/^addons_path[[:space:]]*=[[:space:]]*//p' "$CONF" | tail -1)
  case ",$addons," in *,"$ROOT/addons",*) echo "addons_path = $addons" ;; *) echo "addons_path = $addons,$ROOT/addons" ;; esac
  echo "server_wide_modules = base,web,rust_engine"
  echo "rust_engine_db = ${PREFIX}_a"
  echo "rust_engine_mode = on"
  echo "rust_engine_verify_sample = 0"
} > "$OUT/on.conf"
{
  grep -vE '^(server_wide_modules|rust_engine_[a-z_]*)[[:space:]]*=' "$CONF"
  echo "server_wide_modules = base,web"
} > "$OUT/python.conf"

leg() {  # leg DB CONF
  "$PY" "$ODOO/odoo-bin" shell -c "$2" -d "$1" --no-http --db_maxconn=8 \
      <<< "exec(open('$ROOT/harness/ab_workload.py').read())" > "$OUT/$1.log" 2>&1
  local rc=$?
  echo "  $1: rc=$rc $(grep -a '^AB WORKLOAD' "$OUT/$1.log" | cut -c1-160)"
  return $rc
}
leg "${PREFIX}_a" "$OUT/on.conf" &
leg "${PREFIX}_b" "$OUT/python.conf" &
leg "${PREFIX}_c" "$OUT/python.conf" &
wait
if grep -aq "refusing to arm" "$OUT/${PREFIX}_a.log"; then
  echo "AB DIFF FAILED  the engine leg refused to arm (stale extension); see $OUT/${PREFIX}_a.log"; exit 1
fi
port=$(grep -a "rust port (final)" "$OUT/${PREFIX}_a.log" | tail -1 | sed 's/.*rust port (final): //' | cut -c1-80)
echo "  engine leg port: ${port:-no report}"
failed=0
echo "  control (B vs C):"
"$PY" "$ROOT/harness/ab_compare.py" "${PREFIX}_b" "${PREFIX}_c" --json "$OUT/bc.json" | tail -8 | sed 's/^/    /' || failed=1
echo "  engine (A vs B):"
"$PY" "$ROOT/harness/ab_compare.py" "${PREFIX}_a" "${PREFIX}_b" --json "$OUT/ab.json" | tail -25 | sed 's/^/    /' || failed=1
case "$port" in create_rows=0*|"") echo "AB DIFF FAILED  the port wrote nothing natively on the engine leg"; exit 1 ;; esac
if [ "$failed" = 0 ]; then echo "AB DIFF OK"; else echo "AB DIFF FAILED"; exit 1; fi
