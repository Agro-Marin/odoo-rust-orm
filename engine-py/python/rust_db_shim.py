import contextlib
import os
import threading
import typing
import urllib.parse
from time import monotonic
from typing import Never

import psycopg

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
    """psycopg's `Diagnostic` surface, rebuilt from what rust carried over.

    psycopg builds this from a live PGresult, so it cannot be constructed
    directly and `Error.diag` is a read-only property -- hence the subclass
    below. Every field psycopg exposes is present and defaults to None, so a
    reader asking for one this transport does not carry gets None rather than
    an AttributeError, which is what psycopg gives for a field the server
    omitted.
    """

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
    msg = str(exc)
    if not msg.startswith("SQLSTATE:"):
        raise psycopg.OperationalError(msg) from None
    body = msg[len("SQLSTATE:") :]
    parts = body.split("\x1f")
    code, _, detail = parts[0].partition("|")
    fields = {"sqlstate": code or None, "message_primary": detail}
    for chunk in parts[1:]:
        key, _, value = chunk.partition("\x1e")
        fields[key] = value
    cls = psycopg.errors.lookup(code) if code else psycopg.OperationalError
    err = _error_class_with_diag(cls)(detail)
    err._rust_diag = _Diag(fields)
    raise err from None


def _decode_query(query):
    # tokio-postgres takes the statement as a `&str`, so bytes that are not
    # UTF-8 cannot reach the server through this cursor at all. Decoding them
    # with `.decode()` raised UnicodeDecodeError, which is not a
    # `psycopg.Error` -- so a caller catching database errors saw nothing and
    # a caller suppressing UnicodeDecodeError saw success.
    #
    # PostgreSQL rejects these bytes itself; measured against 18.x through
    # psycopg, `SELECT '\xff\xfe'::int` gives CharacterNotInRepertoire,
    # SQLSTATE 22021, `invalid byte sequence for encoding "UTF8": 0xff`.
    # Raising that here is the server's own answer, produced one hop early
    # because the transport cannot carry the question.
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
        # psycopg's own semantics (_cursor_base._scroll): relative or
        # absolute, IndexError outside the result set, and the position is
        # left untouched when it would leave it.
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
            return _Copy(self._cnx._rust.copy(sql_text))
        except RuntimeError as e:
            _raise_pg(e)


class _Copy:
    """psycopg's `Copy` surface over `RustCopy`, translating its errors.

    Rust reports a server error as `RuntimeError("SQLSTATE:23505|...")`, and
    `FakeCursor.copy` used to hand the raw object straight to the caller. So
    a constraint violation during a bulk insert arrived as a bare
    RuntimeError: not catchable as `psycopg.errors.UniqueViolation`, and
    carrying no `sqlstate` -- which is exactly what
    `odoo.db.errors.has_reached_server` reads to decide whether a failed
    statement cost a round trip. A COPY that failed ON THE SERVER was
    therefore booked as one that never reached it.
    """

    def __init__(self, rust_copy):
        self._copy = rust_copy

    def set_types(self, types):
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
        # psycopg's `_names` is a MAPPING of cache key -> prepared name, and
        # callers ask it for a len(); a count cannot answer that.
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
        # `prepare` is part of psycopg's CONNECTION.execute signature, not
        # just the cursor's, and Odoo uses it on the connection:
        # `odoo/db/lifecycle.py` resets a returned connection with
        # `conn.execute("DISCARD ALL", prepare=False)`. Without the keyword
        # that call is a TypeError, so a shim that only matched the cursor's
        # signature broke connection reset -- the path that keeps session
        # state from leaking between pooled borrows.
        cur = self.cursor()
        cur.execute(query, params, prepare=prepare)
        return cur

    def pipeline(self):
        import contextlib

        return contextlib.nullcontext()


INSTALLED = {"count": 0, "connects": 0, "reused": 0, "delegated": 0, "copies": 0}

_IDLE = []
_IDLE_LOCK = threading.Lock()
_IDLE_PID = os.getpid()
MAX_IDLE = 8


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
    # the same shape as odoo.db.dsn._normalize_dsn_key, so a key the pool
    # hands over compares with one derived here
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
        return False
    if not CONNINFO or not dsn:
        return True
    if _dbname(dsn) != _dbname(CONNINFO):
        return False
    if PSYCOPG_CONNINFO is None:
        return True
    ours = _identity(PSYCOPG_CONNINFO)
    theirs = _identity(dsn, key)
    if ours is None or theirs is None:
        return True
    return ours == theirs


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
    return dict(INSTALLED)


class _ConnInfo:
    # psycopg exposes these off `conn.info`; Odoo reads `server_version` in the
    # borrow-time version gate and `backend_pid` under debug logging.
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
        # `Cursor._is_connection_clean` reads this and is the only consumer in
        # the fork; it asks one question, whether the status is IDLE. Without
        # it that check raised, took its `except: return False` branch, and
        # every cursor whose rollback hook raised cost a warm pooled
        # connection -- a hook bug charged twice.
        #
        # INTRANS rather than INERROR when a transaction is open: the rust
        # connection tracks that a transaction exists, not whether a statement
        # inside it failed. Both are non-IDLE, so the distinction is invisible
        # to every reader of this attribute in the fork today.
        from psycopg.pq import TransactionStatus

        if self._conn.closed:
            return TransactionStatus.UNKNOWN
        return (
            TransactionStatus.INTRANS
            if self._conn._rust.in_transaction
            else TransactionStatus.IDLE
        )


