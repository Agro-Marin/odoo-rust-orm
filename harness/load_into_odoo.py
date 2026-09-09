import os
import sys

import odoo
from odoo.modules.registry import Registry

EXPORT = os.environ.get("RUSTORM_EXPORT")
if not EXPORT or not os.path.exists(EXPORT):
    print("LOAD SKIP: set RUSTORM_EXPORT to a registry export")
    sys.exit(3)  # skipped, not passed

import engine_py  # noqa: E402  (the point of the exercise)

db_shim, orm_shim = engine_py.install_shims()

dbname = env.cr.dbname  # noqa: F821  (env comes from the odoo shell namespace)
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
orm_shim.install()
print("LOAD kernel: %d models, routing mode %r" % (orm_shim.KERNEL.model_count, orm_shim.MODE))

registry = Registry(dbname)
ok = True
with registry.cursor() as cr:
    e = odoo.api.Environment(cr, 2, {})
    before = dict(orm_shim.stats())
    rows = e["res.country"].search_read([("code", "=", "BE")], ["name", "code"])
    count = e["res.country"].search_count([])
    after = dict(orm_shim.stats())
    routed = after["kernel"] - before["kernel"]
    print(
        "LOAD read: %s  count=%d  routed=%d gate=%d error=%d  cursor=%s"
        % (
            rows,
            count,
            routed,
            after["fallback_gate"] - before["fallback_gate"],
            after["fallback_error"] - before["fallback_error"],
            type(cr._cnx).__name__,
        )
    )
    ok = (
        routed >= 1
        and type(cr._cnx).__name__ == "FakeConnection"
        and len(rows) == 1
        and rows[0]["code"] == "BE"
    )

    before = dict(orm_shim.stats())
    spec = {"name": {}, "code": {}, "currency_id": {"fields": {"display_name": {}}},
            "state_ids": {}}
    dom = [("code", "in", ["BE", "FR", "NL", "DE"])]
    page = e["res.country"].web_search_read(dom, spec)
    full = e["res.country"].web_search_read(dom, spec, limit=1)
    capped = e["res.country"].web_search_read(dom, spec, limit=1, count_limit=1)
    after = dict(orm_shim.stats())
    routed = after["kernel"] - before["kernel"]
    rec = page["records"][0]
    print(
        "LOAD web_search_read: length=%d/%d/%d records=%d currency=%s routed=%d error=%d"
        % (page["length"], full["length"], capped["length"], len(page["records"]),
           rec.get("currency_id"), routed,
           after["fallback_error"] - before["fallback_error"])
    )
    good = (
        routed >= 3
        and page["length"] == 4 and len(page["records"]) == 4
        and full["length"] == 4 and len(full["records"]) == 1
        and capped["length"] == 1
        and isinstance(rec["currency_id"], dict) and set(rec["currency_id"]) == {"id", "display_name"}
        and isinstance(rec["state_ids"], list)
        and after["fallback_error"] == before["fallback_error"]
    )
    ok = ok and good
    before = dict(orm_shim.stats())
    pairs = e["ir.module.category"].name_search("sal", limit=5)
    web = e["ir.module.category"].web_name_search("sal", {"display_name": {}}, limit=5)
    after = dict(orm_shim.stats())
    ns_ok = (
        after["kernel"] - before["kernel"] >= 1
        and pairs and all(isinstance(p, tuple) and len(p) == 2 for p in pairs)
        and web and set(web[0]) >= {"id", "display_name"}
        and after["fallback_error"] == before["fallback_error"]
    )
    print("LOAD name_search: %s (%d pairs, routed=%d)" % ("ok" if ns_ok else "WRONG", len(pairs), after["kernel"] - before["kernel"]))
    ok = ok and ns_ok
    empty_ok = True
    for sql in (
        "SELECT count(*) FROM res_partner WHERE id = ANY(%s)",
        "SELECT count(*) FROM res_partner WHERE name = ANY(%s)",
    ):
        try:
            cr.execute(sql, ([],))
            n = cr.fetchone()[0]
            empty_ok = empty_ok and n == 0
        except Exception as exc:  # noqa: BLE001
            print("LOAD empty list: %s -> %s: %s" % (sql, type(exc).__name__, str(exc)[:80]))
            empty_ok = False
            cr.rollback()
    for sql, params, want in (
        ("SELECT make_interval(mins => %s)", (5,), None),
        ("SELECT %s + 1", (70000,), 70001),
        ("SELECT %s + 1", (2 ** 40,), 2 ** 40 + 1),
        ("SELECT %s::numeric + 1", (2 ** 70,), None),
    ):
        try:
            cr.execute(sql, params)
            got = cr.fetchone()[0]
            if want is not None and got != want:
                print("LOAD int typing: %s -> %r, want %r" % (sql, got, want))
                empty_ok = False
        except Exception as exc:  # noqa: BLE001
            print("LOAD int typing: %s -> %s: %s" % (sql, type(exc).__name__, str(exc)[:80]))
            empty_ok = False
            cr.rollback()
    print("LOAD empty list params: %s" % ("ok" if empty_ok else "WRONG"))
    ok = ok and empty_ok
    if orm_shim.MODE == "shadow":
        print("LOAD shadow: ok=%d diff=%d" % (after["shadow_ok"], after["shadow_diff"]))
        ok = ok and after["shadow_diff"] == 0

