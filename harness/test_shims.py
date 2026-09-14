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
sys.path.insert(0, str(HERE))

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
    def __init__(
        self, type_, store=True, related=None, comodel_name=None, domain=None
    ) -> None:
        self.type, self.store, self.related = type_, store, related
        self.comodel_name, self.domain = comodel_name, domain
        self.column_type = (
            (type_, type_) if store and type_ not in ("one2many", "many2many") else None
        )


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
        self.in_failed_transaction = False
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


def test_float8_sums_agree_up_to_summation_order_and_nothing_else() -> None:
    orm_shim = _shims()[1]

    def column(type_, column_type):
        f = F(type_)
        f.column_type = (column_type, column_type)
        return f

    class M:
        _name = "probe.aggregates"
        _fields = {
            "hours": column("float", "float8"),
            "amount": column("float", "numeric"),
            "stage_id": column("many2one", "int4"),
        }

    groupby = ["stage_id"]
    aggregates = ["hours:sum", "hours:max", "amount:sum", "hours:avg"]
    same = orm_shim._group_rows_agree(M(), groupby, aggregates)
    python = [(7, 16.8, 8.4, 16.8, 5.6)]
    check(
        "0.1 + 0.2 + 0.3 depends on the order",
        (repr((0.1 + 0.2) + 0.3), repr(0.1 + (0.2 + 0.3))),
        ("0.6000000000000001", "0.6"),
    )
    check(
        "a float8 sum and average differing by summation order agree",
        same([(7, 16.799999999999997, 8.4, 16.8, 5.6000000000000005)], python),
        True,
    )
    check(
        "a float8 sum off by a whole value does not",
        same([(7, 17.8, 8.4, 16.8, 5.6)], python),
        False,
    )
    check(
        "a numeric sum compares exactly",
        same([(7, 16.8, 8.4, 16.799999999999997, 5.6)], python),
        False,
    )
    check(
        "max is not order dependent",
        same([(7, 16.8, 8.400000000000002, 16.8, 5.6)], python),
        False,
    )
    check(
        "group keys compare exactly", same([(8, 16.8, 8.4, 16.8, 5.6)], python), False
    )
    check("a missing group does not agree", same([], python), False)
    check(
        "no float8 sum keeps plain equality",
        orm_shim._group_rows_agree(M(), groupby, ["amount:sum"])
        is orm_shim.operator.eq,
        True,
    )

    formatted = orm_shim._formatted_groups_agree(M(), ["hours:sum", "__count"])
    python = [
        {"stage_id": (7, "New"), "__extra_domain": [], "hours:sum": 16.8, "__count": 3}
    ]
    check(
        "formatted groups agree up to summation order",
        formatted(
            [
                {
                    "stage_id": (7, "New"),
                    "__extra_domain": [],
                    "hours:sum": 16.799999999999997,
                    "__count": 3,
                }
            ],
            python,
        ),
        True,
    )
    check(
        "formatted groups compare counts exactly",
        formatted(
            [
                {
                    "stage_id": (7, "New"),
                    "__extra_domain": [],
                    "hours:sum": 16.8,
                    "__count": 4,
                }
            ],
            python,
        ),
        False,
    )
    check(
        "formatted groups compare labels exactly",
        formatted(
            [
                {
                    "stage_id": (7, ""),
                    "__extra_domain": [],
                    "hours:sum": 16.8,
                    "__count": 3,
                }
            ],
            python,
        ),
        False,
    )


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


def test_web_many2ones_resolve_through_web_read() -> None:
    # web_read keeps an unreadable many2one target as its id, where read()
    # redacts it to False. A visible target's kernel label is also web_read's
    # answer; a hidden one, and a comodel whose name Python computes, go to
    # web's own resolver, and only those rows do.
    orm_shim = _shims()[1]
    calls = []

    class M:
        _fields = {
            "partner_id": F("many2one"),
            "user_id": F("many2one"),
            "group_id": F("many2one"),
        }

        def _web_read_resolve_many2one(self, values_list, field, name, spec) -> None:
            calls.append((name, field.type, bool(spec), [v["id"] for v in values_list]))
            for vals in values_list:
                vals[name] = {"id": vals[name]}

    recs = [
        {"id": 1, "partner_id": (7, "Seven"), "user_id": 2, "group_id": 5},
        {"id": 2, "partner_id": 8, "user_id": False, "group_id": False},
        {"id": 3, "partner_id": False, "user_id": 3, "group_id": 6},
    ]
    named = {"fields": {"display_name": {}}}
    spec = {"partner_id": named, "user_id": None, "group_id": named}
    out = orm_shim._web_resolve_many2ones(
        M(), recs, spec, raw=["user_id", "group_id"], unredacted=["partner_id"]
    )
    check(
        "the resolver sees a named raw column whole and only hidden labelled rows",
        calls,
        [
            ("group_id", "many2one", True, [1, 2, 3]),
            ("partner_id", "many2one", True, [2]),
        ],
    )
    check(
        "a visible label is web_read's value",
        out[0]["partner_id"],
        {"id": 7, "display_name": "Seven"},
    )
    check("a hidden target is resolved by web", out[1]["partner_id"], {"id": 8})
    check("an empty many2one stays False", out[2]["partner_id"], False)
    check(
        "a plain many2one keeps the raw id", [r["user_id"] for r in out], [2, False, 3]
    )


