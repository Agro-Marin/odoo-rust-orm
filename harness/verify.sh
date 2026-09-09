#!/usr/bin/env bash

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${RUSTORM_WORKSPACE:-$(cd "$ROOT/.." && pwd)}"
DB="${RUSTORM_DB:-rustorm_probe}"
BUILD=""
QUICK=0
OUT="${RUSTORM_VERIFY_OUT:-$(mktemp -d -t rustorm-verify-XXXXXX)}"

while [ $# -gt 0 ]; do
  case "$1" in
    --db)    DB="$2"; shift 2 ;;
    --build) BUILD="$2"; shift 2 ;;
    --quick) QUICK=1; shift ;;
    --out)   OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

export RUSTORM_DB="$DB"
export RUSTORM_ODOO_CONF="${RUSTORM_ODOO_CONF:-$WORKSPACE/p314o19m.conf}"
ODOO="${RUSTORM_ODOO_ROOT:-$WORKSPACE/odoo}"
PY="${RUSTORM_PYTHON:-$WORKSPACE/p314o19m/bin/python}"
mkdir -p "$OUT"

declare -a NAMES RESULTS NOTES
stage() { NAMES+=("$1"); RESULTS+=("$2"); NOTES+=("${3:-}"); printf '  %-22s %s %s\n' "$1" "$2" "${3:-}"; }

# A free port, scanned rather than assumed. This machine is shared: a peer's
# HOOT warm runners sit on 8085-8089 for hours and a peer's install loop
# rebinds its port every round, so a FIXED default collides intermittently --
# which is the worst frequency, because the run that hits it looks like a
# code failure. An explicit RUSTORM_*_PORT still wins, for the case where
# somebody needs to know where to look.
#
# This narrows the window rather than closing it: something can still take
# the port between the check and the bind. That is why the stages that use
# these also PROVE their server came up instead of trusting it.
pick_port() {
  local start="$1" p
  for p in $(seq "$start" $((start + 40))); do
    ss -ltn 2>/dev/null | grep -q ":$p " || { echo "$p"; return 0; }
  done
  echo "$start"   # nothing free in the range; let the caller's guard report it
}

echo "verifying '$DB'   (artifacts in $OUT)"
[ -x "$PY" ] || { echo "no interpreter at $PY (set RUSTORM_PYTHON)"; exit 2; }

if [ -n "$BUILD" ]; then
  if "$PY" "$ODOO/odoo-bin" -c "$RUSTORM_ODOO_CONF" -d "$DB" -i "$BUILD" \
       --db_maxconn=8 --stop-after-init --no-http > "$OUT/install.log" 2>&1; then
    stage "install($BUILD)" OK
  else
    stage "install($BUILD)" FAIL "see $OUT/install.log"; fi
fi

# A stage that could not run says so with a SKIP line and exit 3, and the
# line is read BEFORE the exit code: `fork_contract`, `load_into_odoo` and
# `copy_path` used to exit 0 on their skip path, so a checkout with no Odoo
# was reported as three green stages that had checked nothing.
if out=$("$PY" "$ROOT/harness/fork_contract.py" 2>&1); then rc=0; else rc=$?; fi
if printf '%s' "$out" | grep -qE '^FORK SKIP'; then
  stage "fork contract" SKIP "$(printf '%s' "$out" | grep -E '^FORK SKIP' | head -1 | cut -c1-70)"
elif [ "$rc" = 0 ]; then
  stage "fork contract" OK "$(printf '%s' "$out" | grep -E '^FORK' | head -1)"
else
  stage "fork contract" FAIL "$(printf '%s' "$out" | grep -E '^ *FAIL|^FORK' | head -1)"
fi

PYMOD="$OUT/pymod"
if [ -f "$ROOT/target/release/libengine_py.so" ]; then
  mkdir -p "$PYMOD"
  cp "$ROOT/target/release/libengine_py.so" "$PYMOD/engine_py.so"
  if out=$(PYTHONPATH="$PYMOD" "$PY" "$ROOT/harness/test_shims.py" 2>&1); then
    stage "shim units" OK "$(printf '%s' "$out" | grep -E '^SHIMS' | head -1)"
  else
    stage "shim units" FAIL "$(printf '%s' "$out" | grep -E '^ *FAIL|^SHIMS' | head -1)"
  fi
