import logging
import os
import threading
import typing
import urllib.parse
from time import monotonic, sleep
from typing import Never

import psycopg

_logger = logging.getLogger("odoo.rust_kernel.pool")

RESET_SESSION_STATE_SQL = (
    "RESET ALL;"
    " RESET SESSION AUTHORIZATION;"
    " CLOSE ALL;"
    " UNLISTEN *;"
    " SELECT pg_advisory_unlock_all();"
    " DISCARD TEMP;"
    " DISCARD SEQUENCES"
)


class _Column(typing.NamedTuple):
    name: str
    type_code: object


RUST_DB = None
CONNINFO = None
PSYCOPG_CONNINFO = None
_ADAPT = None

ACTIVE = False


def _adapt_cnx():
    global _ADAPT
    if _ADAPT is None:
        spec = PSYCOPG_CONNINFO or CONNINFO
        if isinstance(spec, dict):
            _ADAPT = psycopg.connect(**spec, autocommit=True)
        else:
            _ADAPT = psycopg.connect(spec, autocommit=True)
    return _ADAPT


class _Diag:
    _FIELDS = (
        "severity",
        "severity_nonlocalized",
        "sqlstate",
        "message_primary",
        "message_detail",
        "message_hint",
        "statement_position",
        "internal_position",
        "internal_query",
        "context",
        "schema_name",
        "table_name",
        "column_name",
        "datatype_name",
        "constraint_name",
        "source_file",
        "source_line",
        "source_function",
    )

    def __init__(self, fields):
        for name in self._FIELDS:
            setattr(self, name, fields.get(name))


_DIAG_CLASSES = {}


def _error_class_with_diag(cls):
    sub = _DIAG_CLASSES.get(cls)
    if sub is None:
        sub = type(
            cls.__name__,
            (cls,),
            {
                "diag": property(lambda self: self._rust_diag),
                "__module__": cls.__module__,
            },
        )
        _DIAG_CLASSES[cls] = sub
    return sub


def _raise_pg(exc) -> Never:
    exc.__traceback__ = None
    msg = str(exc)
    if not msg.startswith("SQLSTATE:"):
        _logger.debug("rust cursor error with no SQLSTATE: %.200s", msg)
        raise psycopg.OperationalError(msg) from None
    body = msg[len("SQLSTATE:") :]
    parts = body.split("\x1f")
    code, _, detail = parts[0].partition("|")
    fields = {"sqlstate": code or None, "message_primary": detail}
    for chunk in parts[1:]:
        key, _, value = chunk.partition("\x1e")
        fields[key] = value
    cls = psycopg.errors.lookup(code) if code else psycopg.OperationalError
    _logger.debug("server error %s (%s): %.200s", code, cls.__name__, detail)
    err = _error_class_with_diag(cls)(detail)
    err._rust_diag = _Diag(fields)
    raise err from None


def _decode_query(query):
    try:
        return query.decode()
    except UnicodeDecodeError as exc:
        raise psycopg.errors.CharacterNotInRepertoire(
            'invalid byte sequence for encoding "UTF8": 0x%02x' % query[exc.start]
        ) from None