def test_an_order_changed_after_the_kernel_was_built_refuses() -> None:
    orm_shim = _shims()[1]

    class Env(dict):
        pass

    env = Env()

    class Model:
        def __init__(self, name, order, fields) -> None:
            self._name, self._order, self._fields, self.env = name, order, fields, env

    env["res.users"] = Model("res.users", "login", {"partner_id": F("many2one")})
    env["res.users"]._fields["partner_id"].comodel_name = "res.partner"
    env["res.partner"] = Model("res.partner", "name", {"country_id": F("many2one")})
    env["res.partner"]._fields["country_id"].comodel_name = "res.country"
    env["res.country"] = Model("res.country", "name", {})
    saved = dict(orm_shim.ORDERS)
    try:
        orm_shim.ORDERS.clear()
        orm_shim.ORDERS.update(
            {"res.users": "login", "res.partner": "name", "res.country": "name"}
        )
        drifted = orm_shim._order_drifted
        users = env["res.users"]
        check("unchanged orders route", drifted(users, None, ["partner_id"]), False)
        env["res.partner"]._order = "country_id, id"
        check(
            "a groupby comodel's changed _order refuses",
            drifted(users, None, ["partner_id"]),
            True,
        )
        env["res.partner"]._order = "name"
        env["res.country"]._order = "id"
        check(
            "...and one its _order chains to is not reached",
            drifted(users, None, ["partner_id"]),
            False,
        )
        env["res.partner"]._order = "country_id"
        orm_shim.ORDERS["res.partner"] = "country_id"
        check(
            "a comodel reached through an exported _order is checked",
            drifted(users, "partner_id", ()),
            True,
        )
    finally:
        orm_shim.ORDERS.clear()
        orm_shim.ORDERS.update(saved)


def test_an_incomplete_registry_does_not_spend_the_kernel_build() -> None:
    orm_shim = _shims()[1]
    built = []

    class Registry:
        models = {"a": 1}

    class Env:
        registry = Registry()

    def factory(registry):
        built.append(len(registry.models))
        if len(registry.models) < 2:
            raise RuntimeError("refusing to write an incomplete export: 1 of 2")
        return "kernel"

    saved = (
        orm_shim.KERNEL,
        orm_shim.KERNEL_FACTORY,
        orm_shim._KERNEL_TRIED_PID,
        orm_shim.PROCESS_HOOK,
        orm_shim._KERNEL_INCOMPLETE[0],
    )
    try:
        orm_shim.KERNEL, orm_shim.PROCESS_HOOK = None, None
        orm_shim._KERNEL_TRIED_PID = None
        orm_shim._KERNEL_INCOMPLETE[0] = None
        orm_shim.KERNEL_FACTORY = factory
        env = Env()
        check(
            "no kernel from an incomplete registry", orm_shim._ensure_kernel(env), False
        )
        check("...asked again, no second export", orm_shim._ensure_kernel(env), False)
        check("...one attempt so far", built, [1])
        check("...and the attempt is not spent", orm_shim._KERNEL_TRIED_PID, None)
        Registry.models = {"a": 1, "b": 2}
        check("the grown registry builds it", orm_shim._ensure_kernel(env), True)
        check("...on the second export", built, [1, 2])
    finally:
        (
            orm_shim.KERNEL,
            orm_shim.KERNEL_FACTORY,
            orm_shim._KERNEL_TRIED_PID,
            orm_shim.PROCESS_HOOK,
            orm_shim._KERNEL_INCOMPLETE[0],
        ) = saved


def test_a_domain_through_a_python_search_method_is_resolved_first() -> None:
    orm_shim = _shims()[1]

    class Field:
        def __init__(self, type_, store=True, search=None, related=None, comodel=None):
            self.type, self.store, self.search, self.related = (
                type_,
                store,
                search,
                related,
            )
            self.relational = type_ in ("many2one", "one2many", "many2many")
            self.comodel_name = comodel

    class Env(dict):
        pass

    env = Env()

    class Model:
        def __init__(self, fields) -> None:
            self._fields, self.env = fields, env

    env["member"] = Model({"partner_id": Field("many2one", comodel="partner")})
    env["partner"] = Model({"name": Field("char")})
    env["channel"] = Model(
        {
            "name": Field("char"),
            "is_member": Field("boolean", store=False, search="_search_is_member"),
            "member_ids": Field("one2many", comodel="member"),
            "display_name": Field("char", store=False, search="_search_display_name"),
            "label": Field("char", store=False, search="_search_label", related="name"),
        }
    )
    env["member"]._fields["channel_id"] = Field("many2one", comodel="channel")
    needs = orm_shim._needs_python_search
    channel, member = env["channel"], env["member"]
    check("a stored column needs nothing", needs(channel, [("name", "=", "x")]), False)
    check(
        "a search method is resolved", needs(channel, [("is_member", "=", True)]), True
    )
    check(
        "...through a dotted path",
        needs(
            member,
            ["|", ("partner_id.name", "=", "a"), ("channel_id.is_member", "=", True)],
        ),
        True,
    )
    check(
        "...and inside an any",
        needs(member, [("channel_id", "any", [("is_member", "=", True)])]),
        True,
    )
    check(
        "display_name is the kernel's",
        needs(channel, [("display_name", "ilike", "a")]),
        False,
    )
    check(
        "a related field is the kernel's", needs(channel, [("label", "=", "a")]), False
    )


def test_web_length() -> None:
    orm_shim = _shims()[1]

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