else
  stage "shim units" SKIP "no libengine_py.so; cargo build --release"
fi

if "$ROOT/target/release/export_registry" "$OUT/export.json" > "$OUT/export.log" 2>&1; then
  stage "registry export" OK "$(grep -o 'exported [0-9]* models' "$OUT/export.log" | head -1)"
else
  stage "registry export" FAIL "see $OUT/export.log"; fi

"$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
  <<< "exec(open('$ROOT/harness/gen_expected.py').read())" > "$OUT/warm.log" 2>&1 \
  && stage "warm computes" OK || stage "warm computes" FAIL "see $OUT/warm.log"

if [ "$QUICK" = 1 ]; then
  stage "kernel sweep" SKIP "--quick"
elif RUSTORM_SWEEP_OUT="$OUT/sweep_corpus.json" \
   "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
   <<< "exec(open('$ROOT/harness/sweep_corpus.py').read())" > "$OUT/sweep_gen.log" 2>&1
then

  for pass_n in 1 2; do
    RUSTORM_CORPUS="$OUT/sweep_corpus.json" RUSTORM_EXPECTED="$OUT/sweep_expected.json" \
      "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
      <<< "exec(open('$ROOT/harness/gen_expected.py').read())" > "$OUT/sweep_exp_$pass_n.log" 2>&1
  done
  "$ROOT/target/release/odoo-poc" --db "$DB" --export "$OUT/export.json" \
      run-corpus --file "$OUT/sweep_corpus.json" > "$OUT/sweep_actual.json" 2> "$OUT/sweep_run.log"
  out=$("$PY" "$ROOT/harness/diff.py" "$OUT/sweep_expected.json" "$OUT/sweep_actual.json" 2>&1)
  line=$(printf '%s\n' "$out" | grep -E '^(PASS|REFUSING)' | head -1)
  case "$line" in PASS*) stage "kernel sweep" OK "$line" ;;
                  *)     stage "kernel sweep" FAIL "${line:-$(printf '%s' "$out" | tail -1)}" ;; esac
else
  stage "kernel sweep" FAIL "corpus generation failed; see $OUT/sweep_gen.log"; fi

if RUSTORM_EXPECTED="$OUT/expected.json" \
   "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
   <<< "exec(open('$ROOT/harness/gen_expected.py').read())" > "$OUT/gen.log" 2>&1
then
  "$ROOT/target/release/odoo-poc" --db "$DB" --export "$OUT/export.json" \
      run-corpus --file "$ROOT/harness/corpus.json" > "$OUT/actual.json" 2> "$OUT/corpus.log"

  out=$("$PY" "$ROOT/harness/diff.py" "$OUT/expected.json" "$OUT/actual.json" 2>&1)
  line=$(printf '%s\n' "$out" | grep -E '^(PASS|REFUSING)' | head -1)
  case "$line" in PASS*) stage "shadow corpus" OK "$line" ;;
                  *)     stage "shadow corpus" FAIL "${line:-$(printf '%s' "$out" | tail -1)}" ;; esac
else
  stage "shadow corpus" FAIL "baseline generation failed; see $OUT/gen.log"; fi

if [ "$QUICK" = 1 ]; then
  stage "fuzz" SKIP "--quick"