class FakeCursor:
    def __init__(self, cnx) -> None:
        self._cnx = cnx
        self._result = None
        self._pos = 0
        self._rowcount_override = None
        self.closed = False

    @property
    def connection(self):
        return self._cnx

    def execute(self, query, params=None, *, prepare=None):  # noqa: ARG002  psycopg signature; the rust side always prepares
        if isinstance(query, bytes):
            query = _decode_query(query)
        self._rowcount_override = None
        try:
            self._result = self._cnx._rust.execute(query, params)
        except RuntimeError as e:
            _raise_pg(e)
        self._pos = 0
        return self

    def scroll(self, value, mode="relative"):
        rows = self._result.rows if self._result else []
        if mode == "relative":
            newpos = self._pos + value
        elif mode == "absolute":
            newpos = value
        else:
            raise ValueError(
                "bad mode: %s. It should be 'relative' or 'absolute'" % mode
            )
        if not 0 <= newpos < len(rows):
            raise IndexError("position out of bound")
        self._pos = newpos

    def executemany(self, query, params_seq, *, returning=False):
        rows = []
        total = 0
        for params in params_seq:
            self.execute(query, params)
            total += self._result.rowcount
            if returning:
                rows.extend(self._result.rows)
        if returning:
            self._result.rows[:] = rows
        self._rowcount_override = total
        return self

    @property
    def rowcount(self):
        if self._rowcount_override is not None:
            return self._rowcount_override
        return self._result.rowcount if self._result else -1

    @property
    def description(self):
        if not self._result or not self._result.columns:
            return None
        return [_Column(name, None) for name in self._result.columns]

    def _rows(self):
        if self._result is None:
            raise psycopg.ProgrammingError("no result available: execute a query first")
        return self._result.rows

    def fetchone(self):
        rows = self._rows()
        if self._pos >= len(rows):
            return None
        row = rows[self._pos]
        self._pos += 1
        return row

    def fetchmany(self, size=1):
        rows = self._rows()[self._pos : self._pos + size]
        self._pos += len(rows)
        return rows

    def fetchall(self):
        all_rows = self._rows()
        rows = all_rows[self._pos :]
        self._pos = len(all_rows)
        return rows

    def nextset(self) -> None:
        return None

    def close(self) -> None:
        self.closed = True

    def copy(self, statement, params=None, *, writer=None):
        if writer is not None:
            raise NotImplementedError("COPY with a custom writer")
        sql_text = statement
        if not isinstance(sql_text, str):
            sql_text = sql_text.as_string(self.connection)
        if params:
            raise NotImplementedError("COPY with parameters")
        INSTALLED["copies"] += 1
        try:
            return _Copy(self._cnx._rust.copy(sql_text), self.connection)
        except RuntimeError as e:
            _raise_pg(e)


class _Copy:
    def __init__(self, rust_copy, connection=None):
        self._copy = rust_copy
        self._connection = connection

    def set_types(self, types):
        fmt = psycopg.pq.Format.BINARY if self._copy.binary else psycopg.pq.Format.TEXT
        psycopg.adapt.Transformer(self._connection).set_dumper_types(types, fmt)
        try:
            return self._copy.set_types(types)
        except RuntimeError as e:
            _raise_pg(e)

    def write_row(self, row):
        try:
            return self._copy.write_row(row)
        except RuntimeError as e:
            _raise_pg(e)

    def write(self, data):
        try:
            return self._copy.write(data)
        except RuntimeError as e:
            _raise_pg(e)

    @property
    def rowcount(self):
        return self._copy.rowcount

    def __enter__(self):
        self._copy.__enter__()
        return self

    def __exit__(self, exc_type, exc, tb):
        try:
            return self._copy.__exit__(exc_type, exc, tb)
        except RuntimeError as e:
            _raise_pg(e)


class _Prepared:
    def __init__(self, rust_conn) -> None:
        self._rust = rust_conn

    @property
    def _names(self):
        return self._rust.prepared_names

    def clear(self) -> None:
        self._rust.clear_prepared()


class FakeConnection:
    prepare_threshold = 5

    def __init__(self, rust_conn) -> None:
        self._rust = rust_conn
        self._prepared = _Prepared(rust_conn)
        self._isolation = None
        self._info = None
        self._dsn = ""

    @property
    def autocommit(self):
        return self._rust.autocommit

    @autocommit.setter
    def autocommit(self, value):
        self._rust.set_autocommit(bool(value))

    @property
    def connection(self):
        return self

    def reset_session(self, discard=None) -> None:
        if discard is None:
            discard = _discard_on_return()
        sql = "DISCARD ALL" if discard else RESET_SESSION_STATE_SQL
        try:
            self._rust.reset_session(sql, bool(discard))
        except RuntimeError as e:
            _raise_pg(e)
        self._isolation = None

    @property
    def info(self):
        if self._info is None:
            self._info = _ConnInfo(self)
        return self._info

    @property
    def pgconn(self):
        return _adapt_cnx().pgconn

    @property
    def adapters(self):
        return _adapt_cnx().adapters

    def cursor(self):
        return FakeCursor(self)

    @property
    def isolation_level(self):
        return self._isolation

    @isolation_level.setter
    def isolation_level(self, value) -> None:
        self._isolation = value

    @property
    def read_only(self) -> bool:
        return False

    @read_only.setter
    def read_only(self, value) -> None:
        self._rust.set_readonly(bool(value))

    @property
    def closed(self):
        return self._rust.closed

    def commit(self) -> None:
        try:
            self._rust.commit()
        except RuntimeError as e:
            _raise_pg(e)

    def rollback(self) -> None:
        try:
            self._rust.rollback()
        except RuntimeError as e:
            _raise_pg(e)

    def close(self) -> None:
        self._rust.close()

    def execute(self, query, params=None, *, prepare=None):
        cur = self.cursor()
        cur.execute(query, params, prepare=prepare)
        return cur

    def pipeline(self):
        return _Pipeline()


