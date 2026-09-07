import collections
import os
import threading

import psycopg

_Column = collections.namedtuple("_Column", "name type_code")

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


def _raise_pg(exc):
    msg = str(exc)
    if msg.startswith("SQLSTATE:"):
        code, _, detail = msg[len("SQLSTATE:"):].partition("|")
        cls = psycopg.errors.lookup(code) if code else psycopg.OperationalError
        raise cls(detail) from None
    raise psycopg.OperationalError(msg) from None


class FakeCursor:
    def __init__(self, cnx):
        self._cnx = cnx
        self._result = None
        self._pos = 0
        self._rowcount_override = None
        self.closed = False

    @property
    def connection(self):
        return self._cnx

    def execute(self, query, params=None, *, prepare=None):
        if isinstance(query, bytes):
            query = query.decode()
        self._rowcount_override = None
        try:
            self._result = self._cnx._rust.execute(query, params)
        except RuntimeError as e:
            _raise_pg(e)
        self._pos = 0
        return self

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

    def fetchone(self):
        rows = self._result.rows
        if self._pos >= len(rows):
            return None
        row = rows[self._pos]
        self._pos += 1
        return row

    def fetchmany(self, size=1):
        rows = self._result.rows[self._pos:self._pos + size]
        self._pos += len(rows)
        return rows

    def fetchall(self):
        rows = self._result.rows[self._pos:]
        self._pos = len(self._result.rows)
        return rows

    def nextset(self):
        return None

    def close(self):
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
        return self._cnx._rust.copy(sql_text)


class _Prepared:
    def __init__(self, rust_conn):
        self._rust = rust_conn

    @property
    def _names(self):
        return self._rust.prepared_count

    def clear(self):
        self._rust.clear_prepared()


class FakeConnection:
    autocommit = False
    prepare_threshold = 5

    def __init__(self, rust_conn):
        self._rust = rust_conn
        self._prepared = _Prepared(rust_conn)
        self._isolation = None

    @property
    def connection(self):
        return self

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
    def isolation_level(self, value):
        self._isolation = value

    @property
    def read_only(self):
        return False

    @read_only.setter
    def read_only(self, value):
        self._rust.set_readonly(bool(value))

    @property
    def closed(self):
        return self._rust.closed

    def commit(self):
        try:
            self._rust.commit()
        except RuntimeError as e:
            _raise_pg(e)

    def rollback(self):
        try:
            self._rust.rollback()
        except RuntimeError as e:
            _raise_pg(e)

    def close(self):
        self._rust.close()

    def execute(self, query, params=None):
        cur = self.cursor()
        cur.execute(query, params)
        return cur

    def pipeline(self):
        import contextlib
        return contextlib.nullcontext()


INSTALLED = {"count": 0, "connects": 0, "reused": 0, "delegated": 0, "copies": 0}

_IDLE = []
_IDLE_LOCK = threading.Lock()
_IDLE_PID = os.getpid()
MAX_IDLE = 8


def _dbname(dsn):
    if isinstance(dsn, dict):
        return dsn.get("dbname") or dsn.get("database")
    if not isinstance(dsn, str):
        return None
    for key, value in _parse_conninfo(dsn):
        if key == "dbname":
            return value
    return None


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


def _take_idle():
    global _IDLE_PID
    with _IDLE_LOCK:
        pid = os.getpid()
        if pid != _IDLE_PID:
            _IDLE.clear()
            _IDLE_PID = pid
            return None
        while _IDLE:
            conn = _IDLE.pop()
            if not conn.closed:
                return conn
    return None


def pool_stats():
    with _IDLE_LOCK:
        return {"idle": len(_IDLE), **INSTALLED}


def install():
    from odoo.db.pool import ConnectionPool

    orig_give_back = ConnectionPool.give_back
    orig_borrow = ConnectionPool.borrow

    def borrow(self, dsn, key=None):
        if CONNINFO and dsn and _dbname(dsn) != _dbname(CONNINFO):
            INSTALLED["delegated"] += 1
            return orig_borrow(self, dsn, key)
        INSTALLED["count"] += 1
        conn = _take_idle()
        if conn is not None:
            INSTALLED["reused"] += 1
        else:
            INSTALLED["connects"] += 1
            conn = RUST_DB.connect()
        return FakeConnection(conn)

    def give_back(self, connection, keep_in_pool=True):
        if not isinstance(connection, FakeConnection):
            return orig_give_back(self, connection, keep_in_pool)
        rust = connection._rust
        if not keep_in_pool or rust.closed:
            connection.close()
            return
        try:
            rust.rollback()
            rust.set_readonly(False)
        except Exception:
            connection.close()
            return
        with _IDLE_LOCK:
            if len(_IDLE) < MAX_IDLE:
                _IDLE.append(rust)
                return
        connection.close()

    ConnectionPool.borrow = borrow
    ConnectionPool.give_back = give_back
