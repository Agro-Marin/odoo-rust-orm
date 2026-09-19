#!/usr/bin/env bash
set -uo pipefail

usage() {
  cat <<'USAGE'
usage: harness/gate.sh [--keep] [--quick] [--ab]

The whole gate, unattended: build and test the crates, deploy the extension
into the venv, create the two scratch databases the battery needs, run
verify.sh with the ORM lane, drop the databases, and leave one summary with
the four repositories' tips beside it. Exit 0 only when every stage passed.

There is no CI here: a gate runs when a person runs it, and the fork's
cursor contract moved for four days in September 2026 before anyone did.
This is the one command to run after a sync, or from a timer.

  --keep    keep the scratch databases (for a re-run or a look)
  --quick   pass --quick to verify.sh (skips sweep, fuzz, tours, soak)
  --ab      also run harness/ab_diff.sh, the A/B database differential (~15 min)

environment: RUSTORM_GATE_OUT (summary dir, default ~/.cache/rustorm-gate),
  RUSTORM_GATE_DB_PREFIX (default rustorm_gate), plus everything verify.sh reads.
USAGE
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${RUSTORM_WORKSPACE:-$(cd "$ROOT/.." && pwd)}"
ODOO="${RUSTORM_ODOO_ROOT:-$WORKSPACE/odoo}"
PY="${RUSTORM_PYTHON:-$WORKSPACE/p314o19m/bin/python}"
CONF="${RUSTORM_ODOO_CONF:-$WORKSPACE/p314o19m.conf}"
PREFIX="${RUSTORM_GATE_DB_PREFIX:-rustorm_gate}"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="${RUSTORM_GATE_OUT:-$HOME/.cache/rustorm-gate}/$STAMP"
KEEP=0; QUICK=""; AB=0
while [ $# -gt 0 ]; do
  case "$1" in
    --keep)  KEEP=1; shift ;;
    --quick) QUICK="--quick"; shift ;;
    --ab)    AB=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
mkdir -p "$OUT"
PROBE="${PREFIX}_probe"; ORMT="${PREFIX}_ormt"
SUMMARY="$OUT/summary.txt"
FAILED=0
say() { echo "$*" | tee -a "$SUMMARY"; }
step() {
  if [ "$2" = 0 ]; then say "  $1: OK"; else say "  $1: FAIL (rc=$2, see $OUT)"; FAILED=1; fi
}

say "rustorm gate $STAMP   (artifacts in $OUT)"
for r in odoo enterprise agromarin design-themes odoo-rust-orm; do
  say "  $r $(git -C "$WORKSPACE/$r" rev-parse --short HEAD 2>/dev/null || echo ABSENT) $(git -C "$WORKSPACE/$r" status --porcelain 2>/dev/null | wc -l) dirty"
done

for db in "$PROBE" "$ORMT"; do
  if psql -U marin -lqt 2>/dev/null | cut -d'|' -f1 | sed 's/ //g' | grep -qx "$db"; then
    say "  refusing: database '$db' exists; drop it or set RUSTORM_GATE_DB_PREFIX"
    exit 2
  fi
done

( cd "$ROOT" && cargo build --release > "$OUT/build.log" 2>&1 ); step "cargo build --release" $?
( cd "$ROOT" && cargo test --release -q > "$OUT/cargo_test.log" 2>&1 ); step "cargo test" $?
bash "$ROOT/harness/install_engine.sh" > "$OUT/install_engine.log" 2>&1; step "install engine into venv" $?
[ "$FAILED" = 0 ] || { say "GATE FAILED (build)"; exit 1; }

"$PY" "$ODOO/odoo-bin" -c "$CONF" -d "$ORMT" \
    -i test_orm,test_read_group,test_access_rights,test_search_panel,test_inherits \
    --stop-after-init --no-http --db_maxconn=8 > "$OUT/ormt_create.log" 2>&1
step "create $ORMT" $?

RUSTORM_VERIFY_OUT="$OUT/verify" RUSTORM_DB="$PROBE" RUSTORM_ORM_TEST_DB="$ORMT" \
  bash "$ROOT/harness/verify.sh" --build mail,contacts $QUICK > "$OUT/verify.out" 2>&1
rc=$?
cat "$OUT/verify.out" >> "$SUMMARY"
step "verify.sh" $rc

if [ "$AB" = 1 ]; then
  RUSTORM_AB_PREFIX="${PREFIX}_ab" RUSTORM_AB_OUT="$OUT/ab" bash "$ROOT/harness/ab_diff.sh" > "$OUT/ab.out" 2>&1
  rc=$?
  cat "$OUT/ab.out" >> "$SUMMARY"
  step "ab_diff.sh" $rc
fi

if [ "$KEEP" = 0 ]; then
  for db in "$PROBE" "$ORMT"; do
    psql -U marin -d postgres -Atc \
      "select pg_terminate_backend(pid) from pg_stat_activity where datname='$db' and pid<>pg_backend_pid()" >/dev/null 2>&1
    psql -U marin -d postgres -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1
  done
fi

if [ "$FAILED" = 0 ]; then say "GATE OK"; else say "GATE FAILED"; exit 1; fi
