#!/usr/bin/env bash
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ODOO="${RUSTORM_ODOO_ROOT:-${RUSTORM_ODOO:-$(cd "$ROOT/../odoo" && pwd)}}"
PY="${RUSTORM_PYTHON:-$(cd "$ROOT/.." && pwd)/p314o19m/bin/python}"
CONF="${RUSTORM_ODOO_CONF:-$(cd "$ROOT/.." && pwd)/p314o19m.conf}"
TAGS="${RUSTORM_ORM_TEST_TAGS:-/test_orm,/test_read_group,/test_access_rights,/test_search_panel,/test_inherits}"
DB=""
while [ $# -gt 0 ]; do
  case "$1" in
    --db) DB="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$DB" ] || { echo "--db is required" >&2; exit 2; }
[ -f "$ROOT/target/release/libengine_py.so" ] || { echo "ORM TESTS SKIP: no libengine_py.so"; exit 3; }

OUT="${RUSTORM_ORM_TESTS_DIR:-$(mktemp -d "${HOME}/.cache/rustorm-orm-tests-XXXXXX")}"
mkdir -p "$OUT/pymod"
cp "$ROOT/target/release/libengine_py.so" "$OUT/pymod/engine_py.so"

base() {
  grep -vE '^(addons_path|server_wide_modules|http_port|logfile|db_maxconn|rust_engine_[a-z_]+) *=' "$CONF"
  printf 'addons_path = %s,%s/addons\n' "$(grep -E '^addons_path *=' "$CONF" | sed 's/^addons_path *= *//')" "$ROOT"
}
{ base; printf 'server_wide_modules = base,web\n'; } > "$OUT/off.conf"
{ base; printf 'server_wide_modules = base,web,rust_engine\nrust_engine_db = %s\nrust_engine_mode = on\nrust_engine_verify_sample = 0\n' "$DB"; } > "$OUT/on.conf"

failures() {
  grep -aoE '(odoo\.addons\.[a-z_]+\.tests\.[a-z_0-9]+|odoo\.tests\.suite): (FAIL|ERROR): [^ ]+( [A-Za-z_(.]+[A-Za-z_0-9)]+)?' "$1" \
    | sed -E 's/^[^:]+: //' | sort -u
}

free_port() {
  "$PY" -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

for leg in off on; do
  PYTHONPATH="$OUT/pymod" "$PY" "$ODOO/odoo-bin" -c "$OUT/$leg.conf" -d "$DB" --test-tags "$TAGS" \
    --stop-after-init --http-port "$(free_port)" --db_maxconn=16 > "$OUT/$leg.log" 2>&1
  failures "$OUT/$leg.log" > "$OUT/$leg.failures"
done

off_line=$(grep -ao '[0-9]* failed, [0-9]* error(s) of [0-9]* tests' "$OUT/off.log" | tail -1)
on_line=$(grep -ao '[0-9]* failed, [0-9]* error(s) of [0-9]* tests' "$OUT/on.log" | tail -1)
routed=$(grep -ao 'rust kernel (final): mode=on [^ ]* routed=[0-9]*' "$OUT/on.log" | tail -1 | sed 's/.*routed=//')
only_on=$(comm -13 "$OUT/off.failures" "$OUT/on.failures")
refusing=$(grep -ac "refusing to arm" "$OUT/on.log")

echo "ORM TESTS off: ${off_line:-no result}; on: ${on_line:-no result}; routed=${routed:-0} (artifacts in $OUT)"
if [ -z "$off_line" ] || [ -z "$on_line" ]; then
  echo "ORM TESTS FAILED: a leg reported no result"; exit 1
fi
if [ "$refusing" != 0 ] || [ "${routed:-0}" = 0 ]; then
  echo "ORM TESTS FAILED: the routing leg did not route; it ran python twice"; exit 1
fi
unavailable=$(grep -ac "INFRASTRUCTURE UNAVAILABLE" "$OUT/on.log")
if [ "$unavailable" != 0 ]; then
  echo "ORM TESTS FAILED: $unavailable test class(es) could not run in the routing leg"; exit 1
fi
if [ "${off_line%% of *}" != "${on_line%% of *}" ] && [ -z "$only_on" ]; then
  echo "ORM TESTS FAILED: the legs report different totals ($off_line vs $on_line) but no failure name differs"; exit 1
fi
if [ -n "$only_on" ]; then
  echo "ORM TESTS FAILED: failing only under routing:"
  printf '%s\n' "$only_on" | sed 's/^/  /'
  exit 1
fi
echo "ORM TESTS OK"
