#!/usr/bin/env bash
# usage: harness/verify.sh [--db NAME] [--build MODULES] [--quick] [--out DIR]
#
# Runs every verification stage this repository claims against one Odoo
# database and prints one OK / FAIL / SKIP line per stage. Exit 0 only when
# every stage that RAN passed; SKIP never fakes a pass.
#
#   --db NAME        the database (default $RUSTORM_DB, else rustorm_probe);
#                    the fixture stage COMMITS seed rows, so the name must
#                    contain rustorm, scratch or probe unless RUSTORM_ALLOW_SEED=1
#   --build MODULES  create/install the database first (comma-separated modules)
#   --quick          skip the kernel sweep, fuzz, registry sweep, tours and soak
#   --out DIR        where logs and artifacts go (default $RUSTORM_VERIFY_OUT or a mktemp dir)
#
# environment: RUSTORM_WORKSPACE, RUSTORM_ODOO_ROOT, RUSTORM_ODOO_CONF, RUSTORM_PYTHON,
#   RUSTORM_DSN / RUSTORM_PGHOST / RUSTORM_PGUSER (also honoured by the psql calls here),
#   RUSTORM_STAGE_TIMEOUT (seconds per stage, default 1800; expiry is a FAIL),
#   RUSTORM_REPLAY (capture file), RUSTORM_TOUR_TAGS, RUSTORM_FUZZ_SEEDS,
#   RUSTORM_SOAK_THREADS / RUSTORM_SOAK_SECONDS / RUSTORM_SOAK_RSS_GROWTH,
#   RUSTORM_MIN_COMPARED_{SWEEP,CORPUS,FUZZ}, RUSTORM_OTHER_DB

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${RUSTORM_WORKSPACE:-$(cd "$ROOT/.." && pwd)}"
DB="${RUSTORM_DB:-rustorm_probe}"
BUILD=""
QUICK=0
OUT="${RUSTORM_VERIFY_OUT:-$(mktemp -d -t rustorm-verify-XXXXXX)}"
STAGE_TIMEOUT="${RUSTORM_STAGE_TIMEOUT:-1800}"

while [ $# -gt 0 ]; do
  case "$1" in
    --db)    DB="$2"; shift 2 ;;
    --build) BUILD="$2"; shift 2 ;;
    --quick) QUICK=1; shift ;;
    --out)   OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

export RUSTORM_DB="$DB"
export RUSTORM_ODOO_CONF="${RUSTORM_ODOO_CONF:-$WORKSPACE/p314o19m.conf}"
export RUSTORM_HARNESS="$ROOT/harness"
export RUSTORM_VERIFY_OUT="$OUT"
ODOO="${RUSTORM_ODOO_ROOT:-$WORKSPACE/odoo}"
PY="${RUSTORM_PYTHON:-$WORKSPACE/p314o19m/bin/python}"
mkdir -p "$OUT"

# The addon arms for the ONE database `rust_engine_db` names, and a workspace
# conf that has been armed for a deployment names THAT one -- not the database
# this battery was pointed at. Every stage needing a routed call then gates on
# "another database" and reports routed=0: measured on p314o19m.conf armed for
# `rustorm_5e_scale`, that is `replay gate controls`, `load into odoo` and
# `copy encoder` failing for a reason that has nothing to do with the code.
# The tours stage already derived its own conf for exactly this; the rest of
# the battery gets the same treatment here rather than five stages on.
if grep -qE '^rust_engine_db *=' "$RUSTORM_ODOO_CONF"; then
  armed_db=$(sed -nE 's/^rust_engine_db *= *//p' "$RUSTORM_ODOO_CONF" | head -1)
  if [ "$armed_db" != "$DB" ]; then
    {
      grep -vE '^rust_engine_db *=' "$RUSTORM_ODOO_CONF"
      printf 'rust_engine_db = %s\n' "$DB"
    } > "$OUT/verify.conf"
    echo "  note: the conf arms rust_engine for '$armed_db'; this run uses a copy armed for '$DB'"
    export RUSTORM_ODOO_CONF="$OUT/verify.conf"
  fi
