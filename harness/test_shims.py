#!/usr/bin/env python3
import datetime as _dt
import importlib.util
import json
import os
import pathlib
import sys
import unittest

HERE = pathlib.Path(pathlib.Path(__file__).resolve()).parent
ROOT = pathlib.Path(HERE).parent

try:
    import engine_py
except ImportError as exc:  # pragma: no cover - depends on the build
    engine_py = None
    IMPORT_ERROR = exc
else:
    IMPORT_ERROR = None

_SHIMS = None
_ODOO = None


def _shims():
    global _SHIMS
    if engine_py is None:
        raise unittest.SkipTest("engine_py is not importable (%s)" % IMPORT_ERROR)
    if _SHIMS is None:
        _SHIMS = engine_py.install_shims()
    return _SHIMS


def _addon():
    path = os.path.join(ROOT, "addons", "rust_engine", "__init__.py")
    spec = importlib.util.spec_from_file_location("rust_engine_probe", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _odoo():
    global _ODOO
    if _ODOO is not None:
        return _ODOO
    odoo_root = os.environ.get("RUSTORM_ODOO_ROOT") or os.path.join(
        pathlib.Path(ROOT).parent, "odoo"
    )
    sys.path.insert(0, odoo_root)
    try:
        import odoo.orm.models.base as base_mod

        import odoo.addons

        odoo.addons.__path__.append(os.path.join(odoo_root, "addons"))
        from odoo.addons.web.models import web_read as web_read_mod
    except ImportError as exc:
        raise unittest.SkipTest(
            "odoo not importable from %s: %s" % (odoo_root, exc)
        ) from exc
    _ODOO = (base_mod, web_read_mod)
    return _ODOO


def check(name, got, want) -> None:
    assert got == want, "%s\n    got  %r\n    want %r" % (name, got, want)


class F:
    def __init__(self, type_, store=True, related=None, comodel_name=None) -> None:
        self.type, self.store, self.related = type_, store, related
        self.comodel_name = comodel_name


def test_dbname_keyword_and_dict_specs() -> None:
    dbname = _shims()[0]._dbname
    check("dict", dbname({"dbname": "mydb"}), "mydb")
    check("dict database=", dbname({"database": "mydb"}), "mydb")
    check("unquoted", dbname("host=localhost dbname=mydb user=odoo"), "mydb")
    check(
        "quoted (the defect)",
        dbname("host='/var/run/postgresql' dbname='mydb' user='odoo'"),
        "mydb",
    )
    check("value with a space", dbname("dbname='has space' host=x"), "has space")
    check(
        "escaped quote in an earlier value",
        dbname(r"password='a b\'c' dbname='mydb'"),
        "mydb",
    )
    check("escaped backslash", dbname(r"password='a\\b' dbname='mydb'"), "mydb")
    check("absent", dbname("host=x user=y"), None)
    check("empty", dbname(""), None)
    check("not a spec", dbname(None), None)
    check("spaces around =", dbname("dbname = mydb"), "mydb")
    check("dangling key", dbname("dbname=mydb host"), "mydb")


def test_dbname_uri_specs() -> None:
    dbname = _shims()[0]._dbname
    check("uri path", dbname("postgresql://odoo:pw@localhost:5432/mydb"), "mydb")
    check(
        "postgres scheme", dbname("postgres://localhost/mydb?sslmode=require"), "mydb"
    )
    check(
        "uri without a path uses the user",
        dbname("postgresql://odoo@localhost"),
        "odoo",
    )
    check(
        "uri with dbname in the query", dbname("postgresql://localhost/?dbname=q"), "q"
    )
    check("percent-encoded path", dbname("postgresql://h/my%20db"), "my db")
    check("dict carrying a dsn", dbname({"dsn": "postgresql://h/fromdsn"}), "fromdsn")
    check(
        "dict dbname beats its dsn",
        dbname({"dsn": "postgresql://h/fromdsn", "dbname": "explicit"}),
        "explicit",
    )


def test_the_composed_dsn_names_every_keyword_once() -> None:
    # tokio-postgres ACCUMULATES `host` and `port` where libpq lets the last
    # occurrence win, and then requires the two counts to match. Odoo's own
    # connection_info carries a port and, with `db_host =` unset, no host at
    # all -- so concatenating it onto the armed dsn gave one host and two
    # ports and every borrow died with `invalid number of ports` before it
    # opened a socket. Measured on this workspace's p314o19m.conf: the engine
    # could not arm at all.
    db_shim = _shims()[0]
    saved = db_shim.CONNINFO
    try:
        db_shim.CONNINFO = (
            "port='5432' user='marin' sslmode='prefer' host='/var/run/postgresql'"
        )
        dsn = db_shim._dsn_with_kwargs(
            "", {"dbname": "mydb", "port": 5432, "user": "marin", "sslmode": "prefer"}
        )
        for key in ("host", "port", "user", "dbname", "sslmode"):
            check(
                "%s appears once in %r" % (key, dsn),
                dsn.count("%s=" % key),
                1,
            )
        check("the host survives", "host='/var/run/postgresql'" in dsn, True)
        check("the later value wins", "dbname='mydb'" in dsn, True)

        # a quoted value carrying spaces is one keyword, not several
        db_shim.CONNINFO = "host='/tmp' options='-c jit=off -c work_mem=16MB'"
        dsn = db_shim._dsn_with_kwargs("", {"dbname": "mydb"})
        check(
            "options keeps its spaces",
            "options='-c jit=off -c work_mem=16MB'" in dsn,
            True,
        )
        check("options is one keyword", dsn.count("options="), 1)
    finally:
        db_shim.CONNINFO = saved


def test_borrow_refuses_readonly_pools_and_foreign_keys() -> None:
    db_shim = _shims()[0]

    class Pool:
        readonly = False

    class ReadOnly:
        readonly = True

    saved = db_shim.CONNINFO, db_shim.PSYCOPG_CONNINFO
    try:
        db_shim.CONNINFO = "host=/tmp user=u dbname=mydb"
        db_shim.PSYCOPG_CONNINFO = None
        check(
            "same db intercepts", db_shim._intercepts(Pool(), {"dbname": "mydb"}), True
        )
        check(
            "other db delegates",
            db_shim._intercepts(Pool(), {"dbname": "other"}),
            False,
        )
        check(
            "readonly pool delegates",
            db_shim._intercepts(ReadOnly(), {"dbname": "mydb"}),
            False,
        )
        check(
            "no conninfo intercepts everything",
            (
                setattr(db_shim, "CONNINFO", None),
                db_shim._intercepts(Pool(), {"dbname": "x"}),
            )[1],
            True,
        )

        db_shim.CONNINFO = "host=/tmp user=u dbname=mydb"
        db_shim.PSYCOPG_CONNINFO = {
            "dbname": "mydb",
            "host": "/tmp",
            "user": "u",
            "application_name": "odoo-1",
        }
        ours = frozenset(
            {
                ("database", "mydb"),
                ("host", "/tmp"),
                ("user", "u"),
                ("application_name", "odoo-2"),
                ("password_fp", ""),
            }
        )
        check(
            "same identity intercepts (application_name ignored)",
            db_shim._intercepts(Pool(), {"dbname": "mydb"}, ours),
            True,
        )
        replica = frozenset(
            {
                ("database", "mydb"),
                ("host", "replica"),
                ("user", "u"),
                ("password_fp", ""),
            }
        )
        check(
            "same db on another host delegates",
            db_shim._intercepts(Pool(), {"dbname": "mydb"}, replica),
            False,
        )
        other_pw = frozenset(
            {
                ("database", "mydb"),
                ("host", "/tmp"),
                ("user", "u"),
                ("password_fp", "deadbeef"),
            }
        )
        check(
            "same db with other credentials delegates",
            db_shim._intercepts(Pool(), {"dbname": "mydb"}, other_pw),
            False,
        )
    finally:
        db_shim.CONNINFO, db_shim.PSYCOPG_CONNINFO = saved


def test_reset_sql_matches_the_fork() -> None:
    db_shim = _shims()[0]
    try:
        _odoo()
        from odoo.db.lifecycle import _RESET_SESSION_STATE_SQL
    except unittest.SkipTest:
        return
    check("reset sql", db_shim.RESET_SESSION_STATE_SQL, _RESET_SESSION_STATE_SQL)


class _Result:
    def __init__(self, rows, columns=(), rowcount=None) -> None:
        self.rows = list(rows)
        self.columns = list(columns)
        self.rowcount = len(rows) if rowcount is None else rowcount


class _RustConn:
    def __init__(self, results=None) -> None:
        self.results = list(results or [])
        self.calls = []
        self.closed = False
        self.in_transaction = False
        self.readonly = False
        self.resets = []

    def execute(self, query, params=None):
        self.calls.append((query, params))
        if self.results:
            return self.results.pop(0)
        return _Result([], [], 0)

    def reset_session(self, sql, discard) -> None:
        self.resets.append((sql, discard))
        self.in_transaction = False

    def set_readonly(self, value) -> None:
        self.readonly = value

    def commit(self) -> None:
        self.in_transaction = False

    def rollback(self) -> None:
        self.in_transaction = False

    def close(self) -> None:
        self.closed = True


def test_fake_cursor_fetch_semantics() -> None:
    import psycopg

    db_shim = _shims()[0]
    cur = db_shim.FakeConnection(_RustConn()).cursor()
    for name in ("fetchone", "fetchall"):
        try:
            getattr(cur, name)()
        except psycopg.ProgrammingError:
            pass
        else:
            raise AssertionError("%s before execute did not raise" % name)
    try:
        cur.fetchmany(2)
    except psycopg.ProgrammingError:
        pass
    else:
        raise AssertionError("fetchmany before execute did not raise")
    check("rowcount before execute", cur.rowcount, -1)
    check("description before execute", cur.description, None)

    rust = _RustConn([_Result([(1, "a"), (2, "b"), (3, "c")], ["id", "name"])])
    cur = db_shim.FakeConnection(rust).cursor()
    cur.execute("SELECT id, name FROM t")
    check("description names", [c.name for c in cur.description], ["id", "name"])
    check("rowcount", cur.rowcount, 3)
    check("fetchone advances", cur.fetchone(), (1, "a"))
    check("fetchmany continues", cur.fetchmany(1), [(2, "b")])
    check("fetchall drains", cur.fetchall(), [(3, "c")])
    check("fetchone at the end is None", cur.fetchone(), None)
    check("fetchall at the end is empty", cur.fetchall(), [])

    rust = _RustConn([_Result([], [], 2), _Result([], [], 3)])
    cur = db_shim.FakeConnection(rust).cursor()
    cur.executemany("UPDATE t SET a = %s", [(1,), (2,)])
    check("executemany totals the counts", cur.rowcount, 5)
    check("executemany ran each", len(rust.calls), 2)


def test_give_back_resets_the_session() -> None:
    import psycopg

    db_shim = _shims()[0]
    rust = _RustConn()
    cnx = db_shim.FakeConnection(rust)
    cnx.read_only = True
    cnx.reset_session(discard=False)
    check(
        "reset ran the fork's statement",
        rust.resets,
        [(db_shim.RESET_SESSION_STATE_SQL, False)],
    )
    cnx.reset_session(discard=True)
    check(
        "discard runs DISCARD ALL and clears prepared",
        rust.resets[-1],
        ("DISCARD ALL", True),
    )
    check(
        "idle after reset",
        cnx.info.transaction_status,
        psycopg.pq.TransactionStatus.IDLE,
    )
    rust.in_transaction = True
    check(
        "in-transaction status",
        cnx.info.transaction_status,
        psycopg.pq.TransactionStatus.INTRANS,
    )


def test_raise_pg_maps_sqlstates() -> None:
    import psycopg

    db_shim = _shims()[0]
    cases = [
        ("SQLSTATE:23505|duplicate key", psycopg.errors.UniqueViolation),
        ("SQLSTATE:40001|could not serialize", psycopg.errors.SerializationFailure),
        ("SQLSTATE:42P01|relation does not exist", psycopg.errors.UndefinedTable),
        ("SQLSTATE:|connection reset", psycopg.OperationalError),
        ("plain runtime failure", psycopg.OperationalError),
    ]
    for text, cls in cases:
        try:
            db_shim._raise_pg(RuntimeError(text))
        except cls as e:
            check("message for %s" % text, str(e), text.partition("|")[2] or text)
        else:
            raise AssertionError("%s did not raise %s" % (text, cls.__name__))
    try:
        db_shim._raise_pg(RuntimeError("SQLSTATE:23505|dup"))
    except psycopg.Error as e:
        check(
            "a unique violation is an IntegrityError",
            isinstance(e, psycopg.IntegrityError),
            True,
        )


def test_routing_policy() -> None:
    orm_shim = _shims()[1]
    saved = orm_shim.MODE, orm_shim.KERNEL, orm_shim.DBNAME
    try:
        orm_shim.KERNEL = object()
        orm_shim.set_mode("off")
        check("mode off refuses", orm_shim._policy_allows("res.partner"), False)
        orm_shim.set_mode("on")
        check("mode on allows", orm_shim._policy_allows("res.partner"), True)

        orm_shim.ONLY = {"res.country"}
        check("ONLY excludes", orm_shim._policy_allows("res.partner"), False)
        check("ONLY includes", orm_shim._policy_allows("res.country"), True)
        orm_shim.ONLY = set()
        orm_shim.EXCEPT = {"res.partner"}
        check("EXCEPT excludes", orm_shim._policy_allows("res.partner"), False)
        orm_shim.EXCEPT = set()

        orm_shim.BREAKER = 2
        orm_shim.STATS["errors_by_model"]["res.partner"] = 2
        check("breaker trips its model", orm_shim._policy_allows("res.partner"), False)
        check("breaker spares the rest", orm_shim._policy_allows("res.country"), True)
        orm_shim.reset_breaker("res.partner")
        check("reset_breaker", orm_shim._policy_allows("res.partner"), True)
        orm_shim.BREAKER = 0

        try:
            orm_shim.set_mode("sideways")
        except ValueError:
            pass
        else:
            raise AssertionError("set_mode accepted an unknown mode")
    finally:
        orm_shim.MODE, orm_shim.KERNEL, orm_shim.DBNAME = saved
        orm_shim.STATS["errors_by_model"].clear()


def test_breaker_counts_only_unexpected_errors() -> None:
    orm_shim = _shims()[1]

    class M:
        _name = "probe.breaker"

    orm_shim.STATS["errors_by_model"].pop("probe.breaker", None)
    orm_shim.STATS["errors"].pop("probe.breaker", None)
    before = orm_shim.STATS["fallback_error"]
    orm_shim._record_error(
        M(), orm_shim.KernelRefused("kernel declined: unsupported operator")
    )
    check("a refusal is a fallback", orm_shim.STATS["fallback_error"] - before, 1)
    check(
        "a refusal does not count toward the breaker",
        orm_shim.STATS["errors_by_model"].get("probe.breaker", 0),
        0,
    )
    orm_shim._record_error(M(), KeyError("boom"))
    check(
        "a bug counts toward the breaker",
        orm_shim.STATS["errors_by_model"].get("probe.breaker", 0),
        1,
    )
    orm_shim.STATS["errors_by_model"].pop("probe.breaker", None)
    orm_shim.STATS["errors"].pop("probe.breaker", None)


def test_native_error_categories_and_shadow_quarantine() -> None:
    orm_shim = _shims()[1]
    from rust_engine_errors import (
        KernelAccessDenied,
        KernelDatabaseError,
        KernelInternalError,
        KernelRefused,
        KernelRegistryStale,
    )

    class M:
        _name = "probe.categories"

    saved = orm_shim.MODE, orm_shim.ONLY, orm_shim.EXCEPT, orm_shim.BREAKER
    try:
        orm_shim.MODE, orm_shim.ONLY, orm_shim.EXCEPT, orm_shim.BREAKER = (
            "on",
            (),
            (),
            0,
        )
        orm_shim.reset_breaker(M._name)
        for kind in (KernelRefused, KernelAccessDenied, KernelRegistryStale):
            orm_shim._record_error(M(), kind("deliberately refused"))
        check(
            "explicit refusals exempt",
            orm_shim.STATS["errors_by_model"].get(M._name, 0),
            0,
        )
        for kind in (RuntimeError, KernelDatabaseError, KernelInternalError):
            orm_shim._record_error(M(), kind("unsupported operator"))
        check(
            "prose cannot turn failures into refusals",
            orm_shim.STATS["errors_by_model"][M._name],
            3,
        )
        orm_shim._shadow(M(), "search_count", 1, 2)
        check(
            "mismatch quarantines even with error breaker off",
            orm_shim._policy_allows(M._name),
            False,
        )
        check(
            "quarantine spares other models",
            orm_shim._policy_allows("probe.other"),
            True,
        )
        orm_shim.reset_breaker(M._name)
        check("explicit reset permits retry", orm_shim._policy_allows(M._name), True)
    finally:
        orm_shim.reset_breaker(M._name)
        orm_shim.STATS["errors"].pop(M._name, None)
        orm_shim.MODE, orm_shim.ONLY, orm_shim.EXCEPT, orm_shim.BREAKER = saved


def test_sampling() -> None:
    orm_shim = _shims()[1]
    saved_sample = orm_shim.SAMPLE
    try:
        orm_shim.set_sample(0)
        check(
            "sample 0 never fires",
            any(orm_shim._verify_this_one() for _ in range(2000)),
            False,
        )
        orm_shim.set_sample(1)
        check(
            "sample 1 always fires",
            all(orm_shim._verify_this_one() for _ in range(200)),
            True,
        )
        orm_shim.set_sample(0.5)
        fired = sum(orm_shim._verify_this_one() for _ in range(4000))
        check("sample 0.5 is a coin", 1600 < fired < 2400, True)
        for bad in (-0.1, 1.5):
            try:
                orm_shim.set_sample(bad)
            except ValueError:
                pass
            else:
                raise AssertionError("set_sample accepted %r" % bad)
    finally:
        orm_shim.SAMPLE = saved_sample


def test_kill_switch_parameters() -> None:
    orm_shim = _shims()[1]
    mod = _addon()
    saved = orm_shim.MODE, orm_shim.SAMPLE
    try:
        orm_shim.set_mode("on")
        mod._apply_params(orm_shim, {mod.PARAM_MODE: "off"})
        check("switch turns routing off", orm_shim.MODE, "off")
        mod._apply_params(orm_shim, {mod.PARAM_MODE: "shadow"})
        check("switch selects shadow", orm_shim.MODE, "shadow")
        mod._apply_params(orm_shim, {})
        check("absent key changes nothing", orm_shim.MODE, "shadow")
        mod._apply_params(orm_shim, {mod.PARAM_MODE: "of"})
        check("a typo is ignored", orm_shim.MODE, "shadow")
        mod._apply_params(orm_shim, {mod.PARAM_MODE: " OFF "})
        check("value is trimmed and lowercased", orm_shim.MODE, "off")

        orm_shim.set_sample(0)
        mod._apply_params(orm_shim, {mod.PARAM_SAMPLE: "0.25"})
        check("switch sets the sample", orm_shim.SAMPLE, 0.25)
        mod._apply_params(orm_shim, {mod.PARAM_SAMPLE: "banana"})
        check("a bad sample is ignored", orm_shim.SAMPLE, 0.25)
        mod._apply_params(orm_shim, {mod.PARAM_SAMPLE: "9"})
        check("an out-of-range sample is ignored", orm_shim.SAMPLE, 0.25)
    finally:
        orm_shim.MODE, orm_shim.SAMPLE = saved


def test_config_typos_leave_routing_off() -> None:
    orm_shim = _shims()[1]
    mod = _addon()
    saved = orm_shim.MODE, orm_shim.SAMPLE
    try:
        orm_shim.set_mode("on")
        orm_shim.set_sample(0.5)
        mod._apply_config(
            orm_shim, {"rust_engine_mode": "of", "rust_engine_verify_sample": "0.1"}
        )
        check("a mode typo leaves routing off", orm_shim.MODE, "off")
        check("a good sample beside a bad mode still applies", orm_shim.SAMPLE, 0.1)
        orm_shim.set_mode("on")
        mod._apply_config(
            orm_shim,
            {"rust_engine_mode": "Shadow ", "rust_engine_verify_sample": "lots"},
        )
        check("mode is trimmed and lowercased", orm_shim.MODE, "shadow")
        check("a bad sample is zeroed", orm_shim.SAMPLE, 0)
        mod._apply_config(
            orm_shim, {"rust_engine_mode": "on", "rust_engine_verify_sample": 7}
        )
        check("an out-of-range sample is zeroed", orm_shim.SAMPLE, 0)
        mod._apply_config(orm_shim, {})
        check("an absent mode is off", orm_shim.MODE, "off")
    finally:
        orm_shim.MODE, orm_shim.SAMPLE = saved


def test_the_report_line_tells_refusals_from_errors() -> None:
    orm_shim = _shims()[1]
    mod = _addon()
    saved = dict(orm_shim.STATS)
    try:
        orm_shim.STATS["fallback_error"] = 7
        orm_shim.STATS["kernel_refused"] = 5
        lines = []
        mod._logger.info = lambda msg, *args: lines.append(msg % args)
        mod._report(orm_shim)
        check("refusals are their own counter", "refused=5 error=2" in lines[0], True)
    finally:
        del mod._logger.info
        orm_shim.STATS.clear()
        orm_shim.STATS.update(saved)


def test_config_scopes_routing_and_arms_the_breaker() -> None:
    orm_shim = _shims()[1]
    mod = _addon()
    saved = (
        orm_shim.MODE,
        orm_shim.SAMPLE,
        orm_shim.ONLY,
        orm_shim.EXCEPT,
        orm_shim.BREAKER,
    )
    try:
        mod._apply_config(orm_shim, {"rust_engine_mode": "on"})
        check("the breaker is armed by default", orm_shim.BREAKER, mod.DEFAULT_BREAKER)
        mod._apply_config(
            orm_shim,
            {
                "rust_engine_mode": "on",
                "rust_engine_only": "res.partner, res.users,",
                "rust_engine_except": "res.users",
                "rust_engine_breaker": "5",
            },
        )
        check(
            "only is a set of trimmed names",
            orm_shim.ONLY,
            frozenset({"res.partner", "res.users"}),
        )
        check("except is a set too", orm_shim.EXCEPT, frozenset({"res.users"}))
        check("the breaker takes the configured count", orm_shim.BREAKER, 5)
        check("except beats only", orm_shim._policy_allows("res.users"), False)
        check("only admits its model", orm_shim._policy_allows("res.partner"), True)
        check("only excludes the rest", orm_shim._policy_allows("res.country"), False)
        mod._apply_config(
            orm_shim, {"rust_engine_mode": "on", "rust_engine_breaker": "many"}
        )
        check(
            "a bad breaker falls back to the default",
            orm_shim.BREAKER,
            mod.DEFAULT_BREAKER,
        )
        mod._apply_config(
            orm_shim, {"rust_engine_mode": "on", "rust_engine_breaker": "0"}
        )
        check("zero disarms it explicitly", orm_shim.BREAKER, 0)
    finally:
        (
            orm_shim.MODE,
            orm_shim.SAMPLE,
            orm_shim.ONLY,
            orm_shim.EXCEPT,
            orm_shim.BREAKER,
        ) = saved


def test_web_records_and_length() -> None:
    orm_shim = _shims()[1]
    recs = [
        {"id": 1, "partner_id": [7, "Seven"], "user_id": [2, "Admin"], "name": "a"},
        {"id": 2, "partner_id": False, "user_id": [3, "Bob"], "name": "b"},
    ]
    out = orm_shim._web_records(recs, named={"partner_id"}, plain={"user_id"})
    check("m2o named", out[0]["partner_id"], {"id": 7, "display_name": "Seven"})
    check("m2o plain", out[0]["user_id"], 2)
    check("m2o empty stays False", out[1]["partner_id"], False)
    check("untouched field", out[1]["name"], "b")

    calls = []

    def count(cap) -> int:
        calls.append(cap)
        return 42

    L = orm_shim._web_length
    check("no records no offset", L(0, 0, 80, None, False, count), 0)
    check("no records with offset counts", L(0, 80, 80, 500, False, count), 42)
    check("...with the count limit", calls[-1], 500)
    check("page not full: current length", L(3, 0, 80, None, False, count), 3)
    check("page full: counts", L(80, 0, 80, None, False, count), 42)
    check("page full but count limit reached", L(80, 0, 80, 80, False, count), 80)
    check("force_search_count counts", L(3, 0, 80, None, True, count), 42)
    check("no limit: current length", L(3, 10, None, None, False, count), 13)


def test_web_spec_plan() -> None:
    orm_shim = _shims()[1]

    class M:
        _fields = {
            "name": F("char"),
            "partner_id": F("many2one"),
            "user_id": F("many2one"),
            "tag_ids": F("many2many"),
            "ref_id": F("reference"),
            "props": F("properties"),
            "icon": F("char", store=False),
            "cur": F("many2one", store=False, related="company_id.currency_id"),
        }

    plan = orm_shim._web_spec_plan
    check(
        "plain spec",
        plan(
            M(),
            {
                "name": {},
                "partner_id": {"fields": {"display_name": {}}},
                "user_id": {},
                "tag_ids": {},
            },
        ),
        (["name", "partner_id", "user_id", "tag_ids"], {"partner_id"}, {"user_id"}),
    )
    check("unknown field refuses", plan(M(), {"nope": {}}), None)
    check(
        "m2o with extra sub-field refuses",
        plan(M(), {"partner_id": {"fields": {"display_name": {}, "email": {}}}}),
        None,
    )
    check(
        "m2o with context refuses",
        plan(
            M(), {"partner_id": {"fields": {"display_name": {}}, "context": {"x": 1}}}
        ),
        None,
    )
    check(
        "x2many with fields refuses",
        plan(M(), {"tag_ids": {"fields": {"name": {}}}}),
        None,
    )
    check("x2many with order refuses", plan(M(), {"tag_ids": {"order": "name"}}), None)
    check("x2many with limit refuses", plan(M(), {"tag_ids": {"limit": 5}}), None)
    check("reference with spec refuses", plan(M(), {"ref_id": {"fields": {}}}), None)
    check(
        "a bare reference refuses too: the kernel cannot read it",
        plan(M(), {"ref_id": {}}),
        None,
    )
    check("properties with spec refuses", plan(M(), {"props": {"fields": {}}}), None)
    check("a compute refuses", plan(M(), {"icon": {}}), None)
    check(
        "a related non-stored field maps",
        plan(M(), {"cur": {}}),
        (["cur"], set(), {"cur"}),
    )
    orm_shim._GATE_CACHE.update({"k": True})
    orm_shim.forget_gates()
    check("forget_gates empties the verdicts", dict(orm_shim._GATE_CACHE), {})


def test_revive_temporal() -> None:
    orm_shim = _shims()[1]
    revive = orm_shim._revive_temporal
    check(
        "a datetime aggregate is revived",
        revive(F("datetime"), "2026-09-10 01:42:04.523820"),
        _dt.datetime(2026, 9, 10, 1, 42, 4, 523820),
    )
    check(
        "a date aggregate is revived",
        revive(F("date"), "2026-09-10"),
        _dt.date(2026, 9, 10),
    )
    check("a numeric granularity stays a number", revive(F("datetime"), 3.0), 3.0)
    check("an unset value stays False", revive(F("datetime"), False), False)
    check("a count has no field and stays as is", revive(None, "7"), "7")
    check(
        "an array_agg of dates is revived element-wise",
        revive(F("date"), ["2026-01-01", None, "2026-01-02"]),
        [_dt.date(2026, 1, 1), None, _dt.date(2026, 1, 2)],
    )
    check(
        "an array_agg of datetimes is revived element-wise",
        revive(F("datetime"), ["2026-01-01 00:00:00"]),
        [_dt.datetime(2026, 1, 1)],
    )
    check(
        "an array_agg of a non-temporal field is untouched",
        revive(F("char"), ["a", "b"]),
        ["a", "b"],
    )
    check("an empty array_agg stays empty", revive(F("date"), []), [])
    ok_aggs = orm_shim._aggregates_ok
    check("sum and __count are routable", ok_aggs(["__count", "amount:sum"]), True)
    check(
        "array_agg is routable since the kernel decodes arrays",
        ok_aggs(["amount:array_agg"]),
        True,
    )
    check("recordset is not", ok_aggs(["partner_id:recordset"]), False)
    check("a bare field spec is not", ok_aggs(["amount"]), False)


def test_label_dependencies_with_a_fake_model() -> None:
    orm_shim = _shims()[1]

    class Partner:
        _name = "res.partner"
        _rec_name = "complete_name"
        _rec_names_search = ["complete_name", "email", "ref"]
        _fields = {
            "complete_name": F("char"),
            "email": F("char"),
            "ref": F("char"),
            "country_id": F("many2one", comodel_name="res.country"),
            "user_id": F("many2one", comodel_name="res.users"),
            "name": F("char"),
        }

    class Country:
        _name = "res.country"
        _rec_name = "name"
        _rec_names_search = None
        _fields = {"name": F("char"), "code": F("char")}

    class Users:
        _name = "res.users"
        _rec_name = "name"
        _rec_names_search = ["name", "login", "partner_id.email"]
        _fields = {"name": F("char"), "login": F("char"), "partner_id": F("many2one")}

    registry = {
        "res.partner": Partner(),
        "res.country": Country(),
        "res.users": Users(),
    }

    class Env:
        def __getitem__(self, name):
            return registry[name]

    for m in registry.values():
        m.env = Env()

    deps = orm_shim._label_dependencies(
        registry["res.partner"], ["name", "country_id", "user_id", "display_name"], {}
    )
    check(
        "the model's own rec_name and rec_names_search back display_name",
        deps["res.partner"],
        {"complete_name", "email", "ref"},
    )
    check(
        "a many2one label reads the comodel's rec_name", deps["res.country"], {"name"}
    )
    check(
        "a comodel's rec_names_search is flushed too, dotted paths by their head",
        deps["res.users"],
        {"name", "login", "partner_id"},
    )
    deps = orm_shim._label_dependencies(registry["res.partner"], ["name"], {"x": {"y"}})
    check("scalar fields add nothing and existing deps survive", deps, {"x": {"y"}})
    check(
        "a rec_name that is not a field is skipped",
        orm_shim._label_fields(
            type(
                "M",
                (),
                {"_rec_name": "ghost", "_rec_names_search": None, "_fields": {}},
            )()
        ),
        set(),
    )


def test_taints_clear_on_commit_and_rollback() -> None:
    orm_shim = _shims()[1]

    class Cr:
        pass

    cr = Cr()
    orm_shim.DIRTY_CRS.add(cr)
    orm_shim.WRITTEN_X2MANY[cr] = {("res.partner", "child_ids")}
    check("tainted", cr in orm_shim.DIRTY_CRS, True)
    orm_shim._untaint(cr)
    check("security taint cleared", cr in orm_shim.DIRTY_CRS, False)
    check("x2many taint cleared", orm_shim.WRITTEN_X2MANY.get(cr), None)
    orm_shim._untaint(cr)
    check("untaint is idempotent", cr in orm_shim.DIRTY_CRS, False)


def test_gate_reasons_and_stats() -> None:
    orm_shim = _shims()[1]

    class _Named:
        _name = "probe.model"

    orm_shim.GATE_REASONS.clear()
    before = orm_shim.STATS["fallback_gate"]
    orm_shim._refuse("policy excludes the model")
    orm_shim._gated(_Named(), "search_read")
    orm_shim._gated(_Named(), "read")
    check(
        "a refused gate counts the fallback",
        orm_shim.STATS["fallback_gate"] - before,
        2,
    )
    check(
        "the gate's reason is recorded under the model and method",
        orm_shim.GATE_REASONS[
            ("probe.model", "search_read", "policy excludes the model")
        ],
        1,
    )
    check(
        "a site refusing on its own shape records that, and the reason does not leak",
        orm_shim.GATE_REASONS[("probe.model", "read", "call shape")],
        1,
    )
    check("stats() exposes the top reasons", len(orm_shim.stats()["gate_reasons"]), 2)
    check(
        "fallback_flush is a plain counter",
        isinstance(orm_shim.STATS["fallback_flush"], int),
        True,
    )


def test_read_fields_and_reorder() -> None:
    orm_shim = _shims()[1]

    class _M:
        _fields = {
            "name": F("char"),
            "icon": F("char", store=False),
            "img": F("binary"),
            "cur": F("many2one", store=False, related="company_id.currency_id"),
            "display_name": F("char", store=False),
        }

    rfo = orm_shim._read_fields_ok
    check("stored fields read", rfo(_M(), ["name"]), True)
    check("related non-stored reads", rfo(_M(), ["name", "cur"]), True)
    check("display_name reads", rfo(_M(), ["display_name"]), True)
    check("a compute does not", rfo(_M(), ["name", "icon"]), False)
    check("a binary does not", rfo(_M(), ["img"]), False)
    check("an unknown field does not", rfo(_M(), ["nope"]), False)

    rr = orm_shim._read_reorder
    recs = [
        {"id": 2, "name": "b", "partner_id": (7, "Seven")},
        {"id": 1, "name": "a", "partner_id": False},
    ]
    check(
        "read keeps the requested order",
        [r["id"] for r in rr([1, 2], recs, "_classic_read")],
        [1, 2],
    )
    check(
        "read repeats a repeated id",
        [r["id"] for r in rr([2, 2, 1], recs, "_classic_read")],
        [2, 2, 1],
    )
    check(
        "classic read keeps m2o tuples",
        rr([2], recs, "_classic_read")[0]["partner_id"],
        (7, "Seven"),
    )
    check("load=None bares the m2o id", rr([2], recs, None)[0]["partner_id"], 7)
    check(
        "load=False bares the m2o id too, as _read_format does",
        rr([2], recs, False)[0]["partner_id"],
        7,
    )
    check("a missing id is not answered", rr([1, 3], recs, None), None)
    check("an unrequested row is ignored", [r["id"] for r in rr([1], recs, None)], [1])


def test_written_x2many() -> None:
    orm_shim = _shims()[1]

    class _Inv:
        def __init__(self, model_name, name) -> None:
            self.model_name, self.name = model_name, name

    class _Pool:
        def __init__(self, inverses) -> None:
            self.field_inverses = inverses

    class _W:
        _name = "res.partner"
        _fields = {
            "tag_ids": F("many2many"),
            "parent_id": F("many2one"),
            "name": F("char"),
            "child_ids": F("one2many"),
        }
        pool = _Pool(
            {
                _fields["parent_id"]: [_Inv("res.partner", "child_ids")],
                _fields["tag_ids"]: [_Inv("res.partner.tag", "partner_ids")],
            }
        )

    ww = orm_shim._written_x2many
    check(
        "an x2many write marks itself and its inverse",
        ww(_W(), [{"tag_ids": [(6, 0, [1])]}]),
        {("res.partner", "tag_ids"), ("res.partner.tag", "partner_ids")},
    )
    check(
        "a many2one write marks the inverse x2many",
        ww(_W(), [{"parent_id": 3}]),
        {("res.partner", "child_ids")},
    )
    check("a scalar write marks nothing", ww(_W(), [{"name": "x"}]), set())


def test_baseline_depth_and_parse_dt() -> None:
    orm_shim = _shims()[1]
    check("policy allows outside a baseline", orm_shim._in_baseline(), False)
    check(
        "policy refuses inside a baseline",
        orm_shim._baseline(orm_shim._in_baseline),
        True,
    )
    check("baseline depth unwinds", orm_shim._in_baseline(), False)

    parse = orm_shim._parse_dt
    check("no microseconds", str(parse("2026-08-31 12:34:56")), "2026-08-31 12:34:56")
    check(
        "microseconds",
        str(parse("2026-08-31 12:34:56.123456")),
        "2026-08-31 12:34:56.123456",
    )
    check(
        "trailing zeros kept",
        str(parse("2026-08-31 12:34:56.100000")),
        "2026-08-31 12:34:56.100000",
    )


def test_install_is_idempotent_and_keeps_stamps() -> None:
    orm_shim = _shims()[1]
    base_mod, web_read_mod = _odoo()
    first = orm_shim.install()
    second = orm_shim.install()
    check("install returns the same originals on a second call", first is second, True)
    db2, orm2 = engine_py.install_shims()
    check("install_shims hands back the registered modules", orm2 is orm_shim, True)
    check("install_shims hands back the registered db shim", db2 is _shims()[0], True)
    BaseModel = base_mod.BaseModel
    check("search_read is patched once", BaseModel.search_read.__name__, "search_read")
    for name in ("search_read", "search_count"):
        check("%s keeps api.model" % name, getattr(BaseModel, name)._api_model, True)
        check("%s keeps api.readonly" % name, getattr(BaseModel, name)._readonly, True)
    check("create keeps api.model", BaseModel.create._api_model, True)
    check("name_search keeps api.model", BaseModel.name_search._api_model, True)
    check("name_search keeps api.readonly", BaseModel.name_search._readonly, True)
    from odoo.fields import Domain

    class _Env:
        uid, su, context = 2, False, {}

        class registry:
            registry_sequence = 7

    class _Model:
        _name = "res.partner"
        env = _Env()

    req = json.loads(
        orm_shim._request(
            _Model(), "search_read", domain=[], fields=["x"], x2many_active_test=False
        )
    )
    check(
        "read() forwards the env's active_test for x2many corecords",
        req["x2many_active_test"],
        False,
    )
    req = json.loads(
        orm_shim._request(
            _Model(),
            "search_count",
            domain=Domain([("id", "=", 1)]) & Domain([("active", "=", True)]),
        )
    )
    check(
        "a Domain object serialises as a list",
        isinstance(req["domain"], list) and len(req["domain"]) >= 2,
        True,
    )
    wsr = web_read_mod.Base.web_search_read
    check("web_search_read keeps api.model", getattr(wsr, "_api_model", False), True)
    check("web_search_read keeps api.readonly", getattr(wsr, "_readonly", False), True)
    import odoo.db.cursor as cursor_mod

    check(
        "commit is patched to clear taints", cursor_mod.Cursor.commit.__name__, "commit"
    )
    check(
        "rollback is patched to clear taints",
        cursor_mod.Cursor.rollback.__name__,
        "rollback",
    )


def test_diff_scoring() -> None:
    diff_path = os.path.join(HERE, "diff.py")
    spec = importlib.util.spec_from_file_location("diff_probe", diff_path)
    diff = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(diff)

    def one(exp, act) -> str:
        p, f, r, s, v, d = diff.score({"c": exp}, {"c": act})
        return (
            ("pass" if p else "")
            + ("fail" if f else "")
            + ("refused" if r else "")
            + ("skipped" if s else "")
            + ("rejected" if v else "")
            + ("+denied" if d else "")
        )

    ok = {"id": "c", "ok": True, "result": [{"id": 1}]}
    other = {"id": "c", "ok": True, "result": [{"id": 2}]}
    denied_py = {"id": "c", "ok": False, "error": "no", "error_type": "AccessError"}
    denied_rs = {"id": "c", "ok": False, "error": "access denied"}
    missing = {"id": "c", "ok": False, "error": "KeyError", "error_type": "KeyError"}
    check("equal results pass", one(ok, ok), "pass")
    check("different results fail", one(ok, other), "fail")
    check("kernel declined is refused", one(ok, missing), "refused")
    check("kernel answered where python raised", one(missing, ok), "fail")
    check("both denied is a pass, and denied", one(denied_py, denied_rs), "pass+denied")
    check("both failed otherwise is rejected", one(missing, missing), "rejected")
    p, _f, _r, _s, v, _d = diff.score({"c": missing}, {"c": missing})
    check("rejected is not a pass", (len(p), len(v)), (0, 1))
    p, f, _r, _s, _v, _d = diff.score({"c": ok}, {})
    check("missing actual fails", (len(p), len(f)), (0, 1))
    absent = {"id": "c", "ok": False, "skipped": "model x.y is not installed"}
    check(
        "inapplicable case the kernel also declines is skipped, not rejected",
        one(absent, missing),
        "skipped",
    )
    check("inapplicable case the kernel answers is a failure", one(absent, ok), "fail")


def main() -> int:
    if engine_py is None:
        print("SHIMS FAILED: engine_py is not importable (%s)" % IMPORT_ERROR)
        return 2
    tests = [
        (name, fn) for name, fn in sorted(globals().items()) if name.startswith("test_")
    ]
    failures, skipped = [], []
    for name, fn in tests:
        try:
            fn()
        except unittest.SkipTest as exc:
            skipped.append("%s: %s" % (name, exc))
        except AssertionError as exc:
            failures.append("%s: %s" % (name, exc))
        except Exception as exc:
            failures.append("%s raised %s: %s" % (name, type(exc).__name__, exc))
    for line in skipped:
        print("  SKIP %s" % line)
    for line in failures:
        print("  FAIL %s" % line)
    ran = len(tests) - len(skipped)
    print(
        "SHIMS %s (%d tests, %d skipped)"
        % ("OK" if not failures else "FAILED (%d)" % len(failures), ran, len(skipped))
    )
    return 1 if failures else 0


def test_routed_records_keep_pythons_key_order() -> None:
    orm_shim = _shims()[1]

    def field(scan):
        f = F("char")
        f.cache_is_read_value = scan
        f.translate = False
        return f

    class M:
        _fields = {
            "name": field(True),
            "country_id": field(False),
            "company_id": field(False),
            "is_company": field(True),
            "write_date": field(True),
        }

    records = [
        {
            "id": 1,
            "name": "a",
            "country_id": (1, "BE"),
            "company_id": False,
            "is_company": True,
            "write_date": "x",
        },
        {
            "id": 2,
            "name": "b",
            "country_id": False,
            "company_id": (2, "Co"),
            "is_company": False,
            "write_date": "y",
        },
    ]
    ordered = orm_shim._python_key_order(M(), records)
    check(
        "scalars first in the requested order, relational fields after them",
        [list(r) for r in ordered],
        [["id", "name", "is_company", "write_date", "country_id", "company_id"]] * 2,
    )
    check("values travel with their keys", ordered[1]["company_id"], (2, "Co"))
    already = [{"id": 3, "name": "c", "write_date": "z", "country_id": False}]
    check(
        "an answer already in that order is returned as is",
        orm_shim._python_key_order(M(), already) is already,
        True,
    )
    check("an empty answer is untouched", orm_shim._python_key_order(M(), []), [])


def test_display_name_exact_precedence_rewrites_the_leaf() -> None:
    orm_shim = _shims()[1]

    class Hits:
        def __init__(self, ids) -> None:
            self.ids = ids

        def __bool__(self) -> bool:
            return bool(self.ids)

    class Users:
        _display_name_search_exact = ("login",)
        _rec_names_search = None
        _rec_name = "name"
        _fields = {"name": F("char"), "login": F("char")}

        def __init__(self, hits) -> None:
            self.hits, self.searched = hits, []

        def _is_rec_names_search_cyclic(self, fname) -> bool:  # noqa: ARG002  the real signature
            return False

        def search(self, domain):
            self.searched.append(list(domain))
            return Hits(self.hits)

    Users._fields["name"].relational = False
    resolve = orm_shim._resolve_display_name_exact
    hit = Users([7, 9])
    check(
        "an exact login is the whole answer",
        resolve(hit, [("display_name", "ilike", "admin"), ("active", "=", True)]),
        [("id", "in", [7, 9]), ("active", "=", True)],
    )
    check(
        "the exact search is on the declared field",
        hit.searched,
        [[("login", "in", ["admin"])]],
    )
    miss = Users([])
    check(
        "a miss is the default composition",
        resolve(miss, [("display_name", "ilike", "adm")]),
        [("name", "ilike", "adm")],
    )
    check(
        "a list value searches every login",
        resolve(Users([3]), [("display_name", "in", ["a", "b"])]),
        [("id", "in", [3])],
    )
    untouched = Users([7])
    check(
        "other operators and empty values stay for the kernel",
        resolve(
            untouched, [("display_name", "=", "admin"), ("display_name", "ilike", "")]
        ),
        [("display_name", "=", "admin"), ("display_name", "ilike", "")],
    )
    check("no search ran for them", untouched.searched, [])

    class Plain(Users):
        _display_name_search_exact = ()

    check(
        "a model without the declaration is untouched",
        resolve(Plain([1]), [("display_name", "ilike", "x")]),
        [("display_name", "ilike", "x")],
    )


def test_pool_identity_keeps_libpq_options_and_rejects_other_hosts() -> None:
    shim = _shims()[0]
    saved = shim.CONNINFO, shim.PSYCOPG_CONNINFO
    info = {
        "host": "/tmp",
        "user": "u",
        "dbname": "mydb",
        "keepalives": 1,
        "keepalives_idle": 60,
        "min_protocol_version": "3.0",
    }
    try:
        shim.CONNINFO = "host=/tmp user=u dbname=mydb"
        shim.PSYCOPG_CONNINFO = info
        check(
            "pool uses native for matching libpq identity",
            shim._pool_intercepts("", {**info, "autocommit": False}),
            True,
        )
        check(
            "pool delegates a different host",
            shim._pool_intercepts("", {**info, "host": "/elsewhere"}),
            False,
        )
        check(
            "pool delegates a different database",
            shim._pool_intercepts("", {**info, "dbname": "other"}),
            False,
        )
    finally:
        shim.CONNINFO, shim.PSYCOPG_CONNINFO = saved


def test_pool_drain_retires_borrowed_connections() -> None:
    from psycopg_pool import PoolTimeout

    shim = _shims()[0]
    pool = shim._RustPool(max_size=1)
    pool._new_connection = lambda: shim.FakeConnection(_RustConn())
    conn = pool.getconn()
    try:
        pool.getconn(timeout=0)
    except PoolTimeout:
        pass
    else:
        raise AssertionError("pool exceeded its connection limit")
    pool.drain()
    pool.putconn(conn)
    check("drain retires a connection borrowed before DDL", conn.closed, True)
    fresh = pool.getconn()
    check("next checkout opens a fresh connection", fresh is conn, False)
    pool.putconn(fresh)
    check("current generation is reused", pool.getconn() is fresh, True)
    pool.putconn(fresh)
    pool.close()
    check("close retires idle connections", fresh.closed, True)


if __name__ == "__main__":
    sys.exit(main())