class _Pipeline:
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def sync(self) -> None:
        return None


INSTALLED = {
    "count": 0,
    "connects": 0,
    "reused": 0,
    "failed_checks": 0,
    "delegated": 0,
    "copies": 0,
    "inactive": 0,
    "switches": 0,
}

_IDLE = []
_IDLE_LOCK = threading.Lock()
_IDLE_PID = os.getpid()
MAX_IDLE = 8


def _close_quietly(conn, why) -> None:
    try:
        conn.close()
    except Exception as exc:
        _logger.debug("closing a connection (%s) raised %s; dropping it", why, exc)


def _discard_on_return():
    try:
        from odoo.db.settings import current

        return bool(current().discard_on_return)
    except Exception:
        return False


def _uri_dbname(uri):
    parts = urllib.parse.urlsplit(uri)
    query = dict(urllib.parse.parse_qsl(parts.query))
    if query.get("dbname"):
        return query["dbname"]
    if len(parts.path) > 1:
        return urllib.parse.unquote(parts.path[1:])
    if parts.username:
        return urllib.parse.unquote(parts.username)
    return parts.hostname or None


def _dbname(dsn):
    if isinstance(dsn, dict):
        name = dsn.get("dbname") or dsn.get("database")
        if name:
            return name
        return _dbname(dsn.get("dsn")) if dsn.get("dsn") else None
    if not isinstance(dsn, str):
        return None
    if dsn.startswith(("postgresql://", "postgres://")):
        return _uri_dbname(dsn)
    for key, value in _parse_conninfo(dsn):
        if key == "dbname":
            return value
    return None


_KEY_IGNORED = frozenset({"application_name", "options"})


def _normalize_key(dsn):
    import hashlib

    if isinstance(dsn, str):
        items = dict(
            _uri_items(dsn)
            if dsn.startswith(("postgresql://", "postgres://"))
            else _parse_conninfo(dsn)
        )
    else:
        items = dict(dsn)
        raw = items.pop("dsn", None)
        if raw:
            base = dict(
                _uri_items(raw)
                if raw.startswith(("postgresql://", "postgres://"))
                else _parse_conninfo(raw)
            )
            items = {**base, **items}
    password = items.pop("password", None)
    fp = (
        hashlib.blake2s(str(password).encode(), digest_size=8).hexdigest()
        if password
        else ""
    )
    aliases = {"dbname": "database"}
    return frozenset(
        [(aliases.get(k, k), str(v)) for k, v in items.items() if v is not None]
        + [("password_fp", fp)]
    )


def _uri_items(uri):
    parts = urllib.parse.urlsplit(uri)
    items = dict(urllib.parse.parse_qsl(parts.query))
    if parts.hostname:
        items["host"] = parts.hostname
    if parts.port:
        items["port"] = str(parts.port)
    if parts.username:
        items["user"] = urllib.parse.unquote(parts.username)
    if parts.password:
        items["password"] = urllib.parse.unquote(parts.password)
    if len(parts.path) > 1:
        items["dbname"] = urllib.parse.unquote(parts.path[1:])
    return items.items()


def _identity(dsn, key=None):
    if key is None:
        try:
            key = _normalize_key(dsn)
        except Exception:
            return None
    return frozenset(t for t in key if t[0] not in _KEY_IGNORED)