fi

# every stage runs under a deadline; rc 124 is what `timeout` returns on expiry
T=(timeout -k 15 "$STAGE_TIMEOUT")
timed_out() { [ "$1" = 124 ] || [ "$1" = 137 ]; }
expired="timed out after ${STAGE_TIMEOUT}s"

declare -a NAMES RESULTS NOTES
stage() { NAMES+=("$1"); RESULTS+=("$2"); NOTES+=("${3:-}"); printf '  %-22s %s %s\n' "$1" "$2" "${3:-}"; }

# odoo-bin shell reads the script on stdin; runpy gives it a __file__ so the
# harness scripts can find _env.py beside themselves
shell_script() {
  local script="$1"; shift
  "${T[@]}" "$PY" "$ODOO/odoo-bin" shell -c "$RUSTORM_ODOO_CONF" -d "$DB" --no-http --db_maxconn=8 "$@" \
    <<< "import runpy; runpy.run_path('$script', init_globals={'env': env}, run_name='__main__')"
}

pg() {
  if [ -n "${RUSTORM_DSN:-}" ]; then
    psql "$("$PY" -c "import sys; sys.path.insert(0, '$ROOT/harness'); from _env import dsn_for; print(dsn_for('$DB'))")" "$@"
  else
    psql -h "${RUSTORM_PGHOST:-/var/run/postgresql}" -U "${RUSTORM_PGUSER:-$USER}" -d "$DB" "$@"
  fi
}

free_port() {
  "$PY" - "$@" <<'EOF'
import socket, sys
lo, hi = int(sys.argv[1]), int(sys.argv[2])
for port in range(lo, hi + 1):
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))
    except OSError:
        continue
    else:
        print(port)
        break
    finally:
        s.close()
else:
    sys.exit(1)
EOF
}

diff_stage() {
  local name="$1" exp="$2" act="$3" floor="$4" json="$5"
  local out line
  out=$("$PY" "$ROOT/harness/diff.py" "$exp" "$act" --min-compared="$floor" --json="$json" 2>&1)
  line=$(printf '%s\n' "$out" | grep -E '^(PASS|FAIL|REFUSING)' | head -1)
  case "$line" in PASS*) stage "$name" OK "$line" ;;
                  *)     stage "$name" FAIL "${line:-$(printf '%s' "$out" | tail -1)}" ;; esac
}

echo "verifying '$DB'   (artifacts in $OUT)"
[ -x "$PY" ] || { echo "no interpreter at $PY (set RUSTORM_PYTHON)"; exit 2; }

if [ -n "$BUILD" ]; then
  "${T[@]}" "$PY" "$ODOO/odoo-bin" -c "$RUSTORM_ODOO_CONF" -d "$DB" -i "$BUILD" \
       --db_maxconn=8 --stop-after-init --no-http > "$OUT/install.log" 2>&1
  rc=$?
  if [ "$rc" = 0 ]; then stage "install($BUILD)" OK
  elif timed_out "$rc"; then stage "install($BUILD)" FAIL "$expired"
  else stage "install($BUILD)" FAIL "see $OUT/install.log"; fi
fi

out=$("${T[@]}" "$PY" "$ROOT/harness/fork_contract.py" 2>&1); rc=$?
case "$rc" in
  0) stage "fork contract" OK "$(printf '%s' "$out" | grep -E '^FORK' | head -1)" ;;
  3) stage "fork contract" SKIP "$(printf '%s' "$out" | grep -E '^FORK' | head -1)" ;;
  *) if timed_out "$rc"; then stage "fork contract" FAIL "$expired"
     else stage "fork contract" FAIL "$(printf '%s' "$out" | grep -E '^ *FAIL|^FORK' | head -1)"; fi ;;
esac