# tokio-postgres understands a SUBSET of libpq's keywords, so this is an
# allowlist and not a denylist: psycopg's own pool passes `keepalives_count`
# and friends, which libpq accepts and tokio-postgres refuses outright with
# `unknown option`. A denylist here fails closed on the next keyword psycopg
# adds, and it fails by refusing every connection.
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
    # `_get_or_create_pool` assembles the libpq options here -- the pool's
    # configured `db_session_gucs` among them -- and hands them to the pool
    # as kwargs. A pool that drops them applies none of the deployment's
    # session policy and says nothing about it.
    # CONNINFO first, as the BASE: Odoo's connection_info carries dbname and
    # user but often no host at all, because libpq falls back to PGHOST and
    # the default socket directory. tokio-postgres does not -- it refuses with
    # `both host and hostaddr are missing` -- so the armed dsn supplies the
    # host and anything later in the string wins, as libpq resolves it.
    parts = [p for p in (CONNINFO if isinstance(CONNINFO, str) else "", conninfo) if p]
    for key, value in (kwargs or {}).items():
        if key not in _DSN_KEYS or value is None or value == "":
            continue
        escaped = str(value).replace("\\", "\\\\").replace("'", "\\'")
        parts.append("%s='%s'" % (key, escaped))
    return " ".join(parts)


class _RustPool:
    """A `psycopg_pool.ConnectionPool` whose connections are rust-backed.

    Rebound over `odoo.db.pool._PsycopgPool`, so `ConnectionPool.borrow`,
    `give_back`, the connection budget, the checkout tracker, the idle
    reaper, `close_database` / `drain_database` and the stats are all Odoo's
    own code running unmodified. The pool is the seam a connection plugs
    into; replacing `borrow` instead meant reimplementing everything the pool
    does AROUND a connection, and silently skipping most of it.
    """

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
        # Caller holds the lock. A forked child inherits the parent's idle
        # connections, and two processes writing one socket is a hang, not an
        # error -- the load probe's child died on its alarm rather than
        # reporting anything. They are DROPPED, never closed: closing sends a
        # terminate message down a socket the parent still owns.
        pid = os.getpid()
        if pid != self._pid:
            self._idle.clear()
            self._out = 0
            self._pid = pid

    @property
    def closed(self):
        return self._closed

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
        conn = FakeConnection(RUST_DB.connect(self._dsn))
        conn._dsn = self._dsn
        if self._configure is not None:
            self._configure(conn)
        return conn

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
                    with contextlib.suppress(Exception):
                        candidate.close()
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
            # The callbacks talk to the server, so they run outside the lock.
            try:
                if conn is None:
                    conn = self._new_connection()
                    INSTALLED["count"] += 1
                else:
                    INSTALLED["reused"] += 1
                    if self._check is not None:
                        self._check(conn)
                # psycopg's pool stamps the connection with the pool that
                # lent it, and Odoo reads it back off `cr._cnx._pool`.
                conn._pool = self
                conn._rust_pool_generation = generation
            except BaseException:
                with self._cond:
                    self._out -= 1
                    self._cond.notify()
                with contextlib.suppress(Exception):
                    if conn is not None:
                        conn.close()
                raise
            return conn

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
            with contextlib.suppress(Exception):
                conn.close()

    def _evict_idle(self):
        with self._cond:
            idle, self._idle = self._idle, []
            self._cond.notify_all()
        for conn, _gen in idle:
            with contextlib.suppress(Exception):
                conn.close()

    def drain(self):
        # A committed DDL invalidates every prepared plan a sibling
        # connection holds, so `Cursor.commit` drains the pool and the
        # registry drains on reload. Bumping the generation is what makes a
        # connection that is currently OUT get closed on return instead of
        # pooled with its stale plans.
        with self._cond:
            self._generation += 1
        self._evict_idle()

    def close(self):
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
    # Retain libpq identity options even when tokio's connector does not
    # accept them; only psycopg's Python-level arguments are excluded.
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


def install():
    from odoo.db import lifecycle as lifecycle_module
    from odoo.db import pool as pool_module

    psycopg_pool_class = pool_module._PsycopgPool

    def pool_factory(conninfo="", **kwargs):
        # The engine is armed for ONE database. A process holding pools for
        # several -- the database manager, a cron sweeping the cluster --
        # keeps psycopg for the others.
        if not _pool_intercepts(conninfo, kwargs.get("kwargs")):
            INSTALLED["delegated"] += 1
            return psycopg_pool_class(conninfo, **kwargs)
        return _RustPool(conninfo, **kwargs)

    # `odoo.db.pool._PsycopgPool` and `odoo.db.lifecycle._PsycopgPool` are two
    # names for ONE class object, and `_check_connection` reads the second
    # while `test_db_cursor` patches the first -- the health-probe tests are
    # only meaningful because patching either reaches the other. Rebinding one
    # name silently cuts that link, so both are rebound to one object, and it
    # carries psycopg's own `check_connection` for the patch to replace.
    pool_factory.check_connection = psycopg_pool_class.check_connection
    pool_module._PsycopgPool = pool_factory
    lifecycle_module._PsycopgPool = pool_factory

    # A pool is built ONCE per dsn and cached, so rebinding the class alone
    # only reaches pools created after this point -- and by the time the
    # engine arms, the process has already built one. Every later borrow then
    # draws a psycopg connection through a psycopg pool while the shim reports
    # itself installed. Closing the armed database's existing pools is what
    # makes the rebind take effect, and without it a gate comparing the two
    # cursors compares psycopg with psycopg and reports perfect agreement.
    dbname = _dbname(CONNINFO)
    if dbname:
        from odoo.db import registry as db_registry

        with contextlib.suppress(Exception):
            db_registry.close_db(dbname)
