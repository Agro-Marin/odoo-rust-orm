"""Database contracts for port routing, with only WHERE compilation substituted.

Set RUSTORM_TEST_DSN to a disposable database. PostgreSQL, RustConn, the port,
Domain, dependency flushing and lazy Query construction are real. The tiny
model facade has no pending computed fields; this is not full addon parity.
"""

import contextlib
import logging
import os
from types import SimpleNamespace

import pytest

_logger = logging.getLogger(__name__)


@pytest.mark.parametrize("name", ["missing_field", "missing_field.name"])
@pytest.mark.parametrize(
    ("operator", "value"),
    [
        ("=?", False),
        ("in", []),
        ("not in", []),
        ("in", False),
        ("ilike", ""),
        ("like", "%"),
        ("not like", "%"),
    ],
)
def test_unknown_fields_are_validated_before_leaf_folding(name, operator, value):
    from odoo.orm.domain import Domain

    model = SimpleNamespace(_name="audit.model", _fields={})
    term = [name, operator, value]
    _logger.debug("Python missing-field validation term=%r", term)
    with pytest.raises(ValueError, match=r"Invalid field audit\.model\.missing_field"):
        Domain([term]).optimize(model)


@pytest.mark.parametrize(
    ("term", "expected"),
    [
        ([2, "=", 2], None),
        ([1, "!=", 0], None),
        ([0, "=", 0], None),
        ([-1, "=", "not a number"], None),
        ([1, "<", 2], None),
        ([1, "=", "1"], None),
        ([None, "=", 1], None),
        ([1, "=", 1], True),
        ([0, "=", 1], False),
        ([True, "=", True], True),
        ([False, "=", True], False),
        ([1.0, "=", 1.0], True),
        ([0.0, "=", 1.0], False),
        ([1, "=", True], True),
    ],
)
def test_numeric_domain_leaf_sentinels(term, expected):
    from odoo.orm.domain import Domain

    _logger.debug("Odoo constant leaf term=%r expected=%r", term, expected)
    if expected is None:
        with pytest.raises(TypeError):
            Domain([term])
    else:
        assert Domain([term]) is (Domain.TRUE if expected else Domain.FALSE)


@pytest.mark.parametrize("value", ["USA", "é日語", 123])
def test_char_search_values_are_not_storage_values(value):
    from odoo.orm.fields import Char
    from odoo.orm.fields.textual import _get_string_comparand

    field = Char()
    field.size = 2
    stored = field.convert_to_cache(value, None)
    compared = field._comparand_to_column(value, None)
    inequality = field._get_inequality_comparand(value, None)
    _logger.debug(
        "value=%r stored=%r compared=%r inequality=%r",
        value,
        stored,
        compared,
        inequality,
    )
    assert stored == str(value)[:2]
    assert compared == inequality == _get_string_comparand(value) == str(value)


@pytest.mark.parametrize(
    ("raw", "expected"),
    [("NaN", 3), ("inf", 4), ("-Infinity", 5), ("1e999", 4), ("1_000", 6), ("١٢", 7)],
)
def test_delegated_numeric_strings_keep_python_and_postgres_meaning(raw, expected):
    """Reference oracle for pure.rs's native numeric-string refusal regression."""
    dsn = os.environ.get("RUSTORM_TEST_DSN")
    if not dsn:
        pytest.skip("RUSTORM_TEST_DSN must name a disposable database")
    import psycopg

    from odoo.orm.domain.optimizations import _coerce_numeric

    value = _coerce_numeric(raw, "float")
    with psycopg.connect(dsn) as conn, conn.cursor() as cr:
        cr.execute(
            "CREATE TEMP TABLE audit_numeric (id integer, value double precision)"
        )
        cr.execute(
            "INSERT INTO audit_numeric VALUES (1, 0), (2, NULL), (3, 'NaN'), "
            "(4, 'Infinity'), (5, '-Infinity'), (6, 1000), (7, 12)"
        )
        cr.execute("SELECT id FROM audit_numeric WHERE value = %s", (value,))
        rows = cr.fetchall()
    _logger.debug("raw=%r parsed=%r PostgreSQL rows=%r", raw, value, rows)
    assert rows == [(expected,)]