PYMOD="$OUT/pymod"
if [ -f "$ROOT/target/release/libengine_py.so" ]; then
  mkdir -p "$PYMOD"
  cp "$ROOT/target/release/libengine_py.so" "$PYMOD/engine_py.so"
  out=$(PYTHONPATH="$PYMOD" "${T[@]}" "$PY" "$ROOT/harness/test_shims.py" 2>&1); rc=$?
  if [ "$rc" = 0 ]; then
    stage "shim units" OK "$(printf '%s' "$out" | grep -E '^SHIMS' | head -1)"
  elif timed_out "$rc"; then stage "shim units" FAIL "$expired"
  else
    stage "shim units" FAIL "$(printf '%s' "$out" | grep -E '^ *FAIL|^SHIMS' | head -1)"
  fi
else
  stage "shim units" SKIP "no libengine_py.so; cargo build --release"
fi

# the sweep generator also seeds the fixture every other stage depends on and
# COMMITS it (more identities and a company, rules, archived corecords): a
# database that is not named as scratch is refused rather than modified
case "$DB" in
  *rustorm*|*scratch*|*probe*) ;;
  *) [ "${RUSTORM_ALLOW_SEED:-}" = 1 ] || {
       echo "refusing: '$DB' is not named as a scratch database (rustorm/scratch/probe) and the fixture stage writes to it; set RUSTORM_ALLOW_SEED=1 to override" >&2
       exit 2; } ;;
esac
if [ -f "$PYMOD/engine_py.so" ]; then
  PYTHONPATH="$PYMOD" "${T[@]}" "$PY" "$ROOT/harness/runtime_contract.py" > "$OUT/runtime_contract.log" 2>&1; rc=$?
  if [ "$rc" = 0 ]; then stage "runtime contracts" OK
  elif timed_out "$rc"; then stage "runtime contracts" FAIL "$expired"
  else stage "runtime contracts" FAIL "see $OUT/runtime_contract.log"; fi
else
  stage "runtime contracts" FAIL "build the native extension first"
fi

RUSTORM_SWEEP_OUT="$OUT/sweep_corpus.json" shell_script "$ROOT/harness/sweep_corpus.py" > "$OUT/sweep_gen.log" 2>&1; rc=$?
if [ "$rc" = 0 ]; then
  stage "fixture + sweep corpus" OK "$(grep -aoE 'wrote [0-9]+ cases' "$OUT/sweep_gen.log" | tail -1), $(grep -aoE 'identities: .*' "$OUT/sweep_gen.log" | tail -1)"
  SWEEP_CORPUS_OK=1
elif timed_out "$rc"; then stage "fixture + sweep corpus" FAIL "$expired"; SWEEP_CORPUS_OK=0
else
  stage "fixture + sweep corpus" FAIL "see $OUT/sweep_gen.log"; SWEEP_CORPUS_OK=0; fi

# the export is taken AFTER seeding: the fixture adds fields (restricted
# custom fields on res.country) the kernel must know about
"${T[@]}" "$ROOT/target/release/export_registry" "$OUT/export.json" > "$OUT/export.log" 2>&1; rc=$?
if [ "$rc" = 0 ]; then
  stage "registry export" OK "$(grep -o 'exported [0-9]* models' "$OUT/export.log" | head -1)"
elif timed_out "$rc"; then stage "registry export" FAIL "$expired"
else
  stage "registry export" FAIL "see $OUT/export.log"; fi


PYTHONPATH="$PYMOD" RUSTORM_EXPORT="$OUT/export.json" "${T[@]}" "$PY" "$ROOT/harness/replay_contract.py" > "$OUT/replay_contract.log" 2>&1; rc=$?
if [ "$rc" = 0 ]; then stage "replay gate controls" OK
elif timed_out "$rc"; then stage "replay gate controls" FAIL "$expired"
else stage "replay gate controls" FAIL "see $OUT/replay_contract.log"; fi


if [ "$QUICK" = 1 ]; then
  stage "kernel sweep" SKIP "--quick"