def _intercepts(pool, dsn, key=None):
    if getattr(pool, "readonly", False):
        _logger.debug("not intercepting a readonly pool")
        return False
    if not CONNINFO or not dsn:
        return True
    if _dbname(dsn) != _dbname(CONNINFO):
        _logger.debug(
            "not intercepting %r: armed for %r", _dbname(dsn), _dbname(CONNINFO)
        )
        return False
    if PSYCOPG_CONNINFO is None:
        return True
    ours = _identity(PSYCOPG_CONNINFO)
    theirs = _identity(dsn, key)
    if ours is None or theirs is None:
        return True
    if ours != theirs:
        _logger.debug(
            "not intercepting %r: the connection identity differs from the armed one",
            _dbname(dsn),
        )
        return False
    return True


def _parse_conninfo(text):
    i, n = 0, len(text)
    while i < n:
        while i < n and text[i].isspace():
            i += 1
        start = i
        while i < n and not text[i].isspace() and text[i] != "=":
            i += 1
        key = text[start:i]
        while i < n and text[i].isspace():
            i += 1
        if i >= n or text[i] != "=" or not key:
            continue
        i += 1
        while i < n and text[i].isspace():
            i += 1
        chars = []
        if i < n and text[i] == "'":
            i += 1
            while i < n and text[i] != "'":
                if text[i] == "\\" and i + 1 < n:
                    i += 1
                chars.append(text[i])
                i += 1
            i += 1
        else:
            while i < n and not text[i].isspace():
                if text[i] == "\\" and i + 1 < n:
                    i += 1
                chars.append(text[i])
                i += 1
        yield key, "".join(chars)


def pool_stats():
    return {"active": ACTIVE, **INSTALLED}


class _ConnInfo:
    def __init__(self, conn):
        self._conn = conn
        self._pid = None
        self._server_version = None

    def _scalar(self, sql):
        in_transaction = self._conn._rust.in_transaction
        try:
            cur = self._conn.cursor()
            cur.execute(sql)
            return cur.fetchone()[0]
        finally:
            if not in_transaction:
                self._conn.rollback()

    @property
    def backend_pid(self):
        if self._pid is None:
            self._pid = int(self._scalar("SELECT pg_backend_pid()"))
        return self._pid

    @property
    def server_version(self):
        if self._server_version is None:
            self._server_version = int(self._scalar("SHOW server_version_num"))
        return self._server_version

    @property
    def dsn(self):
        return getattr(self._conn, "_dsn", "") or ""

    @property
    def transaction_status(self):
        from psycopg.pq import TransactionStatus

        if self._conn.closed:
            return TransactionStatus.UNKNOWN
        rust = self._conn._rust
        if not rust.in_transaction:
            return TransactionStatus.IDLE
        if rust.in_failed_transaction:
            return TransactionStatus.INERROR
        return TransactionStatus.INTRANS


_DSN_KEYS = frozenset(
    (
        "host",
        "hostaddr",
        "port",
        "dbname",
        "user",
        "password",
        "options",
        "application_name",
        "connect_timeout",
        "sslmode",
        "target_session_attrs",
    )
)


def _dsn_with_kwargs(conninfo, kwargs):
    resolved = {}
    for text in (CONNINFO if isinstance(CONNINFO, str) else "", conninfo):
        if text:
            resolved.update(_parse_conninfo(text))
    for key, value in (kwargs or {}).items():
        if key not in _DSN_KEYS or value is None or value == "":
            continue
        resolved[key] = str(value)
    return " ".join(
        "%s='%s'" % (key, value.replace("\\", "\\\\").replace("'", "\\'"))
        for key, value in resolved.items()
    )


