#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ODOO="${RUSTORM_ODOO_ROOT:-${RUSTORM_ODOO:-$(cd "$ROOT/../odoo" && pwd)}}"
PY="${RUSTORM_PYTHON:-$(cd "$ROOT/.." && pwd)/p314o19m/bin/python}"
CONF="${RUSTORM_ODOO_CONF:-$(cd "$ROOT/.." && pwd)/p314o19m.conf}"

DB=""; MODE=""; ROWS=120000; PASSWORD=""; ONLY=""; CONTINUE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --db)       DB="$2"; shift 2 ;;
    --rows)     ROWS="$2"; shift 2 ;;
    --password) PASSWORD="$2"; shift 2 ;;

    --modules)  ONLY="$2"; shift 2 ;;
    --volume|--scale) MODE="${1#--}"; shift ;;
    --continue) CONTINUE=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$DB" ] && [ -n "$MODE" ] || { echo "--db and one of --volume/--scale are required" >&2; exit 2; }

if psql -U "${USER:-marin}" -lqt 2>/dev/null | cut -d'|' -f1 | grep -qw "$DB"; then
  if [ "$CONTINUE" = 1 ]; then
    echo "== continuing on the existing $DB =="
  else
    echo "refusing: database '$DB' already exists; drop it first (or pass --continue)" >&2
    exit 2
  fi
fi

if [ -z "${ODOO_API_ENCRYPTION_KEY:-}" ]; then
  export ODOO_API_ENCRYPTION_KEY
  ODOO_API_ENCRYPTION_KEY="$("$PY" -c 'from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())')"
  echo "== ODOO_API_ENCRYPTION_KEY generated for this fixture: $ODOO_API_ENCRYPTION_KEY =="
fi

odoo() { "$PY" "$ODOO/odoo-bin" -c "$CONF" -d "$DB" --no-http --stop-after-init "$@"; }

if [ "$CONTINUE" != 1 ]; then
  echo "== creating $DB with base =="
  odoo -i base > /dev/null 2>&1
fi

if [ "$MODE" = scale ]; then

  prev=-1
  broken=""
  for round in 1 2 3 4 5 6 7 8 9 10; do
    filter=""
    if [ -n "$ONLY" ]; then
      filter=" and name = any(string_to_array('$ONLY', ','))"
    fi
    if [ -n "$broken" ]; then
      filter="$filter and name <> all(string_to_array('$broken', ','))"
    fi
    installable="with recursive bad as (
        select m.name from ir_module_module m
          join ir_module_module_dependency d on d.module_id = m.id
         where m.state = 'uninstallable'
            or d.name not in (select name from ir_module_module)
            or d.name = any(string_to_array('$broken', ','))
        union
        select m.name from ir_module_module m
          join ir_module_module_dependency d on d.module_id = m.id
          join bad on bad.name = d.name)
      select name from ir_module_module
       where state = 'uninstalled'$filter and name not in (select name from bad)"
    todo=$(psql -U "${USER:-marin}" -d "$DB" -tAc \
      "select string_agg(name, ',') from ($installable) t")
    count=$(psql -U "${USER:-marin}" -d "$DB" -tAc \
      "select count(*) from ($installable) t")
    installed=$(psql -U "${USER:-marin}" -d "$DB" -tAc \
      "select count(*) from ir_module_module where state = 'installed'")
    echo "== round $round: $installed installed, $count uninstalled =="
    [ -n "$todo" ] || break
    [ "$count" != "$prev" ] || { echo "== converged =="; break; }
    prev=$count

    log="${RUSTORM_FIXTURE_LOG_DIR:-${TMPDIR:-/tmp}}/fixture-$DB-round$round.log"
    odoo -i "$todo" > "$log" 2>&1 || true
    if grep -qE 'ERROR|CRITICAL' "$log"; then
      echo "== round $round logged errors; first one, full log at $log =="
      grep -m1 -E 'ERROR|CRITICAL' "$log"
      culprit=$(grep -aoE 'Loading module [a-z0-9_]+' "$log" | tail -1 | awk '{print $3}')
      if [ -n "$culprit" ] && grep -q "Failed to load registry" "$log"; then
        echo "== skipping $culprit and its dependants from now on =="
        broken="${broken:+$broken,}$culprit"
        prev=-1
      fi
    fi
  done
  [ -z "$broken" ] || echo "== modules skipped because their install raised: $broken =="

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

echo "== seeding languages and users =="
"$PY" "$ODOO/odoo-bin" shell -c "$CONF" -d "$DB" --no-http --db_maxconn=4 <<PYEOF > /dev/null
if "$PASSWORD":
    env["res.users"].browse(2).write({"password": "$PASSWORD"})
env["res.lang"]._activate_lang("fr_FR")
if "account.chart.template" in env:
    for company in env["res.company"].search([("chart_template", "=", False)]):
        ChartTemplate = env["account.chart.template"].with_company(company)
        code = ChartTemplate._guess_chart_template(company.country_id) or "generic_coa"
        ChartTemplate.try_loading(code, company)
Users = env["res.users"]
if not Users.search([("id", "not in", [1, 2]), ("active", "=", True)], limit=1):
    Users.create({"login": "rustorm_other", "name": "RUSTORM other",
                  "group_ids": [(6, 0, [env.ref("base.group_user").id])]})
env.cr.commit()
PYEOF

echo
psql -U "${USER:-marin}" -d "$DB" -tAc \
  "select 'FIXTURE $DB: '||(select count(*) from ir_module_module where state='installed')||' modules, '
        ||(select count(*) from ir_model)||' models, '
        ||(select count(*) from res_partner)||' partners, '
        ||pg_size_pretty(pg_database_size('$DB'))"
echo "FIXTURE OK  (admin password: ${PASSWORD:-admin, as created; the web tours type it})"
