#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ODOO="${RUSTPOC_ODOO:-$(cd "$ROOT/../odoo" && pwd)}"
PY="${RUSTPOC_PYTHON:-$(cd "$ROOT/.." && pwd)/p314o19m/bin/python}"
CONF="${RUSTPOC_ODOO_CONF:-$(cd "$ROOT/.." && pwd)/p314o19m.conf}"

DB=""; MODE=""; ROWS=120000; PASSWORD="kernelprobe"; ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --db)       DB="$2"; shift 2 ;;
    --rows)     ROWS="$2"; shift 2 ;;
    --password) PASSWORD="$2"; shift 2 ;;

    --modules)  ONLY="$2"; shift 2 ;;
    --volume|--scale) MODE="${1#--}"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$DB" ] && [ -n "$MODE" ] || { echo "--db and one of --volume/--scale are required" >&2; exit 2; }

if psql -U "${USER:-marin}" -lqt 2>/dev/null | cut -d'|' -f1 | grep -qw "$DB"; then
  echo "refusing: database '$DB' already exists; drop it first" >&2
  exit 2
fi

if [ -z "${ODOO_API_ENCRYPTION_KEY:-}" ]; then
  export ODOO_API_ENCRYPTION_KEY
  ODOO_API_ENCRYPTION_KEY="$("$PY" -c 'from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())')"
  echo "== ODOO_API_ENCRYPTION_KEY generated for this fixture: $ODOO_API_ENCRYPTION_KEY =="
fi

odoo() { "$PY" "$ODOO/odoo-bin" -c "$CONF" -d "$DB" --no-http --stop-after-init "$@"; }

echo "== creating $DB with base =="
odoo -i base > /dev/null 2>&1

if [ "$MODE" = scale ]; then

  prev=-1
  for round in 1 2 3 4 5 6; do
    filter=""
    if [ -n "$ONLY" ]; then
      filter=" and name = any(string_to_array('$ONLY', ','))"
    fi
    todo=$(psql -U "${USER:-marin}" -d "$DB" -tAc \
      "select string_agg(name, ',') from ir_module_module where state = 'uninstalled'$filter")
    count=$(psql -U "${USER:-marin}" -d "$DB" -tAc \
      "select count(*) from ir_module_module where state = 'uninstalled'$filter")
    installed=$(psql -U "${USER:-marin}" -d "$DB" -tAc \
      "select count(*) from ir_module_module where state = 'installed'")
    echo "== round $round: $installed installed, $count uninstalled =="
    [ -n "$todo" ] || break
    [ "$count" != "$prev" ] || { echo "== converged =="; break; }
    prev=$count

    log="${RUSTPOC_FIXTURE_LOG_DIR:-${TMPDIR:-/tmp}}/fixture-$DB-round$round.log"
    odoo -i "$todo" > "$log" 2>&1 || true
    if grep -qE 'ERROR|CRITICAL' "$log"; then
      echo "== round $round logged errors; first one, full log at $log =="
      grep -m1 -E 'ERROR|CRITICAL' "$log"
    fi
  done

  psql -U "${USER:-marin}" -d "$DB" -tAc \
    "select 'final: '||count(*) filter (where state='installed')||' installed, '
          ||count(*) filter (where state='uninstalled')||' uninstalled, '
          ||count(*) filter (where state='uninstallable')||' uninstallable (unmet deps)'
       from ir_module_module"
fi

if [ "$MODE" = volume ]; then
  echo "== creating $ROWS partners through the ORM =="
  "$PY" "$ODOO/odoo-bin" shell -c "$CONF" -d "$DB" --no-http --db_maxconn=4 <<PYEOF
import time
Partner = env["res.partner"]
want, t0, made = $ROWS, time.time(), 0
have = Partner.search_count([])
while have + made < want:
    n = min(2000, want - have - made)
    Partner.create([
        {"name": "Bench Partner %06d" % (made + i),
         "is_company": (i % 7 == 0),
         "ref": "BP%06d" % (made + i)}
        for i in range(n)
    ])
    made += n
    if made % 20000 == 0:
        env.cr.commit()
env.cr.commit()
print("VOL %d partners in %.0fs" % (Partner.search_count([]), time.time() - t0))
PYEOF

  psql -U "${USER:-marin}" -d "$DB" -c "VACUUM ANALYZE res_partner" > /dev/null
fi

echo "== setting the admin password =="
"$PY" "$ODOO/odoo-bin" shell -c "$CONF" -d "$DB" --no-http --db_maxconn=4 <<PYEOF > /dev/null
env["res.users"].browse(2).write({"password": "$PASSWORD"})
# The corpus names a second language and a second identity ("uid": "other",
# the lowest active non-admin user) and a database with neither turns 14 of
# its cases VACUOUS: both sides fail to run them, and the stage still says OK.
env["res.lang"]._activate_lang("fr_FR")
Users = env["res.users"]
if not Users.search([("id", "not in", [1, 2]), ("active", "=", True)], limit=1):
    Users.create({"login": "rustpoc_other", "name": "RUSTPOC other",
                  "group_ids": [(6, 0, [env.ref("base.group_user").id])]})
env.cr.commit()
PYEOF

echo
psql -U "${USER:-marin}" -d "$DB" -tAc \
  "select 'FIXTURE $DB: '||(select count(*) from ir_module_module where state='installed')||' modules, '
        ||(select count(*) from ir_model)||' models, '
        ||(select count(*) from res_partner)||' partners, '
        ||pg_size_pretty(pg_database_size('$DB'))"
echo "FIXTURE OK  (admin password: $PASSWORD)"
