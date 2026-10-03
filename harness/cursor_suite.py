import io
import json
import os
import unittest

MODULES = [
    "odoo.addons.base.tests.test_db_cursor",
]


def _cases(suite):
    for t in suite:
        if isinstance(t, unittest.TestSuite):
            yield from _cases(t)
        else:
            yield t


MODE = os.environ.get("RUSTORM_CURSOR", "psycopg")
OUT = os.environ.get("RUSTORM_CURSOR_OUT", "/tmp/rustorm_cursor_%s.json" % MODE)

if MODE == "rust":
    import engine_py

    db_shim, _orm_shim = engine_py.install_shims()
    dbname = env.cr.dbname  # noqa: F821  (from the odoo shell namespace)
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
    db_shim.PSYCOPG_CONNINFO = None
    db_shim.install()
    db_shim.set_active(True)

import importlib
import pathlib

from odoo import tools

tools.config["db_name"] = os.environ.get("RUSTORM_DB", "rustorm_probe")

outcomes = {}
detail = {}
for modname in MODULES:
    mod = importlib.import_module(modname)
    suite = unittest.TestLoader().loadTestsFromModule(mod)
    short = modname.rsplit(".", 1)[-1]
    for case in _cases(suite):
        outcomes["%s.%s" % (short, case.id().split(".", 3)[-1])] = "ok"
    result = unittest.TextTestRunner(verbosity=0, stream=io.StringIO()).run(suite)
    for case, tb in result.failures:
        key = "%s.%s" % (short, case.id().split(".", 3)[-1])
        outcomes[key] = "fail"
        detail[key] = tb.strip().splitlines()[-1][:200]
    for case, tb in result.errors:
        key = "%s.%s" % (short, case.id().split(".", 3)[-1])
        outcomes[key] = "error"
        detail[key] = tb.strip().splitlines()[-1][:200]
    for case, _reason in result.skipped:
        outcomes["%s.%s" % (short, case.id().split(".", 3)[-1])] = "skip"
    outcomes["__ran__%s" % short] = result.testsRun

if MODE == "rust":
    from odoo.db import db_connect

    _probe_cr = db_connect(os.environ.get("RUSTORM_DB", "rustorm_probe")).cursor()
    _cnx_class = type(_probe_cr._cnx).__name__
    _probe_cr.rollback()
    _probe_cr.close()
    _counters = db_shim.pool_stats()
    outcomes["__cursor__"] = _cnx_class
    outcomes["__connects__"] = _counters.get("connects", 0)
    if _cnx_class != "FakeConnection" or not _counters.get("connects"):
        raise SystemExit(
            "VACUOUS: the rust leg drew a %s with connects=%r -- it ran on "
            "psycopg, so this leg proves nothing"
            % (_cnx_class, _counters.get("connects"))
        )

if MODE != "rust":
    from odoo.db import db_connect

    _probe_cr = db_connect(os.environ.get("RUSTORM_DB", "rustorm_probe")).cursor()
    _cnx_class = type(_probe_cr._cnx).__name__
    _probe_cr.rollback()
    _probe_cr.close()
    outcomes["__cursor__"] = _cnx_class
    if _cnx_class == "FakeConnection":
        raise SystemExit(
            "VACUOUS: the psycopg leg drew a FakeConnection -- the engine is "
            "installed in this process, so this leg is the rust cursor and the "
            "comparison proves nothing. Run it on a conf without rust_engine."
        )

outcomes["__detail__"] = detail
with pathlib.Path(OUT).open("w", encoding="utf-8") as fh:
    json.dump(outcomes, fh, indent=1)
print(
    "CURSOR SUITE mode=%s ran=%d not-ok=%d -> %s"
    % (
        MODE,
        sum(v for k, v in outcomes.items() if k.startswith("__ran__")),
        sum(1 for v in outcomes.values() if v in ("fail", "error")),
        OUT,
    )
)