class _RustPool:
    def __init__(
        self,
        conninfo="",
        *,
        kwargs=None,
        min_size=0,  # noqa: ARG002  pool opens connections lazily
        max_size=None,
        max_idle=None,  # noqa: ARG002  outer Odoo pool owns idle expiry
        configure=None,
        reset=None,
        check=None,
        open=True,  # noqa: A002  psycopg pool signature
        **_unused,
    ):
        self._kwargs = dict(kwargs or {})
        self._dsn = _dsn_with_kwargs(conninfo, self._kwargs)
        self._max_size = max(1, int(max_size or 1))
        self._configure = configure
        self._reset = reset
        self._check = check
        self._idle = []
        self._out = 0
        self._generation = 0
        self._closed = not open
        self._cond = threading.Condition()
        self._pid = os.getpid()

    def _forget_inherited_after_fork(self):
        pid = os.getpid()
        if pid != self._pid:
            _logger.debug(
                "forked from pid %d: dropping %d inherited idle connection(s) "
                "without closing them",
                self._pid,
                len(self._idle),
            )
            self._idle.clear()
            self._out = 0
            self._pid = pid

    @property
    def closed(self):
        return self._closed

    @property
    def _pool(self):
        return [conn for conn, _gen in self._idle]

    @staticmethod
    def check_connection(conn):
        conn.execute("")

    def get_stats(self):
        with self._cond:
            return {
                "pool_size": len(self._idle) + self._out,
                "pool_available": len(self._idle),
                "pool_min": 0,
                "pool_max": self._max_size,
                "requests_waiting": 0,
            }

    def _new_connection(self):
        INSTALLED["connects"] += 1
        started = monotonic()
        conn = FakeConnection(RUST_DB.connect(self._dsn))
        conn._dsn = self._dsn
        if self._configure is not None:
            self._configure(conn)
        _logger.debug(
            "opened rust connection %d in %.1f ms (%d out, %d idle, max %d)",
            INSTALLED["connects"],
            (monotonic() - started) * 1000,
            self._out,
            len(self._idle),
            self._max_size,
        )
        return conn

    def _connect_until(self, deadline, timeout):
        from psycopg_pool import PoolTimeout

        delay = 0.05
        while True:
            try:
                return self._new_connection()
            except Exception as exc:
                remaining = deadline - monotonic()
                if remaining <= 0:
                    raise PoolTimeout(
                        "couldn't get a connection after %.2f sec: %s"
                        % (30.0 if timeout is None else timeout, exc)
                    ) from exc
                _logger.debug("opening a rust connection failed, retrying: %s", exc)
                sleep(min(delay, remaining))
                delay = min(delay * 2, 1.0)

    def getconn(self, timeout=None):
        from psycopg_pool import PoolClosed, PoolTimeout

        deadline = monotonic() + (30.0 if timeout is None else timeout)
        while True:
            conn = None
            with self._cond:
                self._forget_inherited_after_fork()
                if self._closed:
                    raise PoolClosed("the pool %r is already closed" % self)
                generation = self._generation
                while self._idle:
                    candidate, gen = self._idle.pop()
                    if gen == generation and not candidate.closed:
                        conn = candidate
                        break
                    _close_quietly(candidate, "stale generation or already closed")
                if conn is None and self._out >= self._max_size:
                    remaining = deadline - monotonic()
                    if remaining <= 0:
                        raise PoolTimeout(
                            "couldn't get a connection after %.2f sec"
                            % (30.0 if timeout is None else timeout)
                        )
                    self._cond.wait(remaining)
                    continue
                self._out += 1
            try:
                if conn is None:
                    conn = self._connect_until(deadline, timeout)
                    INSTALLED["count"] += 1
                else:
                    INSTALLED["reused"] += 1
                    if self._check is not None and not self._passes_check(conn):
                        with self._cond:
                            self._out -= 1
                            self._cond.notify()
                        continue
                conn._pool = self
                conn._rust_pool_generation = generation
            except BaseException:
                with self._cond:
                    self._out -= 1
                    self._cond.notify()
                if conn is not None:
                    _close_quietly(conn, "checkout failed")
                raise
            return conn

    def _passes_check(self, conn):
        try:
            self._check(conn)
        except Exception as exc:
            INSTALLED["failed_checks"] += 1
            _logger.debug("an idle rust connection failed its check: %s", exc)
            _close_quietly(conn, "failed its check")
            return False
        return True

    def putconn(self, conn):
        keep = True
        try:
            if self._reset is not None and not conn.closed:
                self._reset(conn)
        except Exception:
            keep = False
        with self._cond:
            self._forget_inherited_after_fork()
            self._out = max(0, self._out - 1)
            room = len(self._idle) < self._max_size
            current = conn._rust_pool_generation == self._generation
            if keep and room and current and not self._closed and not conn.closed:
                self._idle.append((conn, self._generation))
                conn = None
            self._cond.notify()
        if conn is not None:
            _close_quietly(conn, "not returned to the pool")

    def _evict_idle(self):
        with self._cond:
            idle, self._idle = self._idle, []
            self._cond.notify_all()
        for conn, _gen in idle:
            _close_quietly(conn, "pool evicted or closed")

    def drain(self):
        _logger.debug(
            "draining the pool: %d idle, %d out, generation %d -> %d",
            len(self._idle),
            self._out,
            self._generation,
            self._generation + 1,
        )
        with self._cond:
            self._generation += 1
        self._evict_idle()

    def close(self):
        _logger.debug("closing the pool: %d idle, %d out", len(self._idle), self._out)
        with self._cond:
            self._closed = True
        self._evict_idle()

    def open(self, wait=False, timeout=None):  # noqa: ARG002  synchronous pool has no background opening
        with self._cond:
            self._closed = False

    def wait(self, timeout=None):  # noqa: ARG002  connections open synchronously on checkout
        return None

    def resize(self, min_size=None, max_size=None):  # noqa: ARG002  lazy pool retains no minimum connections
        if max_size:
            with self._cond:
                self._max_size = max(1, int(max_size))


