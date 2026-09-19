import collections
import json
import os
import pathlib
import sys

_HERE = (
    str(pathlib.Path(__file__).resolve().parent) if "__file__" in globals() else None
)
sys.path.insert(
    0,
    os.environ.get("RUSTORM_HARNESS")
    or _HERE
    or os.path.join(
        os.environ.get("RUSTORM_WORKSPACE") or pathlib.Path("~/Odoo").expanduser(),
        "odoo-rust-orm",
        "harness",
    ),
)
import pathlib

from _env import dsn_for

import odoo
from odoo.modules.registry import Registry
from odoo.service.model import call_kw

EXPORT = os.environ.get("RUSTORM_EXPORT")
CAPTURE = os.environ.get("RUSTORM_REPLAY")
if not EXPORT or not pathlib.Path(EXPORT).exists():
    print("REPLAY SKIP: set RUSTORM_EXPORT to a registry export")
    sys.exit(3)
if not CAPTURE or not pathlib.Path(CAPTURE).exists():
    print("REPLAY SKIP: set RUSTORM_REPLAY to a capture file")
    sys.exit(3)

import pathlib

import engine_py

db_shim, orm_shim = engine_py.install_shims()
dbname = env.cr.dbname  # noqa: F821
conninfo = dsn_for(dbname)
rust_db = engine_py.RustDb(conninfo)
db_shim.RUST_DB = rust_db
db_shim.CONNINFO = conninfo
db_shim.install()
db_shim.set_active(True)
orm_shim.KERNEL = engine_py.RustKernel.build(
    rust_db, pathlib.Path(EXPORT).read_text(encoding="utf-8")
)
orm_shim.set_mode("shadow")
orm_shim.install()

REPLAYED = {
    "search_read",
    "web_search_read",
    "search_count",
    "read_group",
    "web_read_group",
    "read",
    "web_read",
    "name_search",
    "web_name_search",
}

calls = []
with pathlib.Path(CAPTURE).open(encoding="utf-8") as fh:
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
        uid = call.get("uid") or 2
        cr.execute("SELECT 1 FROM res_users WHERE id = %s", (uid,))
        if not cr.fetchone():
            per[key]["missing_user"] += 1
            cr.rollback()
            continue
        e = odoo.api.Environment(cr, uid, call.get("context") or {})
        before = dict(orm_shim.stats())
        unexpected_before = sum(before["errors_by_model"].values())
        try:
            call_kw(
                e[call["model"]],
                call["method"],
                call.get("args") or [],
                call.get("kwargs") or {},
            )
        except Exception as exc:
            per[key]["raised"] += 1
            first_raise.setdefault(key, "%s: %s" % (type(exc).__name__, str(exc)[:160]))
        after = dict(orm_shim.stats())
        per[key]["routed"] += after["kernel"] - before["kernel"]
        per[key]["shadow_ok"] += after["shadow_ok"] - before["shadow_ok"]
        per[key]["shadow_diff"] += after["shadow_diff"] - before["shadow_diff"]
        per[key]["gate"] += after["fallback_gate"] - before["fallback_gate"]
        per[key]["refused"] += after.get("kernel_refused", 0) - before.get(
            "kernel_refused", 0
        )
        per[key]["error"] += sum(after["errors_by_model"].values()) - unexpected_before
        per[key]["calls"] += 1
        cr.rollback()

total = collections.Counter()
for key in sorted(per):
    c = per[key]
    total.update(c)
    print(
        "REPLAY %-40s calls=%-5d routed=%-5d ok=%-5d diff=%-4d gate=%-4d error=%-3d refused=%d missing_user=%d raised=%d"
        % (
            ".".join(key),
            c["calls"],
            c["routed"],
            c["shadow_ok"],
            c["shadow_diff"],
            c["gate"],
            c["error"],
            c["refused"],
            c["missing_user"],
            c["raised"],
        )
    )
    if key in first_raise:
        print("REPLAY   first raise: %s" % first_raise[key])
replayed = total["calls"]
share = (total["routed"] / replayed) if replayed else 0.0
verdict = "OK" if total["shadow_diff"] == 0 else "DIVERGED"
if total["error"]:
    verdict = "FAILED (unexpected native or shim errors)"
if total["shadow_ok"] + total["shadow_diff"] == 0:
    verdict = "FAILED (nothing compared)"
print(
    "REPLAY %s: %d captured, %d replayed, routed share %.2f, shadow ok=%d diff=%d, gate=%d error=%d refused=%d missing_user=%d"
    % (
        verdict,
        len(calls),
        replayed,
        share,
        total["shadow_ok"],
        total["shadow_diff"],
        total["gate"],
        total["error"],
        total["refused"],
        total["missing_user"],
    )
)
sys.exit(0 if verdict == "OK" else 1)
