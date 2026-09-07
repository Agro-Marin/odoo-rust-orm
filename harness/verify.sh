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

echo "verifying '$DB'   (artifacts in $OUT)"
[ -x "$PY" ] || { echo "no interpreter at $PY (set RUSTORM_PYTHON)"; exit 2; }

if [ -n "$BUILD" ]; then
  if "$PY" "$ODOO/odoo-bin" -c "$RUSTORM_ODOO_CONF" -d "$DB" -i "$BUILD" \
       --db_maxconn=8 --stop-after-init --no-http > "$OUT/install.log" 2>&1; then
    stage "install($BUILD)" OK
  else
    stage "install($BUILD)" FAIL "see $OUT/install.log"; fi
fi

if out=$("$PY" "$ROOT/harness/fork_contract.py" 2>&1); then
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

if "$ROOT/target/release/phase1_shell" > "$OUT/phase1.log" 2>&1; then
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
       > "$OUT/probe_$mode.log" 2>&1; then
    if grep -q "$want" "$OUT/probe_$mode.log"; then stage "$name" OK "$(grep -o "$want.*" "$OUT/probe_$mode.log" | head -1)"
    else stage "$name" FAIL "$(grep -a MISMATCH "$OUT/probe_$mode.log" | head -1 | cut -c1-70)"; fi
  else
    stage "$name" SKIP "see $OUT/probe_$mode.log"; fi
}
probe types "mismatches=0" "cursor type layer"
probe race  "wrong=0"      "concurrency"       "$OUT/export.json"

if [ -f "$ROOT/target/release/libengine_py.so" ]; then

  if PYTHONPATH="$PYMOD" RUSTORM_EXPORT="$OUT/export.json" RUSTORM_ROUTE=shadow \
     RUSTORM_OTHER_DB="${RUSTORM_OTHER_DB:-}" \
     "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
     < "$ROOT/harness/load_into_odoo.py" > "$OUT/load.log" 2>&1
  then
    stage "load into odoo" OK "$(grep -acE '^LOAD (read|other db|fork exit)' "$OUT/load.log") checks"
  else
    stage "load into odoo" FAIL "$(grep -aE '^LOAD|Error' "$OUT/load.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "load into odoo" SKIP "no libengine_py.so; cargo build --release"
fi

if [ -n "${RUSTORM_REPLAY:-}" ] && [ -f "$RUSTORM_REPLAY" ] && [ -f "$ROOT/target/release/libengine_py.so" ]; then
  if PYTHONPATH="$PYMOD" RUSTORM_EXPORT="$OUT/export.json" RUSTORM_ROUTE=shadow \
     RUSTORM_REPLAY="$RUSTORM_REPLAY" \
     "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
     < "$ROOT/harness/replay.py" > "$OUT/replay.log" 2>&1
  then
    stage "replay" OK "$(grep -aE '^REPLAY (OK|DIVERGED)' "$OUT/replay.log" | tail -1 | cut -c8-)"
  else
    stage "replay" FAIL "$(grep -aE '^REPLAY|Error' "$OUT/replay.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "replay" SKIP "no capture file (set RUSTORM_REPLAY)"
fi

if [ -f "$ROOT/target/release/libengine_py.so" ]; then
  if PYTHONPATH="$PYMOD" \
     "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 \
     < "$ROOT/harness/copy_path.py" > "$OUT/copy.log" 2>&1
  then
    stage "copy encoder" OK "$(grep -a '^COPY streams' "$OUT/copy.log" | head -1 | cut -c1-58)"
  else
    stage "copy encoder" FAIL "$(grep -aE '^ *COPY MISMATCH|^COPY ' "$OUT/copy.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "copy encoder" SKIP "no libengine_py.so; cargo build --release"
fi

if [ "$QUICK" = 1 ]; then
  stage "soak" SKIP "--quick"
else
  SOAK_PORT="${RUSTORM_SOAK_PORT:-8099}"
  "$ROOT/target/release/odoo-poc" --db "$DB" --export "$OUT/export.json" \
      serve --port "$SOAK_PORT" > "$OUT/soak_serve.log" 2>&1 &
  soak_pid=$!
  for _ in $(seq 1 30); do
    curl -sf -m 2 "http://127.0.0.1:$SOAK_PORT/health" >/dev/null 2>&1 && break
    sleep 1
  done
  if "$PY" "$ROOT/harness/soak.py" --port "$SOAK_PORT" \
       --threads "${RUSTORM_SOAK_THREADS:-8}" --seconds "${RUSTORM_SOAK_SECONDS:-20}" \
       > "$OUT/soak.log" 2>&1; then
    stage "soak" OK "$(grep -o '[0-9]* requests in .*' "$OUT/soak.log" | head -1)"
  else
    stage "soak" FAIL "$(grep -aE 'MISMATCH|error:|SOAK' "$OUT/soak.log" | head -1 | cut -c1-70)"
  fi
  kill -TERM "$soak_pid" 2>/dev/null
  wait "$soak_pid" 2>/dev/null
fi

fail=0
for r in "${RESULTS[@]}"; do [ "$r" = FAIL ] && fail=1; done
echo
if [ "$fail" = 0 ]; then echo "VERIFY OK   ($DB)"; else echo "VERIFY FAILED   ($DB)"; fi
exit "$fail"