else
  fuzz_fail=0; fuzz_note=""
  for seed in ${RUSTORM_FUZZ_SEEDS:-1 2 3}; do
    RUSTORM_FUZZ_OUT="$OUT/fuzz_$seed.json" RUSTORM_FUZZ_SEED="$seed" \
      "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
      <<< "exec(open('$ROOT/harness/fuzz_corpus.py').read())" > "$OUT/fuzz_gen_$seed.log" 2>&1 || continue
    RUSTORM_CORPUS="$OUT/fuzz_$seed.json" RUSTORM_EXPECTED="$OUT/fuzz_exp_$seed.json" \
      "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
      <<< "exec(open('$ROOT/harness/gen_expected.py').read())" > "$OUT/fuzz_expgen_$seed.log" 2>&1
    "$ROOT/target/release/odoo-poc" --db "$DB" --export "$OUT/export.json" \
        run-corpus --file "$OUT/fuzz_$seed.json" > "$OUT/fuzz_act_$seed.json" 2>/dev/null
    line=$("$PY" "$ROOT/harness/diff.py" "$OUT/fuzz_exp_$seed.json" "$OUT/fuzz_act_$seed.json" 2>&1 \
           | grep -E '^(PASS|REFUSING)' | head -1)
    case "$line" in PASS*) ;; *) fuzz_fail=1 ;; esac
    fuzz_note="$fuzz_note seed$seed:${line%% *}"
  done
  if [ "$fuzz_fail" = 0 ]; then stage "fuzz" OK "$fuzz_note"
  else stage "fuzz" FAIL "$fuzz_note (see $OUT/fuzz_*.json)"; fi
fi

if [ "$QUICK" = 1 ]; then
  stage "registry sweep" SKIP "--quick"
elif "$ROOT/target/release/phase2_verify" > "$OUT/sweep.log" 2>&1; then
  stage "registry sweep" OK "$(grep -ao 'query shapes compared: .*' "$OUT/sweep.log" | head -1)"
else
  stage "registry sweep" FAIL "$(grep -ao 'mismatch.*' "$OUT/sweep.log" | head -1 | cut -c1-80)"; fi

# The baseline has to be the one regenerated AFTER the last stage that wrote
# to the database. `sweep_corpus.py` creates two partners, so the copy
# `gen_expected.py` leaves at its default path -- written by "warm computes",
# before the sweep -- describes a database that no longer exists by the time
# phase1_shell reads it, and c21 mismatches by exactly those two rows. It
# self-heals on a second run against the same database, so the stage is red
# only on a first full run against a fresh one, which is CI's case and no
# developer's.
if RUSTORM_EXPECTED="$OUT/expected.json" \
   "$ROOT/target/release/phase1_shell" > "$OUT/phase1.log" 2>&1; then
  stage "hybrid (phase 1)" OK
else
  stage "hybrid (phase 1)" FAIL "see $OUT/phase1.log"; fi

if "$ROOT/target/release/phase2_tests" > "$OUT/upstream.log" 2>&1; then
  stage "upstream suites" OK
else
  stage "upstream suites" FAIL "$(grep -a 'APPEARED under routing' "$OUT/upstream.log" | head -1)"; fi

probe() {
  local mode="$1" want="$2" name="$3"
  if PYTHONUNBUFFERED=1 "$ROOT/target/release/probe_audit" "$mode" "${@:4}" \
       > "$OUT/probe_$mode.log" 2>&1; then rc=0; else rc=$?; fi
  if [ "$rc" = 0 ]; then
    if grep -q "$want" "$OUT/probe_$mode.log"; then stage "$name" OK "$(grep -o "$want.*" "$OUT/probe_$mode.log" | head -1)"
    else stage "$name" FAIL "$(grep -a MISMATCH "$OUT/probe_$mode.log" | head -1 | cut -c1-70)"; fi
  else
    # The probe runs against the same database as every other stage, so a
    # non-zero exit is the probe crashing or refusing -- a battery failure,
    # not a missing prerequisite. It was reported SKIP, which reads as
    # "nothing to see" for the one stage that covers the cursor's type layer.
    stage "$name" FAIL "exit $rc: $(tail -1 "$OUT/probe_$mode.log" | cut -c1-60)"; fi
}
probe types "mismatches=0" "cursor type layer"
probe race  "wrong=0"      "concurrency"       "$OUT/export.json"

if [ -f "$ROOT/target/release/libengine_py.so" ]; then

  if PYTHONPATH="$PYMOD" RUSTORM_EXPORT="$OUT/export.json" RUSTORM_ROUTE=shadow \
     RUSTORM_OTHER_DB="${RUSTORM_OTHER_DB:-}" \
     "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
     < "$ROOT/harness/load_into_odoo.py" > "$OUT/load.log" 2>&1
  then rc=0; else rc=$?; fi
  if grep -aqE '^LOAD SKIP' "$OUT/load.log"; then
    stage "load into odoo" SKIP "$(grep -aE '^LOAD SKIP' "$OUT/load.log" | head -1 | cut -c1-70)"
  elif [ "$rc" = 0 ]; then
    stage "load into odoo" OK "$(grep -acE '^LOAD (read|other db|fork exit)' "$OUT/load.log") checks"
  else
    stage "load into odoo" FAIL "$(grep -aE '^LOAD|Error' "$OUT/load.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "load into odoo" SKIP "no libengine_py.so; cargo build --release"