elif [ "$SWEEP_CORPUS_OK" = 1 ]; then
  RUSTORM_CORPUS="$OUT/sweep_corpus.json" RUSTORM_EXPECTED="$OUT/sweep_expected.json" \
    shell_script "$ROOT/harness/gen_expected.py" > "$OUT/sweep_exp.log" 2>&1 \
    || echo "gen_expected exited $?" >> "$OUT/sweep_exp.log"
  "${T[@]}" "$ROOT/target/release/rustorm" --db "$DB" --export "$OUT/export.json" \
      run-corpus --file "$OUT/sweep_corpus.json" > "$OUT/sweep_actual.json" 2> "$OUT/sweep_run.log" \
    || echo "run-corpus exited $?" >> "$OUT/sweep_run.log"
  diff_stage "kernel sweep" "$OUT/sweep_expected.json" "$OUT/sweep_actual.json" \
    "${RUSTORM_MIN_COMPARED_SWEEP:-300}" "$OUT/sweep_diff.json"
else
  stage "kernel sweep" FAIL "corpus generation failed; see $OUT/sweep_gen.log"; fi

RUSTORM_EXPECTED="$OUT/expected.json" shell_script "$ROOT/harness/gen_expected.py" > "$OUT/gen.log" 2>&1; rc=$?
if [ "$rc" = 0 ]; then
  "${T[@]}" "$ROOT/target/release/rustorm" --db "$DB" --export "$OUT/export.json" \
      run-corpus --file "$ROOT/harness/corpus.json" > "$OUT/actual.json" 2> "$OUT/corpus.log"
  diff_stage "shadow corpus" "$OUT/expected.json" "$OUT/actual.json" \
    "${RUSTORM_MIN_COMPARED_CORPUS:-150}" "$OUT/corpus_diff.json"
elif timed_out "$rc"; then stage "shadow corpus" FAIL "$expired"
else
  stage "shadow corpus" FAIL "baseline generation failed; see $OUT/gen.log"; fi

# phase 1 compares one res.partner read against expected.json, so it runs before
# the fuzz and registry sweeps, whose seeding creates partners that baseline lacks
RUSTORM_EXPECTED="$OUT/expected.json" "${T[@]}" "$ROOT/target/release/phase1_shell" > "$OUT/phase1.log" 2>&1; rc=$?
if [ "$rc" = 0 ]; then stage "hybrid (phase 1)" OK
elif timed_out "$rc"; then stage "hybrid (phase 1)" FAIL "$expired"
else stage "hybrid (phase 1)" FAIL "see $OUT/phase1.log"; fi

if [ "$QUICK" = 1 ]; then
  stage "fuzz" SKIP "--quick"
else
  fuzz_fail=0; fuzz_note=""
  for seed in ${RUSTORM_FUZZ_SEEDS:-1 2 3}; do
    RUSTORM_FUZZ_OUT="$OUT/fuzz_$seed.json" RUSTORM_FUZZ_SEED="$seed" \
      shell_script "$ROOT/harness/fuzz_corpus.py" > "$OUT/fuzz_gen_$seed.log" 2>&1 \
      || { fuzz_fail=1; fuzz_note="$fuzz_note seed$seed:GENFAIL"; continue; }
    RUSTORM_CORPUS="$OUT/fuzz_$seed.json" RUSTORM_EXPECTED="$OUT/fuzz_exp_$seed.json" \
      shell_script "$ROOT/harness/gen_expected.py" > "$OUT/fuzz_expgen_$seed.log" 2>&1 \
      || { fuzz_fail=1; fuzz_note="$fuzz_note seed$seed:EXPFAIL"; continue; }
    "${T[@]}" "$ROOT/target/release/rustorm" --db "$DB" --export "$OUT/export.json" \
        run-corpus --file "$OUT/fuzz_$seed.json" > "$OUT/fuzz_act_$seed.json" 2> "$OUT/fuzz_run_$seed.log" \
      || { fuzz_fail=1; fuzz_note="$fuzz_note seed$seed:RUNFAIL"; continue; }
    line=$("$PY" "$ROOT/harness/diff.py" "$OUT/fuzz_exp_$seed.json" "$OUT/fuzz_act_$seed.json" \
           --min-compared="${RUSTORM_MIN_COMPARED_FUZZ:-50}" --json="$OUT/fuzz_diff_$seed.json" 2>&1 \
           | grep -E '^(PASS|FAIL|REFUSING)' | head -1)
    case "$line" in PASS*) ;; *) fuzz_fail=1 ;; esac
    fuzz_note="$fuzz_note seed$seed:${line%% *}"
  done
  if [ "$fuzz_fail" = 0 ]; then stage "fuzz" OK "$fuzz_note"
  else stage "fuzz" FAIL "$fuzz_note (see $OUT/fuzz_*.json)"; fi
