import collections
import json
import os
import sys

import odoo
from odoo.modules.registry import Registry
from odoo.service.model import call_kw

EXPORT = os.environ.get("RUSTORM_EXPORT")
CAPTURE = os.environ.get("RUSTORM_REPLAY")
if not EXPORT or not os.path.exists(EXPORT):
    print("REPLAY SKIP: set RUSTORM_EXPORT to a registry export")
    sys.exit(0)
if not CAPTURE or not os.path.exists(CAPTURE):
    print("REPLAY SKIP: set RUSTORM_REPLAY to a capture file")
    sys.exit(0)

import engine_py  # noqa: E402

db_shim, orm_shim = engine_py.install_shims()
dbname = env.cr.dbname  # noqa: F821
conninfo = os.environ.get("RUSTORM_DSN") or (
    "host=%s user=%s dbname=%s"
    % (
        os.environ.get("RUSTORM_PGHOST", "/var/run/postgresql"),
        os.environ.get("RUSTORM_PGUSER", os.environ.get("USER", "marin")),
        dbname,
    )
)
rust_db = engine_py.RustDb(conninfo)
db_shim.RUST_DB = rust_db
db_shim.CONNINFO = conninfo
db_shim.install()
orm_shim.KERNEL = engine_py.RustKernel.build(rust_db, open(EXPORT).read())
orm_shim.set_mode("shadow")
orm_shim.install()

REPLAYED = {"search_read", "web_search_read", "search_count", "read_group", "web_read_group", "read", "web_read", "name_search", "web_name_search"}

calls = []
with open(CAPTURE) as fh:
    for line in fh:
        line = line.strip()
        if line:
            calls.append(json.loads(line))

per = collections.defaultdict(collections.Counter)
first_raise = {}
registry = Registry(dbname)
with registry.cursor() as cr:
    for call in calls:
        key = (call["model"], call["method"])
        if call["method"] not in REPLAYED:
            per[key]["skipped"] += 1
            continue
        if call["model"] not in registry:
            per[key]["missing_model"] += 1
            continue
        e = odoo.api.Environment(cr, call.get("uid") or 2, call.get("context") or {})
        before = dict(orm_shim.stats())
        try:
            call_kw(e[call["model"]], call["method"], call.get("args") or [], call.get("kwargs") or {})
        except Exception as exc:  # noqa: BLE001
            per[key]["raised"] += 1
            first_raise.setdefault(key, "%s: %s" % (type(exc).__name__, str(exc)[:160]))
        after = dict(orm_shim.stats())
        per[key]["routed"] += after["kernel"] - before["kernel"]
        per[key]["shadow_ok"] += after["shadow_ok"] - before["shadow_ok"]
        per[key]["shadow_diff"] += after["shadow_diff"] - before["shadow_diff"]
        per[key]["gate"] += after["fallback_gate"] - before["fallback_gate"]
        per[key]["error"] += after["fallback_error"] - before["fallback_error"]
        per[key]["calls"] += 1
        cr.rollback()

total = collections.Counter()
for key in sorted(per):
    c = per[key]
    total.update(c)
    print(
        "REPLAY %-40s calls=%-5d routed=%-5d ok=%-5d diff=%-4d gate=%-4d error=%-3d raised=%d"
        % (".".join(key), c["calls"], c["routed"], c["shadow_ok"], c["shadow_diff"],
           c["gate"], c["error"], c["raised"])
    )
    if key in first_raise:
        print("REPLAY   first raise: %s" % first_raise[key])
replayed = total["calls"]
share = (total["routed"] / replayed) if replayed else 0.0
print(
    "REPLAY %s: %d captured, %d replayed, routed share %.2f, shadow ok=%d diff=%d, gate=%d error=%d"
    % ("OK" if total["shadow_diff"] == 0 else "DIVERGED", len(calls), replayed, share,
       total["shadow_ok"], total["shadow_diff"], total["gate"], total["error"])
)
sys.exit(0 if total["shadow_diff"] == 0 else 1)
