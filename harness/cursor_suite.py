"""Run Odoo's own cursor suites and report every outcome as JSON.

One leg of `harness/cursor_parity.sh`. `RUSTORM_CURSOR=rust` installs the
engine's db shim first, so every cursor in the process is rust-backed;
anything else leaves psycopg in place. The point is that BOTH legs run the
same modules through the same runner, so the failures that are artifacts of
this runner rather than of a cursor appear on both sides and cancel.

Reports per test, not just counts: a gate that compares failure COUNTS reads
"one fixed, one new" as no change.
"""

import io
import json
import os
import sys
import unittest

MODULES = [
    "odoo.addons.base.tests.test_db_cursor",
]

# Excluded from BOTH legs, so the comparison stays symmetric, and reported in
# the output so the exclusion is visible rather than silent.
#
# `test_a_baseexception_during_construction_returns_the_connection` raises a
# real `KeyboardInterrupt` on purpose, to check that a BaseException during
# connection construction still returns the connection to the pool. Odoo's own
# runner installs a handler for that; a bare `unittest` runner inside
# `odoo-bin shell` does not, so the interrupt escapes and takes the process
# with it -- the whole suite then reports nothing at all, which is worse than
# skipping one test.
EXCLUDE = {"test_a_baseexception_during_construction_returns_the_connection"}


def _keep(suite):
    out = unittest.TestSuite()
    for t in suite:
        if isinstance(t, unittest.TestSuite):
            out.addTest(_keep(t))
        elif getattr(t, "_testMethodName", None) not in EXCLUDE:
            out.addTest(t)
    return out

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
    db_shim.install()

import importlib  # noqa: E402

import odoo.tools as tools  # noqa: E402

tools.config["db_name"] = os.environ.get("RUSTORM_DB", "rustorm_probe")

outcomes = {}
detail = {}
for modname in MODULES:
    mod = importlib.import_module(modname)
    suite = _keep(unittest.TestLoader().loadTestsFromModule(mod))
    result = unittest.TextTestRunner(verbosity=0, stream=io.StringIO()).run(suite)
    short = modname.rsplit(".", 1)[-1]
    # The last traceback line is kept, not the whole trace: it is what
    # classifies a difference as a cursor defect or as a test asserting
    # psycopg's own internals, and a gate that reports only names makes
    # somebody re-run it to find out.
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
    outcomes["__excluded__%s" % short] = sorted(EXCLUDE)

# THE GATE MUST NOT BE ABLE TO PASS BY NOT RUNNING THE THING UNDER TEST.
# A pool is built once per dsn and cached, so a shim that swaps the pool
# CLASS reaches only pools created after it installs; miss that and every
# borrow returns a psycopg connection while the leg still calls itself
# "rust", and the diff reports perfect agreement between psycopg and
# psycopg. This asserts the leg actually held a rust connection.
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

outcomes["__detail__"] = detail
with open(OUT, "w") as fh:
    json.dump(outcomes, fh, indent=1)
print(
    "CURSOR SUITE mode=%s ran=%d not-ok=%d -> %s"
    % (
        MODE,
        sum(v for k, v in outcomes.items() if k.startswith("__ran__")),
        sum(1 for k, v in outcomes.items() if v in ("fail", "error")),
        OUT,
    )
)