fi

if [ "$QUICK" = 1 ]; then
  stage "registry sweep" SKIP "--quick"
else
  "${T[@]}" "$ROOT/target/release/phase2_verify" > "$OUT/sweep.log" 2>&1; rc=$?
  if [ "$rc" = 0 ]; then
    stage "registry sweep" OK "$(grep -ao 'query shapes compared: .*' "$OUT/sweep.log" | head -1)"
  elif timed_out "$rc"; then stage "registry sweep" FAIL "$expired"
  else stage "registry sweep" FAIL "$(grep -ac '^ *MISMATCH' "$OUT/sweep.log") MISMATCH lines: $(grep -ao '^ *MISMATCH.*' "$OUT/sweep.log" | head -1 | cut -c5-90)"; fi
fi

"${T[@]}" "$ROOT/target/release/phase2_tests" > "$OUT/upstream.log" 2>&1; rc=$?
if [ "$rc" = 0 ]; then stage "upstream suites" OK
elif timed_out "$rc"; then stage "upstream suites" FAIL "$expired"
else stage "upstream suites" FAIL "$(grep -a 'APPEARED under routing' "$OUT/upstream.log" | head -1)"; fi

probe() {
  local mode="$1" want="$2" name="$3"
  PYTHONUNBUFFERED=1 "${T[@]}" "$ROOT/target/release/probe_audit" "$mode" "${@:4}" \
       > "$OUT/probe_$mode.log" 2>&1
  local rc=$?
  if [ "$rc" = 0 ]; then
    if grep -q "$want" "$OUT/probe_$mode.log"; then stage "$name" OK "$(grep -o "$want.*" "$OUT/probe_$mode.log" | head -1)"
    else stage "$name" FAIL "$(grep -a MISMATCH "$OUT/probe_$mode.log" | head -1 | cut -c1-70)"; fi
  elif timed_out "$rc"; then stage "$name" FAIL "$expired"
  else
    # a probe that did not finish is not a probe that could not run: the
    # database is the same one every other stage reached
    stage "$name" FAIL "probe_audit exited non-zero; see $OUT/probe_$mode.log"; fi
}
probe types "mismatches=0" "cursor type layer"
probe race  "wrong=0"      "concurrency"       "$OUT/export.json"

if [ -f "$ROOT/target/release/libengine_py.so" ]; then
  PYTHONPATH="$PYMOD" RUSTORM_EXPORT="$OUT/export.json" RUSTORM_ROUTE=shadow \
     RUSTORM_OTHER_DB="${RUSTORM_OTHER_DB:-}" \
     shell_script "$ROOT/harness/load_into_odoo.py" > "$OUT/load.log" 2>&1; rc=$?
  if [ "$rc" = 0 ]; then
    stage "load into odoo" OK "$(grep -acE '^LOAD (read|other db|fork exit)' "$OUT/load.log") checks"
  elif timed_out "$rc"; then stage "load into odoo" FAIL "$expired"
  else
    stage "load into odoo" FAIL "$(grep -aE '^LOAD|Error' "$OUT/load.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "load into odoo" SKIP "no libengine_py.so; cargo build --release"
fi


TOUR_TAGS="${RUSTORM_TOUR_TAGS:-/mail:TestDiscussChannelExpand,/mail:TestMailActivityChatter,/mail:TestMailComposerUI,/mail:TestMailTemplateUI,/mail:TestUserTours,/web:TestFavorite,/web:TestUserSwitch,/base:TestIrModelFieldsTranslation}"
if [ "$QUICK" = 1 ]; then
  stage "web tours (shadow)" SKIP "--quick"