def test_wire_encoder() -> None:
    import datetime
    import json

    from odoo.tools import SQL

    orm_shim = _shims()[1]
    encode = orm_shim._wire_encoder("n")
    check(
        "a fragment carries its code, params and the request's nonce",
        json.loads(json.dumps(SQL("SELECT id FROM t WHERE p = %s", 7), default=encode)),
        {"$sql": "SELECT id FROM t WHERE p = %s", "$params": [7], "$nonce": "n"},
    )
    for label, param in (
        ("a datetime", datetime.datetime(2026, 9, 14, 8)),
        ("an empty list", []),
        ("None", None),
        ("a mixed list", [1, "a"]),
    ):
        try:
            json.dumps(SQL("SELECT %s", param), default=encode)
        except orm_shim.KernelRefused:
            pass
        else:
            raise AssertionError("a SQL comparand binding %s was sent" % label)
    zone = datetime.timezone(datetime.timedelta(hours=2))
    check(
        "a datetime is its naive UTC timestamp, the way Python compares it",
        json.dumps(
            [
                datetime.datetime(2026, 9, 14, 10, 0, 0, 500, tzinfo=zone),
                datetime.datetime(2026, 9, 14, 8),
                datetime.date(2026, 9, 14),
            ],
            default=encode,
        ),
        '["2026-09-14 08:00:00.000500", "2026-09-14 08:00:00", "2026-09-14"]',
    )
    try:
        json.dumps(object(), default=encode)
    except orm_shim.KernelRefused:
        pass
    else:
        raise AssertionError("an object with no wire form was sent")