def _pool_intercepts(conninfo, kwargs):
    driver_keys = {
        "autocommit",
        "prepare_threshold",
        "row_factory",
        "cursor_factory",
        "context",
    }
    actual = {
        "dsn": conninfo,
        **{k: v for k, v in (kwargs or {}).items() if k not in driver_keys},
    }
    return _intercepts(None, actual)


def _close_armed_pools():
    dbname = _dbname(CONNINFO)
    _logger.info(
        "rust connection layer switched for %s; pools built before this point are being closed",
        dbname or "<unknown database>",
    )
    if dbname:
        from odoo.db import registry as db_registry

        try:
            db_registry.close_db(dbname)
        except Exception as exc:
            _logger.warning(
                "could not close the existing pools for %s (%s); borrows made "
                "through them stay on psycopg",
                dbname,
                exc,
            )


def set_active(flag):
    global ACTIVE
    flag = bool(flag)
    if flag == ACTIVE:
        return
    ACTIVE = flag
    INSTALLED["switches"] += 1
    _close_armed_pools()


def install():
    from odoo.db import lifecycle as lifecycle_module
    from odoo.db import pool as pool_module

    psycopg_pool_class = pool_module._PsycopgPool

    def pool_factory(conninfo="", **kwargs):
        if not ACTIVE:
            INSTALLED["inactive"] += 1
            return psycopg_pool_class(conninfo, **kwargs)
        if not _pool_intercepts(conninfo, kwargs.get("kwargs")):
            INSTALLED["delegated"] += 1
            _logger.debug(
                "delegating a pool to psycopg (%d so far)", INSTALLED["delegated"]
            )
            return psycopg_pool_class(conninfo, **kwargs)
        _logger.debug("building a rust-backed pool for the armed database")
        return _RustPool(conninfo, **kwargs)

    pool_factory.check_connection = psycopg_pool_class.check_connection
    pool_module._PsycopgPool = pool_factory
    lifecycle_module._PsycopgPool = pool_factory

    original_simple_query = getattr(lifecycle_module, "_run_simple_query", None)

    def run_simple_query(conn, sql, expected):
        if not isinstance(conn, FakeConnection):
            return original_simple_query(conn, sql, expected)
        rust = conn._rust
        if isinstance(sql, bytes):
            sql = _decode_query(sql)
        status, message = rust.exec_simple(sql)
        if status != expected:
            raise psycopg.OperationalError(message)
        return None

    if original_simple_query is not None and not getattr(
        original_simple_query, "_rust_shim", False
    ):
        run_simple_query._rust_shim = True
        lifecycle_module._run_simple_query = run_simple_query

    _close_armed_pools()