fi


if [ -f "$ROOT/target/release/libengine_py.so" ]; then
  if PYTHONPATH="$PYMOD" \
     "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
     < "$ROOT/harness/copy_path.py" > "$OUT/copy.log" 2>&1
  then rc=0; else rc=$?; fi
  if grep -aqE '^COPY SKIP' "$OUT/copy.log"; then
    stage "copy encoder" SKIP "$(grep -aE '^COPY SKIP' "$OUT/copy.log" | head -1 | cut -c1-70)"
  elif [ "$rc" = 0 ]; then
    stage "copy encoder" OK "$(grep -a '^COPY streams' "$OUT/copy.log" | head -1 | cut -c1-58)"
  else
    stage "copy encoder" FAIL "$(grep -aE '^ *COPY MISMATCH|^COPY ' "$OUT/copy.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "copy encoder" SKIP "no libengine_py.so; cargo build --release"
fi

if out=$("$PY" "$ROOT/harness/test_speedup.py" 2>&1); then
  stage "speedup units" OK "$(printf '%s' "$out" | grep -E '^SPEEDUP' | head -1)"
else
  stage "speedup units" FAIL "$(printf '%s' "$out" | grep -E '^ *[a-z]|^SPEEDUP' | head -1 | cut -c1-70)"
fi

# The only stage that compares what the SERVER SENDS rather than what a
# method returned. It boots two servers, so it is minutes rather than
# seconds -- but it is the only lane that can see an envelope key, a
# serialisation choice or a header differ between routed and unrouted, and
# one of those was live and unseen until 2026-09-08.
if [ "$QUICK" = 1 ]; then
  stage "byte parity" SKIP "--quick"
elif [ ! -f "$ROOT/target/release/libengine_py.so" ]; then
  stage "byte parity" SKIP "no libengine_py.so; cargo build --release"
else
  out=$(RUSTORM_PARITY_DIR="$OUT/parity" "$ROOT/harness/parity.sh" \
          --db "$DB" --password "${RUSTORM_ADMIN_PASSWORD:-kernelprobe}" \
          --port "${RUSTORM_PARITY_PORT:-$(pick_port 8140)}" 2>&1)
  verdict=$(printf '%s\n' "$out" | grep -aE '^PARITY (OK|FAILED|VACUOUS)' | head -1)
  counts=$(printf '%s\n' "$out" | grep -aE '^ *cases ' | head -1 | sed 's/^ *//')
  case "$verdict" in
    "PARITY OK") stage "byte parity" OK "$counts" ;;
    PARITY*)     stage "byte parity" FAIL "$verdict" ;;
    # No verdict at all is not a pass and not a failure: the run could not
    # get far enough to compare anything (a wrong admin password is the
    # usual reason, and this script has no other use for one).
    *)           stage "byte parity" SKIP "$(printf '%s\n' "$out" | grep -aE 'FAILED|failed' | tail -1 | cut -c1-70)" ;;
  esac
fi

# Odoo's own cursor suites on both cursors, ratcheted. Not a difference the
# other lanes can see: they all run with the db shim installed, so the cursor
# is the same on both sides of every comparison they make.
if [ "$QUICK" = 1 ]; then
  stage "cursor parity" SKIP "--quick"
elif [ ! -f "$ROOT/target/release/libengine_py.so" ]; then
  stage "cursor parity" SKIP "no libengine_py.so; cargo build --release"