elif [ ! -f "$ROOT/target/release/libengine_py.so" ]; then
  stage "web tours (shadow)" SKIP "no libengine_py.so; cargo build --release"
elif ! pg -tAc "select 1 from ir_module_module where name='mail' and state='installed'" 2>/dev/null | grep -q 1; then
  stage "web tours (shadow)" SKIP "mail is not installed on $DB (set RUSTORM_TOUR_TAGS for another set)"
else
  TOUR_PORT="$(free_port 9401 9449 || echo 9401)"
  {
    grep -vE '^(addons_path|server_wide_modules|http_port|logfile|rust_engine_[a-z_]+) *=' "$RUSTORM_ODOO_CONF"
    printf 'addons_path = %s,%s/addons\n' "$(grep -E '^addons_path *=' "$RUSTORM_ODOO_CONF" | sed 's/^addons_path *= *//')" "$ROOT"
    printf 'server_wide_modules = base,web,rust_engine\nrust_engine_db = %s\nrust_engine_mode = shadow\nrust_engine_report_seconds = 15\nrust_engine_capture = %s\nhttp_port = %s\n' "$DB" "$OUT/tours_capture.jsonl" "$TOUR_PORT"
  } > "$OUT/tours.conf"
  PYTHONPATH="$PYMOD" "${T[@]}" "$PY" "$ODOO/odoo-bin" -c "$OUT/tours.conf" -d "$DB" --test-enable \
      --test-tags "$TOUR_TAGS" --stop-after-init > "$OUT/tours.log" 2>&1
  tours_rc=$?
  diverged=$(grep -ac "SHADOW DIVERGENCE" "$OUT/tours.log")
  result=$(grep -ao "[0-9]* failed, [0-9]* error(s) of [0-9]* tests" "$OUT/tours.log" | tail -1)
  report=$(grep -ao "routed=[0-9]* share=[0-9.]* gate=[0-9]*" "$OUT/tours.log" | tail -1)
  if [ "$tours_rc" = 0 ] && [ "$diverged" = 0 ] && [[ "$report" =~ routed=[1-9][0-9]* ]]; then
    stage "web tours (shadow)" OK "$result; $report; divergences 0"
  elif timed_out "$tours_rc"; then stage "web tours (shadow)" FAIL "$expired"
  else
    stage "web tours (shadow)" FAIL "${result:-no result line} rc=$tours_rc divergences=$diverged; see $OUT/tours.log"
  fi
fi

# the tours' own traffic is the default capture: replaying it after the run
# is the second pass over exactly the calls the web client made
REPLAY_FILE="${RUSTORM_REPLAY:-}"
if [ -z "$REPLAY_FILE" ] && [ -s "$OUT/tours_capture.jsonl" ]; then REPLAY_FILE="$OUT/tours_capture.jsonl"; fi
if [ -n "$REPLAY_FILE" ] && [ -f "$ROOT/target/release/libengine_py.so" ]; then
  PYTHONPATH="$PYMOD" RUSTORM_EXPORT="$OUT/export.json" RUSTORM_ROUTE=shadow \
     RUSTORM_REPLAY="$REPLAY_FILE" \
     shell_script "$ROOT/harness/replay.py" > "$OUT/replay.log" 2>&1; rc=$?
  case "$rc" in
    0) stage "replay" OK "$(grep -aE '^REPLAY (OK|DIVERGED)' "$OUT/replay.log" | tail -1 | cut -c8-)" ;;
    3) stage "replay" SKIP "$(grep -aE '^REPLAY SKIP' "$OUT/replay.log" | tail -1 | cut -c8-)" ;;
    *) if timed_out "$rc"; then stage "replay" FAIL "$expired"
       else stage "replay" FAIL "$(grep -aE '^REPLAY|Error' "$OUT/replay.log" | tail -1 | cut -c1-70)"; fi ;;
  esac
else
  stage "replay" SKIP "no capture: the tours did not run (set RUSTORM_REPLAY for another file)"