@pytest.fixture
def port(monkeypatch):
    dsn = os.environ.get("RUSTORM_TEST_DSN")
    if not dsn:
        pytest.skip("RUSTORM_TEST_DSN must name a disposable database")
    engine = pytest.importorskip("engine_py")
    backend = engine.install_backend()
    import rust_orm_shim

    from odoo.libs.sql import SQL
    from odoo.tools.query import Query

    conn = engine.RustDb(dsn).connect()

    class Cursor:
        rows = []

        def execute(self, sql, params=None):
            self.rows = conn.execute(sql, params).rows

        def fetchone(self):
            return self.rows[0]

        @contextlib.contextmanager
        def savepoint(self, *, flush=False):
            assert not flush
            self.execute("SAVEPOINT audit_search")
            try:
                yield
            except Exception:
                self.execute("ROLLBACK TO SAVEPOINT audit_search")
                raise
            finally:
                self.execute("RELEASE SAVEPOINT audit_search")

    class Env:
        cr = Cursor()
        registry = SimpleNamespace()

        def __getitem__(self, name):
            assert name == model._name
            return model

    # Trigger capability caching needs a weak-referenceable registry.
    class Registry:
        pass

    env = Env()
    env.registry = Registry()

    def field(kind, comodel=None):
        return SimpleNamespace(
            type=kind,
            comodel_name=comodel,
            store=True,
            is_one2many=False,
            related=None,
            relational=kind == "many2one",
        )

    model = SimpleNamespace(
        env=env,
        _name="audit.ref",
        _table="audit_search_ref",
        _auto=True,
        _table_sql=None,
        _table_inheritance_root=False,
        _fields={
            "res_id": field("integer"),
            "res_model": field("char"),
            "target_id": field("many2one", "audit.target"),
        },
        flush_model=lambda _names: None,
    )
    target = SimpleNamespace(env=env, _name="audit.target")

    class Delegate:
        def create_rows(self, _model, _stored, _columns, _fields):
            return [
                conn.execute(
                    "INSERT INTO audit_search_target DEFAULT VALUES RETURNING id"
                ).rows[0][0]
            ]

        def search_raw(self, *_args, **_kwargs):
            return None

    def compile_where(records, domain, *_args, **_kwargs):
        query = Query(records.env, records._table)
        for leaf in domain.iter_conditions():
            assert leaf.operator == "="
            query.add_where(SQL("%s = %s", SQL.identifier(leaf.field_expr), leaf.value))
        return query

    monkeypatch.setattr(
        backend.RustBackend,
        "_armed",
        lambda _self, method, _model: method == "search_raw",
    )
    monkeypatch.setattr(backend, "_search_native", compile_where)
    monkeypatch.setattr(rust_orm_shim, "_rust_conn", lambda _env: conn)
    # Fixtures use temporary relations: no persistent application tables touched.
    conn.execute("CREATE TEMP TABLE audit_search_target (id serial PRIMARY KEY)")
    conn.execute(
        "CREATE TEMP TABLE audit_search_ref (id serial PRIMARY KEY, res_model text, res_id int, target_id int REFERENCES audit_search_target)"
    )
    conn.execute(
        "CREATE FUNCTION pg_temp.audit_add_ref() RETURNS void LANGUAGE SQL AS 'INSERT INTO audit_search_ref(target_id) VALUES (1)'"
    )
    conn.commit()
    try:
        yield SimpleNamespace(
            backend=backend.RustBackend(Delegate()),
            model=model,
            target=target,
            conn=conn,
            env=env,
        )
    finally:
        conn.rollback()
        conn.close()


@pytest.mark.parametrize(
    "scenario", ["dangling_reference", "lazy_query", "sql_function"]
)
def test_search_reads_the_rows_visible_when_it_executes(port, scenario):
    from odoo.orm.domain import Domain

    conn = port.conn
    if scenario == "dangling_reference":
        conn.execute(
            "INSERT INTO audit_search_ref (res_model, res_id) VALUES ('audit.target', 1)"
        )
        conn.commit()
    created = port.backend.create_rows(port.target, [{}], [], [])
    assert created == [1]
    domain = (
        Domain([("res_model", "=", "audit.target"), ("res_id", "=", 1)])
        if scenario == "dangling_reference"
        else Domain([("target_id", "=", 1)])
    )
    if scenario == "sql_function":
        conn.execute("SELECT pg_temp.audit_add_ref()")
    query = port.backend.search_raw(port.model, domain, 0, None, None)
    assert query is not None
    if scenario == "lazy_query":
        conn.execute("INSERT INTO audit_search_ref(target_id) VALUES (1)")
    sql = query.select()
    actual = conn.execute(sql.code, sql.params).rows
    expected = conn.execute("SELECT id FROM audit_search_ref").rows
    _logger.debug(
        "scenario=%s query=%s actual=%r expected=%r",
        scenario,
        sql.code,
        actual,
        expected,
    )
    assert expected == [(1,)]
    assert actual == expected


@pytest.mark.parametrize("method", ["search", "search_raw"])
def test_refusal_counts_the_method_that_was_called(monkeypatch, method):
    engine = pytest.importorskip("engine_py")
    backend = engine.install_backend()
    from odoo.orm.domain import Domain

    monkeypatch.setattr(
        backend, "STATS", {key: type(value)() for key, value in backend.STATS.items()}
    )
    monkeypatch.setattr(backend.RustBackend, "_armed", lambda *_args: True)
    delegate = SimpleNamespace(
        search=lambda *_a, **_kw: None, search_raw=lambda *_a, **_kw: None
    )
    model = SimpleNamespace(env=SimpleNamespace(su=False))
    getattr(backend.RustBackend(delegate), method)(
        model, Domain([]), 0, None, None, check_access=False
    )
    stats = backend.stats()
    _logger.debug("called=%s stats=%r", method, stats)
    assert stats["delegated"] == {method: 1}
    assert stats["reasons_by_method"] == {
        method: {"bypass_access without superuser": 1}
    }