other = os.environ.get("RUSTORM_OTHER_DB")
if other and other != dbname:
    try:
        with Registry(other).cursor() as cr:
            e = odoo.api.Environment(cr, 2, {})
            b = dict(orm_shim.stats())
            n = e["res.country"].search_count([])
            a = dict(orm_shim.stats())
        good = (
            n > 0
            and type(cr._cnx).__name__ != "FakeConnection"
            and a["kernel"] == b["kernel"]
            and a["fallback_error"] == b["fallback_error"]
        )
        print(
            "LOAD other db %s: rows=%d cursor=%s routed=%d err=%d -> %s"
            % (
                other,
                n,
                type(cr._cnx).__name__,
                a["kernel"] - b["kernel"],
                a["fallback_error"] - b["fallback_error"],
                "ok" if good else "WRONG",
            )
        )
        ok = ok and good
    except Exception as exc:  # noqa: BLE001
        print("LOAD other db %s: BROKEN %s: %s" % (other, type(exc).__name__, str(exc)[:120]))
        ok = False
else:
    print("LOAD other db: skipped (set RUSTORM_OTHER_DB)")

import signal  # noqa: E402

child = os.fork()
if child == 0:
    try:
        signal.alarm(30)
        with Registry(dbname).cursor() as cr:
            e = odoo.api.Environment(cr, 2, {})
            b = dict(orm_shim.stats())
            rows = e["res.country"].search_read([("code", "=", "BE")], ["name"])
            a = dict(orm_shim.stats())
        good = (
            len(rows) == 1
            and a["kernel"] - b["kernel"] >= 1
            and rust_db.runtime_pid == os.getpid()
        )
        print(
            "LOAD fork: pid=%d rows=%d routed=%d runtime_pid=%d -> %s"
            % (
                os.getpid(),
                len(rows),
                a["kernel"] - b["kernel"],
                rust_db.runtime_pid,
                "ok" if good else "WRONG",
            )
        )
        sys.stdout.flush()
        os._exit(0 if good else 1)
    except BaseException as exc:  # noqa: BLE001
        print("LOAD fork: FAILED %s: %s" % (type(exc).__name__, str(exc)[:80]))
        sys.stdout.flush()
        os._exit(1)

_, status = os.waitpid(child, 0)
forked_ok = os.waitstatus_to_exitcode(status) == 0
print("LOAD fork exit: %d" % os.waitstatus_to_exitcode(status))
ok = ok and forked_ok

print("LOAD %s" % ("OK" if ok else "FAILED"))
sys.exit(0 if ok else 1)