fi

if [ -f "$ROOT/target/release/libengine_py.so" ]; then
  PYTHONPATH="$PYMOD" shell_script "$ROOT/harness/copy_path.py" > "$OUT/copy.log" 2>&1; rc=$?
  if [ "$rc" = 0 ]; then
    stage "copy encoder" OK "$(grep -a '^COPY streams' "$OUT/copy.log" | head -1 | cut -c1-58)"
  elif timed_out "$rc"; then stage "copy encoder" FAIL "$expired"
  else
    stage "copy encoder" FAIL "$(grep -aE '^ *COPY MISMATCH|^COPY ' "$OUT/copy.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "copy encoder" SKIP "no libengine_py.so; cargo build --release"
fi

# The write path at the persistence port. Like the copy encoder above it is
# differential against psycopg rather than against a recorded expectation,
# because a corrupted write is equally corrupt on both sides of a read
# comparison -- and it counts the statements the port answered natively, so a
# run where everything delegated cannot report a clean comparison of two
# identical Python writes.
if [ -f "$ROOT/target/release/libengine_py.so" ]; then
  PYTHONPATH="$PYMOD" shell_script "$ROOT/harness/write_path.py" > "$OUT/write.log" 2>&1; rc=$?
  if [ "$rc" = 0 ]; then
    stage "write path (port)" OK "$(grep -a '^WRITE native update_rows' "$OUT/write.log" | head -1 | cut -c1-70)"
  elif timed_out "$rc"; then stage "write path (port)" FAIL "$expired"
  else
    stage "write path (port)" FAIL "$(grep -aE '^ *WRITE MISMATCH|^WRITE ' "$OUT/write.log" | tail -1 | cut -c1-70)"
  fi
else
  stage "write path (port)" SKIP "no libengine_py.so; cargo build --release"
fi

# The statement the port composes, against the statement the FORK composes,
# with no database involved: `test_shims.py` and `kernel/tests/pure.rs` each
# derive the contract file independently, and this is the half that asks Odoo.
out=$("${T[@]}" "$PY" "$ROOT/harness/update_sql_contract.py" 2>&1); rc=$?
if [ "$rc" = 0 ]; then
  stage "update sql contract" OK "$(printf '%s' "$out" | grep -E '^CONTRACT' | head -1)"
elif timed_out "$rc"; then stage "update sql contract" FAIL "$expired"
else
  stage "update sql contract" FAIL "$(printf '%s' "$out" | grep -E '^CONTRACT' | head -1)"
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
  out=$(RUSTORM_PARITY_DIR="$OUT/parity" "${T[@]}" "$ROOT/harness/parity.sh" \
          --db "$DB" --password "${RUSTORM_ADMIN_PASSWORD:-admin}" \
          --port "${RUSTORM_PARITY_PORT:-$(free_port 8140 8180)}" 2>&1)
  verdict=$(printf '%s\n' "$out" | grep -aE '^PARITY (OK|FAILED|VACUOUS)' | head -1)
  counts=$(printf '%s\n' "$out" | grep -aE '^ *cases ' | head -1 | sed 's/^ *//')
  case "$verdict" in
    "PARITY OK") stage "byte parity" OK "$counts" ;;
    PARITY*)     stage "byte parity" FAIL "$verdict" ;;
    # Missing evidence cannot pass: authentication and timeout failures
    # may stop the run before it can print a verdict.
    *)           stage "byte parity" FAIL "$(printf '%s\n' "$out" | grep -aE 'FAILED|failed' | tail -1 | cut -c1-70)" ;;
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
  out=$(RUSTORM_CURSOR_DIR="$OUT/cursor" "${T[@]}" "$ROOT/harness/cursor_parity.sh" --db "$DB" 2>&1)
  verdict=$(printf '%s\n' "$out" | grep -aE '^CURSOR PARITY (OK|FAILED|VACUOUS)' | head -1)
  counts=$(printf '%s\n' "$out" | grep -aE '^ *ran psycopg=' | head -1 | sed 's/^ *//')
  case "$verdict" in
    "CURSOR PARITY OK"*) stage "cursor parity" OK "$counts" ;;
    CURSOR*)             stage "cursor parity" FAIL "$verdict" ;;
    *)                   stage "cursor parity" FAIL "$(printf '%s\n' "$out" | tail -1 | cut -c1-70)" ;;
  esac