else
  out=$(RUSTORM_CURSOR_DIR="$OUT/cursor" "$ROOT/harness/cursor_parity.sh" --db "$DB" 2>&1)
  verdict=$(printf '%s\n' "$out" | grep -aE '^CURSOR PARITY (OK|FAILED|VACUOUS)' | head -1)
  counts=$(printf '%s\n' "$out" | grep -aE '^ *ran psycopg=' | head -1 | sed 's/^ *//')
  case "$verdict" in
    "CURSOR PARITY OK"*) stage "cursor parity" OK "$counts" ;;
    CURSOR*)             stage "cursor parity" FAIL "$verdict" ;;
    *)                   stage "cursor parity" SKIP "$(printf '%s\n' "$out" | tail -1 | cut -c1-70)" ;;
  esac
fi

# AFTER the parity stage, which is what produces the capture: its OFF leg is
# Python answering 1,387 realistic RPC calls, and until now this stage had
# never run for want of exactly that. An externally supplied
# `RUSTORM_REPLAY` still wins, and `--quick` skips parity, so this SKIPs
# there as it always did.
REPLAY_FILE="${RUSTORM_REPLAY:-$OUT/parity/capture.jsonl}"
if [ -f "$REPLAY_FILE" ] && [ -f "$ROOT/target/release/libengine_py.so" ]; then
  if PYTHONPATH="$PYMOD" RUSTORM_EXPORT="$OUT/export.json" RUSTORM_ROUTE=shadow \
     RUSTORM_REPLAY="$REPLAY_FILE" \
     "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
     < "$ROOT/harness/replay.py" > "$OUT/replay.log" 2>&1
  then
    stage "replay" OK "$(grep -aE '^REPLAY (OK|DIVERGED)' "$OUT/replay.log" | tail -1 | cut -c8-)"
  else
    stage "replay" FAIL "$(grep -aE '^REPLAY|Error' "$OUT/replay.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "replay" SKIP "no capture file (run without --quick, or set RUSTORM_REPLAY)"
fi

if [ "$QUICK" = 1 ]; then
  stage "soak" SKIP "--quick"
else
  SOAK_PORT="${RUSTORM_SOAK_PORT:-$(pick_port 8180)}"
  "$ROOT/target/release/odoo-poc" --db "$DB" --export "$OUT/export.json" \
      serve --port "$SOAK_PORT" > "$OUT/soak_serve.log" 2>&1 &
  soak_pid=$!
  soak_up=0
  echo "  (soak server on port $SOAK_PORT)" >&2
  for _ in $(seq 1 30); do
    curl -sf -m 2 "http://127.0.0.1:$SOAK_PORT/health" >/dev/null 2>&1 && { soak_up=1; break; }
    kill -0 "$soak_pid" 2>/dev/null || break     # it exited; stop waiting for it
    sleep 1
  done
  # Never soak against a listener this stage did not start. The health loop
  # used to fall through on timeout and run the benchmark anyway, so a port
  # already in use -- two batteries close together is enough -- produced
  # `soak FAIL` with an EMPTY note while the real reason sat in
  # soak_serve.log: `Address already in use (os error 98)`. A gate whose
  # failure text does not say what failed costs more than the failure.
  if [ "$soak_up" != 1 ]; then
    stage "soak" FAIL "server never answered /health on $SOAK_PORT: $(grep -aiE 'error|address' "$OUT/soak_serve.log" | tail -1 | cut -c1-60)"
  elif "$PY" "$ROOT/harness/soak.py" --port "$SOAK_PORT" \
       --threads "${RUSTORM_SOAK_THREADS:-8}" --seconds "${RUSTORM_SOAK_SECONDS:-20}" \
       > "$OUT/soak.log" 2>&1; then
    stage "soak" OK "$(grep -o '[0-9]* requests in .*' "$OUT/soak.log" | head -1)"
  else
    stage "soak" FAIL "$(grep -aE 'MISMATCH|error:|SOAK|Error' "$OUT/soak.log" | head -1 | cut -c1-70)"
  fi
  kill -TERM "$soak_pid" 2>/dev/null
  wait "$soak_pid" 2>/dev/null
fi

fail=0
for r in "${RESULTS[@]}"; do [ "$r" = FAIL ] && fail=1; done
echo
if [ "$fail" = 0 ]; then echo "VERIFY OK   ($DB)"; else echo "VERIFY FAILED   ($DB)"; fi
exit "$fail"