def test_web_spec_plan() -> None:
    orm_shim = _shims()[1]

    def base_search(_self, domain):
        return domain

    class Comodel:
        _search = base_search
        _fields = {
            "currency_id": F("many2one", comodel_name="probe.tag"),
            "permission": F("selection", store=False),
        }

        def sudo(self):
            return self

    class SearchingComodel(Comodel):
        def _search(self, domain):
            return domain

    orm_shim._BASE_METHODS["_search"] = base_search

    class M:
        _name = "probe.web_spec"
        env = {
            "probe.tag": Comodel(),
            "probe.activity": SearchingComodel(),
            "probe.company": Comodel(),
        }
        _fields = {
            "name": F("char"),
            "partner_id": F("many2one"),
            "user_id": F("many2one"),
            "tag_ids": F("many2many", comodel_name="probe.tag"),
            "activity_ids": F("one2many", comodel_name="probe.activity"),
            "member_ids": F(
                "many2many", comodel_name="probe.tag", domain=lambda _env: []
            ),
            "ref_id": F("reference"),
            "props": F("properties"),
            "icon": F("char", store=False),
            "company_id": F("many2one", comodel_name="probe.company"),
            "cur": F("many2one", store=False, related="company_id.currency_id"),
            "perm": F("selection", store=False, related="company_id.permission"),
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
        (["name", "partner_id", "user_id", "tag_ids"], ["partner_id", "user_id"], {}),
    )
    check("unknown field refuses", plan(M(), {"nope": {}}), None)
    # a refused plan says why, or the gate log reads it as "call shape"
    check("...and records why", orm_shim._GATE_TL.reason, "unknown field nope")
    orm_shim._GATE_TL.reason = None
    for label, spec in (
        (
            "m2o with extra sub-field",
            {"partner_id": {"fields": {"display_name": {}, "email": {}}}},
        ),
        (
            "m2o with context",
            {"partner_id": {"fields": {"display_name": {}}, "context": {"x": 1}}},
        ),
        ("x2many with fields", {"tag_ids": {"fields": {"name": {}}}}),
        ("x2many with order", {"tag_ids": {"order": "name"}}),
        ("x2many with limit", {"tag_ids": {"limit": 5}}),
        ("reference with spec", {"ref_id": {"fields": {}}}),
        ("a bare reference", {"ref_id": {}}),
        ("properties with spec", {"props": {"fields": {}}}),
        ("a compute", {"icon": {}}),
        ("an x2many whose comodel searches in python", {"activity_ids": {}}),
        ("an x2many whose domain is computed", {"member_ids": {}}),
        ("a related through a compute", {"perm": {}}),
    ):
        check("%s is left to web_read" % label, plan(M(), spec), (["id"], [], spec))
    check(
        "a mixed spec splits, the kernel keeping what it reads",
        plan(M(), {"icon": {}, "name": {}, "partner_id": {}}),
        (["name", "partner_id"], ["partner_id"], {"icon": {}}),
    )
    check(
        "a related non-stored field maps",
        plan(M(), {"cur": {}}),
        (["cur"], ["cur"], {}),
    )
    merged = orm_shim._web_merge(
        {"icon": {}, "name": {}},
        [{"id": 2, "name": "b"}, {"id": 1, "name": "a"}],
        [{"id": 1, "icon": "x"}, {"id": 2, "icon": "y"}],
    )
    check(
        "the halves merge by id, in the specification's order",
        [list(rec.items()) for rec in merged],
        [
            [("id", 2), ("icon", "y"), ("name", "b")],
            [("id", 1), ("icon", "x"), ("name", "a")],
        ],
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

    class _Stored:
        rows = {1, 2, 3}
        _fields = {
            "name": F("char"),
            "partner_id": F("many2one"),
            "tag_ids": F("many2many", store=True),
            "label": F("char", store=False),
        }

        def browse(self, ids):
            present = [i for i in ids if i in self.rows]

            class _Records:
                def exists(self):
                    return present

            return _Records()

    def rr(ids, records, load, fields=("name",)):
        return orm_shim._read_reorder(_Stored(), ids, records, load, fields)

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
    check("a row the kernel hid is not answered", rr([1, 3], recs, None), None)
    check(
        "an id with no row is dropped when a column is read, as read drops it",
        [r["id"] for r in rr([9, 1, 8], recs, None)],
        [1],
    )
    check(
        "an id with no row is kept by read when no column is read, so it refuses",
        rr([9, 1], recs, None, ("tag_ids", "label")),
        None,
    )
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

    class _Rules:
        def _get_domain_accessible_records(self, *_args):
            return Domain.TRUE

    class _Env:
        uid, su, context = 2, False, {}

        class registry:
            registry_sequence = 7
            ormcache_lrus = {}

        def __getitem__(self, model_name):
            return _Rules()

    class _Model:
        _name = "res.partner"
        _fields = {}
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


def test_an_idle_connection_failing_its_check_is_replaced_not_raised() -> None:
    import psycopg

    shim = _shims()[0]
    checked = []

    def check_connection(conn):
        checked.append(conn)
        if len(checked) == 1:
            raise psycopg.OperationalError("connection closed")

    pool = shim._RustPool(max_size=1, check=check_connection)
    pool._new_connection = lambda: shim.FakeConnection(_RustConn())
    dead = pool.getconn()
    pool.putconn(dead)
    replacement = pool.getconn(timeout=1)
    check("the idle connection was checked", checked[:1], [dead])
    check("the connection that failed its check is closed", dead.closed, True)
    check("a fresh connection is handed out instead", replacement is dead, False)
    check("its slot was not leaked", pool._out, 1)
    pool.putconn(replacement)
    check("a connection that passes is reused", pool.getconn() is replacement, True)
    pool.putconn(replacement)
    pool.close()


def test_a_refused_connection_waits_like_psycopg_pool_then_times_out() -> None:
    from psycopg_pool import PoolTimeout

    shim = _shims()[0]
    pool = shim._RustPool(max_size=2)
    attempts = []

    def refuse_twice():
        attempts.append(1)
        if len(attempts) <= 2:
            raise RuntimeError("db error: FATAL: sorry, too many clients already")
        return shim.FakeConnection(_RustConn())

    pool._new_connection = refuse_twice
    conn = pool.getconn(timeout=5)
    check("a refused connection is retried until it opens", len(attempts), 3)
    pool.putconn(conn)

    pool._new_connection = lambda: (_ for _ in ()).throw(
        RuntimeError("db error: FATAL: sorry, too many clients already")
    )
    pool._idle.clear()
    try:
        pool.getconn(timeout=0.2)
    except PoolTimeout as exc:
        check(
            "...and times out as psycopg_pool does",
            "too many clients" in str(exc),
            True,
        )
    else:
        raise AssertionError("a pool that cannot connect returned a connection")
    check("the failed checkout gave its slot back", pool._out, 0)
    pool.close()


def test_every_kernel_failure_path_reports_somewhere() -> None:
    # The completeness half of a census, which a null control does NOT imply:
    # a corpus reading zero refusals says nothing about a site that refuses and
    # never reports. `refuse!` / `refusal!` / `deny_access!` cover every site
    # that uses them by construction, so what this looks for is the failure
    # paths that BYPASS them -- a bare `bail!` or `anyhow!` on the read path is
    # invisible to `odoo_kernel::refusal` and to every other target.
    #
    # Each one listed here is either logged at its site or answered at a
    # boundary that logs; a NEW one is neither until somebody decides which.
    import pathlib
    import re

    root = pathlib.Path(__file__).resolve().parent.parent
    pattern = re.compile(r"\b(?:anyhow::)?(?:bail!|anyhow!)\(")
    found = []
    for rel in ("kernel/src", "engine-py/src", "server/src"):
        for path in sorted((root / rel).rglob("*.rs")):
            if path.name == "error.rs" or "/bin/" in str(path):
                continue
            for n, line in enumerate(path.read_text().splitlines(), 1):
                if pattern.search(line) and not line.lstrip().startswith("//"):
                    found.append("%s:%d" % (path.relative_to(root), n))
    # kernel: two internal invariants, both logged at `error` on their own
    # subsystem target; connect: dsn rejections, logged by the parser.
    # server: every one is answered by handle_call, which logs status and kind.
    known = 14
    check(
        "failure paths bypassing the refusal macros (%s)" % ", ".join(found),
        len(found),
        known,
    )


def _backend():
    if engine_py is None:
        raise unittest.SkipTest("engine_py is not importable (%s)" % IMPORT_ERROR)
    _odoo()
    return engine_py.install_backend()


def _protocol_members():
    from odoo.orm.runtime.backend import StorageBackend

    methods = sorted(
        name
        for name, value in vars(StorageBackend).items()
        if not name.startswith("_") and callable(value)
    )
    flags = sorted(
        name
        for name in getattr(StorageBackend, "__annotations__", {})
        if not name.startswith("_")
    )
    return methods, flags


def test_the_port_covers_the_forks_protocol_and_nothing_else() -> None:
    # The point of implementing StorageBackend rather than patching methods on
    # BaseModel is that the fork DECLARES this surface and pins it
    # (odoo/orm/tests/test_backend_dispatch_surface.py). A method added to the
    # protocol upstream has to reach RustBackend, and the failure mode if it
    # does not is that the attribute lookup falls through to nothing -- so
    # read the protocol here rather than restating it.
    backend = _backend()
    methods, flags = _protocol_members()
    check("protocol methods", sorted(backend.PROTOCOL_METHODS), methods)
    check("protocol flags", sorted(backend.PROTOCOL_FLAGS), flags)
    missing = [
        name for name in methods + flags if not hasattr(backend.RustBackend, name)
    ]
    check("RustBackend implements every member (%s)" % ", ".join(missing), missing, [])


def _shape(fn):
    # Parameter name, kind and default -- the part a CALLER can observe. The
    # annotations are not compared: the fork annotates its ORM and this
    # repository does not, this not being one of the fork's core packages, so
    # comparing them would fail on a difference no call can see.
    import inspect

    return [
        (p.name, p.kind, p.default) for p in inspect.signature(fn).parameters.values()
    ]


def test_the_ports_signatures_match_the_delegates() -> None:
    # A delegating wrapper whose signature has drifted does not fail at import:
    # it fails at the one call site that passes the argument it dropped, which
    # on this port is a write.
    from odoo.orm.runtime.backend import PostgresBackend

    backend = _backend()
    wrong = []
    for name in ("create_rows", "update_rows", "search"):
        mine = _shape(getattr(backend.RustBackend, name))
        theirs = _shape(getattr(PostgresBackend, name))
        if mine != theirs:
            wrong.append("%s: %s != %s" % (name, mine, theirs))
    check("signatures (%s)" % "; ".join(wrong), wrong, [])


class _RecordingDelegate:
    supports_recursive_queries = True

    def __init__(self) -> None:
        self.calls = []

    def __getattr__(self, name):
        def record(*args, **kwargs):
            self.calls.append((name, args, kwargs))
            return ("delegated", name)

        return record


def test_an_unarmed_port_is_its_delegate() -> None:
    # The port is installed before any method is native, so "installed"
    # must mean "no answer changed". Every protocol method is called here,
    # including the ones with keyword-only arguments, because a wrapper that
    # drops `check_access` would route a search AROUND the record rules.
    backend = _backend()

    class _Unarmed(backend.RustBackend):
        # What the port is on a worker where nothing has been armed yet, which
        # is the state every new native method starts in.
        NATIVE = frozenset()

    delegate = _RecordingDelegate()
    port = _Unarmed(delegate)
    backend.reset_stats()

    calls = {
        "create_rows": (("model", ["stored"], ["col"], ["field"]), {}),
        "update_rows": (("model", ("name",), [(1, "x")]), {}),
        "fetch": (("model", "query", ["a"], ["b"]), {}),
        "search": (
            ("model", "domain", 0, None, None),
            {"check_access": False, "prof": "p"},
        ),
        "as_query": (("model", False), {}),
        "descendants": (
            ("model", "parent_id", [1]),
            {"domain": "d", "step_domain": "s", "same_columns": ("a",)},
        ),
        "read_group_rows": (
            ("model", "select"),
            {
                "domain": "d",
                "query": "q",
                "groupby": ["g"],
                "aggregates": ["__count"],
                "having": None,
                "order": None,
                "limit": None,
                "offset": 0,
            },
        ),
        "get_existing_ids": (("model", [1, 2]), {}),
        "lock_for_update": (("model",), {"allow_referencing": True}),
        "try_lock_for_update": (("model",), {"allow_referencing": True, "limit": 3}),
        "unlink_rows": (("model", (1,), "Defaults", "Attachment"), {}),
        "link_m2m_pairs": (("model", "rel", "c1", "c2", [(1, 2)]), {}),
        "unlink_m2m_pairs": (("model", "rel", "c1", "c2", [(1, 2)]), {}),
        "read_m2m_groups": (("records", "rel", "c1", "c2", "query"), {}),
        "set_parent_paths": (("model", [1]), {}),
        "move_parent_paths": (("model", [1], "1/"), {}),
        "ancestors": (("model", "parent_id", [1]), {}),
        "records_with_parent_changed": (("model", {1: [2]}), {}),
        "timezone_names": (("env",), {}),
        "count_m2m_groups": (("records", "rel", "c1", "c2", "query"), {}),
    }
    check(
        "every protocol method is exercised",
        sorted(calls),
        sorted(backend.PROTOCOL_METHODS),
    )
    for name, (args, kwargs) in calls.items():
        got = getattr(port, name)(*args, **kwargs)
        check("%s returns the delegate's answer" % name, got, ("delegated", name))

    seen = {name for name, _args, _kwargs in delegate.calls}
    check("every call reached the delegate", sorted(seen), sorted(calls))
    for name, (args, kwargs) in calls.items():
        recorded = next(c for c in delegate.calls if c[0] == name)
        # positional-vs-keyword is not preserved by the port and does not need
        # to be; what must survive is the VALUES, keyword-only ones included.
        passed = dict(zip(("a", "b", "c", "d", "e"), recorded[1], strict=False))
        passed.update(recorded[2])
        wanted = dict(zip(("a", "b", "c", "d", "e"), args, strict=False))
        wanted.update(kwargs)
        check("%s forwards its arguments" % name, passed, wanted)

    stats = backend.stats()
    check("nothing was answered natively", stats["native"], {})
    check(
        "every call is counted as delegated",
        sum(stats["delegated"].values()),
        len(calls),
    )
    check("the native share is zero", stats["native_share"], 0.0)


def test_the_port_mirrors_its_delegates_support_flags() -> None:
    backend = _backend()
    _methods, flags = _protocol_members()
    delegate = _RecordingDelegate()
    port = backend.RustBackend(delegate)
    for flag in flags:
        for value in (True, False):
            setattr(delegate, flag, value)
            check(
                "%s mirrors the delegate (%r)" % (flag, value),
                getattr(port, flag),
                value,
            )


def test_a_protocol_member_the_port_does_not_know_is_delegated() -> None:
    backend = _backend()

    class Delegate:
        supports_something_new = True

        def something_new(self, model):
            return ("delegate", model)

    port = backend.RustBackend(Delegate())
    before = backend.STATS["delegated"]["something_new"]
    check("a new flag is the delegate's", port.supports_something_new, True)
    check("a new method is the delegate's", port.something_new("m"), ("delegate", "m"))
    check(
        "...and counted as delegated",
        backend.STATS["delegated"]["something_new"] > before,
        True,
    )
    import copy

    check(
        "copying the port does not recurse", type(copy.copy(port)), backend.RustBackend
    )


def test_installing_the_port_leaves_the_in_memory_backend_alone() -> None:
    # InMemoryBackend is how the ORM runs with no database. Wrapping it would
    # put a kernel that needs a connection in front of the case defined by not
    # having one.
    backend = _backend()
    from odoo.orm.runtime.backend import POSTGRES_BACKEND

    orig_init = None
    try:
        backend.install(dbname=None)
        orig_init = backend.installed()["orig_init"]

        class _Registry:
            db_name = "any"

        class _T:
            backend = None

        from odoo.orm.runtime.transaction import Transaction

        made = object.__new__(Transaction)
        Transaction.__init__(made, _Registry())
        check(
            "a postgres transaction gets the port",
            type(made.backend).__name__,
            "RustBackend",
        )
        check(
            "and the port wraps the postgres backend",
            made.backend.delegate,
            POSTGRES_BACKEND,
        )

        storage = {}
        made2 = object.__new__(Transaction)
        Transaction.__init__(made2, _Registry(), storage)
        check(
            "an in-memory transaction keeps its own backend",
            type(made2.backend).__name__,
            "InMemoryBackend",
        )
    finally:
        backend.uninstall()
    from odoo.orm.runtime.transaction import Transaction

    check("uninstall restores the original", Transaction.__init__ is orig_init, True)
    check("and forgets it installed", backend.installed(), None)


def test_the_port_only_arms_for_the_database_it_was_built_for() -> None:
    # The same process can hold transactions for more than one database (the
    # database manager does), and a kernel built from one database's registry
    # must not answer for another.
    backend = _backend()
    try:
        backend.install(dbname="the_one")
        from odoo.orm.runtime.transaction import Transaction

        class _Registry:
            def __init__(self, name) -> None:
                self.db_name = name

        mine = object.__new__(Transaction)
        Transaction.__init__(mine, _Registry("the_one"))
        check(
            "the armed database gets the port",
            type(mine.backend).__name__,
            "RustBackend",
        )

        other = object.__new__(Transaction)
        Transaction.__init__(other, _Registry("another"))
        check(
            "another database does not",
            type(other.backend).__name__,
            "PostgresBackend",
        )
    finally:
        backend.uninstall()
        backend.DBNAME = None


def test_the_fork_still_composes_the_writes_the_contract_pins() -> None:
    # The other half of `kernel/tests/pure.rs::the_kernel_composes_the_update_
    # the_forks_backend_composes`. That one asks the kernel; this one asks the
    # FORK, so the statement is derived twice and neither derivation is
    # checked against a copy of itself. A fork that starts composing something
    # else fails HERE, and stays failing in Rust until the kernel is taught it.
    _odoo()
    import write_sql_contract

    contract = write_sql_contract.load()
    check("the contract names update cases", bool(contract["cases"]), True)
    check("the contract names insert cases", bool(contract["insert_cases"]), True)
    for case in contract["cases"]:
        check(
            "the fork composes %r" % case["name"],
            write_sql_contract.compose(contract, case),
            case["sql"],
        )
    for case in contract["insert_cases"]:
        check(
            "the fork composes %r" % case["name"],
            write_sql_contract.compose_insert(contract, case),
            case["sql"],
        )


def test_the_port_arms_only_what_the_contract_covers() -> None:
    # `NATIVE` is the arming switch, and the reason it is separate from the
    # implementation is that an implementation with no verification behind it
    # must stay off. Every armed method needs a line here saying what verifies
    # it; a method armed without one fails this.
    backend = _backend()
    verified_by = {
        "update_rows": "harness/write_sql_contract.json, both derivations",
        "create_rows": "write_sql_contract.json insert_cases; write_path.py creates",
    }
    unverified = sorted(set(backend.RustBackend.NATIVE) - set(verified_by))
    check(
        "armed with nothing verifying it (%s)" % ", ".join(unverified), unverified, []
    )
    check(
        "every armed method is a protocol method",
        sorted(set(backend.RustBackend.NATIVE) - set(backend.PROTOCOL_METHODS)),
        [],
    )


def test_a_column_group_the_kernel_refuses_falls_through_to_the_delegate() -> None:
    # The native path reports whether it ran, and the port delegates when it
    # did not. With no kernel in the process -- which is every worker that
    # could not arm -- that is the whole behaviour, and it has to be the
    # delegate's answer rather than a skipped write.
    backend = _backend()
    delegate = _RecordingDelegate()
    port = backend.RustBackend(delegate)
    backend.reset_stats()

    class _Model:
        env = object()

    model = _Model()
    backend.KERNEL_FOR = lambda _env: None
    try:
        got = port.update_rows(model, ("name",), [(1, "a")])
    finally:
        backend.KERNEL_FOR = None
    check("the delegate answered", got, ("delegated", "update_rows"))
    check(
        "and it was told the same thing",
        delegate.calls[-1][1],
        (model, ("name",), [(1, "a")]),
    )
    stats = backend.stats()
    check("nothing was counted native", stats["native"], {})
    check(
        "the reason names the missing kernel",
        "no kernel in this process" in stats["reasons"],
        True,
    )


class _InsertField:
    is_html = False

    def __init__(self, name, convert=None) -> None:
        self.name = name
        self._convert = convert

    def convert_to_column_insert(self, value, *_args, **_kwargs):
        return self._convert(value) if self._convert else value


class _InsertCursor:
    def __init__(self, in_pipeline=False) -> None:
        self.in_pipeline = in_pipeline
        self.executed = []

    def execute(self, sql, params=None) -> None:
        self.executed.append((sql, params))

    def fetchall(self):
        return []


class _InsertModel:
    _name = "res.partner"

    def __init__(self, cr) -> None:
        class _Env:
            pass

        self.env = _Env()
        self.env.cr = cr
        self.env.registry = type("R", (), {"registry_sequence": 7})()


class _InsertKernel:
    registry_sequence = 7

    def __init__(self) -> None:
        self.asked = []

    def insert_rows_sql(self, model, columns, row_count):
        self.asked.append((model, list(columns), row_count))
        return "INSERT %d" % row_count


def test_create_rows_splits_its_strategies_where_the_fork_does() -> None:
    # Ten rows or more outside a pipeline are COPY, which the cursor owns and
    # the port delegates; the same ten inside a pipeline are INSERT, which the
    # kernel composes. The split reads the fork's own constants, so a change to
    # COPY_THRESHOLD moves both sides together -- this pins that it does.
    backend = _backend()
    _odoo()
    from odoo.orm.runtime.backend import COPY_DISABLED, COPY_THRESHOLD

    if COPY_DISABLED:
        raise unittest.SkipTest(
            "ODOO_DISABLE_COPY is set, so there is no COPY strategy"
        )
    kernel = _InsertKernel()
    backend.KERNEL_FOR = lambda _env: kernel
    columns = ["name"]
    fields = [_InsertField("name")]
    try:
        for in_pipeline, count, want in (
            (False, COPY_THRESHOLD, "delegated"),
            (True, COPY_THRESHOLD, "native"),
            (False, COPY_THRESHOLD - 1, "native"),
        ):
            backend.reset_stats()
            kernel.asked.clear()
            cr = _InsertCursor(in_pipeline)
            got = backend._create_rows_native(
                _InsertModel(cr), [{"name": "x"}] * count, columns, fields
            )
            label = "%d rows, in_pipeline=%s" % (count, in_pipeline)
            if want == "delegated":
                check(label + " delegates", got, None)
                check(label + " never asks the kernel", kernel.asked, [])
                check(
                    label + " says why",
                    backend.stats()["reasons"],
                    {"COPY strategy: the cursor owns it": 1},
                )
            else:
                check(
                    label + " asks the kernel",
                    kernel.asked,
                    [("res.partner", columns, count)],
                )
                check(label + " runs one statement", len(cr.executed), 1)
                check(label + " binds one value per row", len(cr.executed[0][1]), count)
    finally:
        backend.KERNEL_FOR = None


def test_create_rows_delegates_a_value_that_is_not_one_parameter() -> None:
    # `SQL` inlines an SQL value as code and expands a tuple into a list, so
    # neither is a single `%s`. The kernel's statement assumes one per value,
    # and a converter returning either would change the statement's shape.
    backend = _backend()
    _odoo()
    from odoo.libs.sql.builder import SQL

    kernel = _InsertKernel()
    backend.KERNEL_FOR = lambda _env: kernel
    try:
        for odd in (SQL("DEFAULT"), (1, 2)):
            backend.reset_stats()
            cr = _InsertCursor()
            got = backend._create_rows_native(
                _InsertModel(cr),
                [{"name": "x"}],
                ["name"],
                [_InsertField("name", convert=lambda _v, odd=odd: odd)],
            )
            name = type(odd).__name__
            check("a %s value delegates" % name, got, None)
            check("and nothing ran", cr.executed, [])
            check(
                "and the reason names it",
                any(name in reason for reason in backend.stats()["reasons"]),
                True,
            )
    finally:
        backend.KERNEL_FOR = None


def test_an_extension_older_than_the_port_does_not_stop_the_engine_arming() -> None:
    # The addon and the engine_py extension are versioned apart, and the venv
    # carries a build that predates the port. Arming used to call
    # install_backend() unguarded between installing the shims and arming the
    # registry hook, so that extension left a server half armed. Every case
    # below must return normally and leave the port uninstalled.
    addon = _addon()

    class _Config(dict):
        pass

    class _OrmShim:
        KERNEL = None

    class _NoPort:
        __file__ = "old/engine_py.so"

    class _BrokenPort:
        def install_backend(self):
            raise RuntimeError("the embedded port source failed to import")

    for label, engine, config in (
        ("an extension without the port", _NoPort(), _Config()),
        ("a port that raises while installing", _BrokenPort(), _Config()),
        ("the port switched off", _NoPort(), _Config(rust_engine_port="off")),
    ):
        addon._STATE["port"] = None
        addon._arm_port(engine, _OrmShim(), config, "db")
        check("%s leaves the port uninstalled" % label, addon._STATE["port"], None)


def test_the_extension_under_test_was_built_from_this_checkout() -> None:
    # The shims are compiled into engine_py, so every other test here checks
    # whatever sources the loaded build embedded. build.rs and the addon
    # checksum the same files two ways; this is where they must agree.
    if engine_py is None:
        raise unittest.SkipTest("engine_py is not importable (%s)" % IMPORT_ERROR)
    addon = _addon()
    check(
        "the build's stamp is the checkout's checksum",
        getattr(engine_py, "__source_crc__", None),
        addon.source_crc(pathlib.Path(ROOT)),
    )
    check("the addon arms this build", addon.stale_extension(engine_py), None)


def test_a_stale_extension_is_refused() -> None:
    import shutil
    import tempfile

    addon = _addon()
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        for name, _suffix in addon.SOURCE_INPUTS:
            source = pathlib.Path(ROOT) / name
            if source.is_dir():
                shutil.copytree(
                    source, root / name, ignore=shutil.ignore_patterns("__pycache__")
                )
            elif source.is_file():
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(source, root / name)
        shutil.copy(
            pathlib.Path(ROOT) / "engine-py/build.rs", root / "engine-py/build.rs"
        )

        class _Built:
            __file__ = "engine_py.so"
            __source_crc__ = addon.source_crc(root)
            __profile__ = "release"

        class _Unstamped:
            __file__ = "engine_py.so"

        class _Debug(_Built):
            __profile__ = "debug"

        check(
            "a matching release build arms", addon.stale_extension(_Built, root), None
        )
        for label, engine in (
            ("an unstamped build", _Unstamped),
            ("a debug build", _Debug),
        ):
            check(
                "%s is refused" % label,
                addon.stale_extension(engine, root) is not None,
                True,
            )
        shim = root / "engine-py/python/rust_orm_shim.py"
        shim.write_text(shim.read_text() + "\n")
        check(
            "an edit to an embedded Python module refuses the build",
            "was built from" in (addon.stale_extension(_Built, root) or ""),
            True,
        )
        os.environ[addon.SKIP_FRESHNESS_ENV] = "1"
        try:
            check("the bypass arms it", addon.stale_extension(_Built, root), None)
        finally:
            del os.environ[addon.SKIP_FRESHNESS_ENV]
        (root / "engine-py/build.rs").unlink()
        check(
            "an addon deployed without its checkout compares nothing",
            addon.stale_extension(_Unstamped, root),
            None,
        )


def test_search_is_implemented_and_not_armed() -> None:
    # Native search answers the sweep corpus exactly (harness/search_path.py)
    # and runs slower than the Python it would replace
    # (harness/search_bench.py), so it is deliberately left out of NATIVE. This
    # pins that decision, so arming it is a change someone makes on purpose
    # with a benchmark in hand, and not a side effect.
    backend = _backend()
    check("search is not armed", "search" in backend.RustBackend.NATIVE, False)
    check(
        "but it is implemented",
        callable(getattr(backend, "_search_native", None)),
        True,
    )


def test_a_domain_without_a_wire_form_is_delegated() -> None:
    # `Domain.custom(to_sql=...)` and a `Query` value are what optimize_full
    # leaves for a field's search= method; neither has a JSON form, and
    # compiling around them would answer a different question.
    backend = _backend()
    _odoo()
    from odoo.fields import Domain
    from odoo.libs.sql.builder import SQL

    custom = Domain.custom(to_sql=lambda *_a: SQL("TRUE"))
    for label, domain in (
        ("a custom SQL condition", custom),
        (
            "a custom SQL condition under a conjunction",
            Domain("name", "=", "x") & custom,
        ),
    ):
        try:
            backend._domain_json(domain)
        except backend._NoWireForm as exc:
            check(label + " names what it refused", "custom SQL" in str(exc), True)
        else:
            raise AssertionError("%s was serialised" % label)

    class _Opaque:
        pass

    try:
        backend._domain_json(Domain("id", "in", [1]) & Domain("name", "=", _Opaque()))
    except backend._NoWireForm as exc:
        check("an object value names its type", "_Opaque" in str(exc), True)
    else:
        raise AssertionError("an object value was serialised")
    check(
        "a plain domain serialises to the prefix list",
        json.loads(
            backend._domain_json(Domain("name", "=", "x") | Domain("id", "in", [1, 2]))
        ),
        ["|", ["name", "=", "x"], ["id", "in", [1, 2]]],
    )


def test_bypass_access_without_superuser_is_delegated() -> None:
    # `_search(bypass_access=True)` drops the root's rules and keeps every
    # sub-query's; the kernel's modes are "rules everywhere" and "superuser",
    # so neither answers it, and the port must not pick one.
    backend = _backend()

    class _Env:
        su = False

    class _Model:
        env = _Env()

    backend.reset_stats()
    got = backend._search_native(_Model(), None, 0, None, None, False)
    check("delegated", got, None)
    check(
        "with its reason",
        backend.stats()["reasons_by_method"].get("search"),
        {"bypass_access without superuser": 1},
    )


def test_search_delegates_once_the_transaction_wrote_security() -> None:
    # The kernel's rules come from a snapshot that moves on commit; a rule
    # written in this transaction is visible to Python and not to it. The
    # shim's DIRTY_CRS is the record of that write, and the port must read it
    # before it compiles anything -- and delegate when no shim keeps it.
    backend = _backend()
    orm_shim = _shims()[1]

    class _Cursor:
        pass

    class _Env:
        su = True
        cr = _Cursor()

    class _Model:
        env = _Env()

    saved = orm_shim._INSTALLED
    backend.KERNEL_FOR = lambda _env: None
    try:
        orm_shim._INSTALLED = None
        backend.reset_stats()
        check(
            "no shim: delegated",
            backend._search_native(_Model(), None, 0, None, None, True),
            None,
        )
        check(
            "no shim: the reason says the writes are untracked",
            list(backend.stats()["reasons_by_method"]["search"]),
            ["the method shim is not installed, so security writes are not tracked"],
        )

        orm_shim._INSTALLED = {"installed": True}
        orm_shim.DIRTY_CRS.add(_Model.env.cr)
        backend.reset_stats()
        check(
            "tainted cursor: delegated",
            backend._search_native(_Model(), None, 0, None, None, True),
            None,
        )
        check(
            "tainted cursor: the reason names the security write",
            list(backend.stats()["reasons_by_method"]["search"]),
            ["this transaction wrote a security model"],
        )

        orm_shim.DIRTY_CRS.discard(_Model.env.cr)
        backend.reset_stats()
        backend._search_native(_Model(), None, 0, None, None, True)
        check(
            "a clean cursor goes past the check to the kernel lookup",
            list(backend.stats()["reasons_by_method"]["search"]),
            ["no kernel in this process"],
        )
    finally:
        orm_shim._INSTALLED = saved
        orm_shim.DIRTY_CRS.discard(_Model.env.cr)
        backend.KERNEL_FOR = None


if __name__ == "__main__":
    sys.exit(main())