fi

if [ "$QUICK" = 1 ]; then
  stage "soak" SKIP "--quick"
else
  # Earlier lanes commit fixtures and login logs. Compare against Python on
  # that final state, not the pre-fuzz baseline used by phase 1.
  RUSTORM_EXPECTED="$OUT/soak_expected.json" shell_script "$ROOT/harness/gen_expected.py" > "$OUT/soak_gen.log" 2>&1
  baseline_rc=$?
  "${T[@]}" "$ROOT/target/release/export_registry" "$OUT/soak_export.json" > "$OUT/soak_export.log" 2>&1
  export_rc=$?
  if [ "$baseline_rc" != 0 ] || [ "$export_rc" != 0 ]; then
    stage "soak baseline" FAIL "could not refresh Python baseline and registry"
  fi
  SOAK_PORT="${RUSTORM_SOAK_PORT:-$(free_port 9450 9499 || echo 9450)}"
  # the same rule cases.py applies to a corpus uid of "other": the lowest
  # active user that is neither OdooBot nor the administrator
  OTHER_UID="$(pg -tAc "select min(id) from res_users where active and id not in (1, 2)" 2>/dev/null | tr -d '[:space:]')"
  SOAK_TOKEN="$("$PY" -c 'import secrets; print(secrets.token_hex(16))')"
  RUSTORM_SERVE_TOKEN="$SOAK_TOKEN" "$ROOT/target/release/rustorm" --db "$DB" --export "$OUT/soak_export.json" \
      serve --port "$SOAK_PORT" > "$OUT/soak_serve.log" 2>&1 &
  soak_pid=$!
  soak_up=0
  echo "  (soak server on port $SOAK_PORT)" >&2
  for _ in $(seq 1 30); do
    curl -sf -m 2 "http://127.0.0.1:$SOAK_PORT/health" >/dev/null 2>&1 && { soak_up=1; break; }
    kill -0 "$soak_pid" 2>/dev/null || break     # it exited; stop waiting for it
    sleep 1
  done
  soak_args=(--port "$SOAK_PORT" --threads "${RUSTORM_SOAK_THREADS:-8}" --seconds "${RUSTORM_SOAK_SECONDS:-20}")
  if [ -n "$OTHER_UID" ]; then
    soak_args+=(--uids "$OTHER_UID" --other-uid "$OTHER_UID")
    soak_note="model probes at uid $OTHER_UID (other), corpus baseline at every identity"
  else
    soak_note="no other identity on $DB: model probes at uid 2"
  fi
  RUSTORM_SERVE_TOKEN="$SOAK_TOKEN" RUSTORM_EXPECTED="$OUT/soak_expected.json" \
    "${T[@]}" "$PY" "$ROOT/harness/soak.py" "${soak_args[@]}" > "$OUT/soak.log" 2>&1; rc=$?
  if [ "$rc" = 0 ]; then
    stage "soak" OK "$(grep -o '[0-9]* requests in .*' "$OUT/soak.log" | head -1); $soak_note"
  elif timed_out "$rc"; then stage "soak" FAIL "$expired"
  else
    stage "soak" FAIL "$(grep -aE 'MISMATCH|error:|SOAK|GREW' "$OUT/soak.log" | head -1 | cut -c1-70)"
  fi
  kill -TERM "$soak_pid" 2>/dev/null
  wait "$soak_pid" 2>/dev/null
fi

fail=0
for r in "${RESULTS[@]}"; do [ "$r" = FAIL ] && fail=1; done
echo
if [ "$fail" = 0 ]; then echo "VERIFY OK   ($DB)"; else echo "VERIFY FAILED   ($DB)"; fi
exit "$fail"
