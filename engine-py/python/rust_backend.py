"""The Rust engine on Odoo's persistence port.

`odoo/orm/runtime/backend.py` declares `StorageBackend`, the protocol every
row read and every row write in the ORM goes through, and ships two
implementors: `PostgresBackend` and `InMemoryBackend`. `RustBackend` is a
third. It wraps one of the others and answers whatever the kernel can answer
natively, delegating the rest unchanged.

This is a different seam from `rust_orm_shim`, which replaces methods on
`BaseModel`. That one routes the calls a client makes -- `search_read`,
`read`, `_read_group` -- and has to decide in Python whether the kernel
supports each one. The port is below all of them: every path that touches a
row arrives here, including the ones no RPC method names, and the protocol is
pinned by `odoo/orm/tests/test_backend_dispatch_surface.py` rather than
discovered by reading call sites.

The two coexist. Installing the port changes no answer on its own: an
unarmed `RustBackend` is its delegate with a counter attached.
"""

import collections
import logging

from rust_engine_errors import KernelRefused, KernelRegistryStale

_logger = logging.getLogger("odoo.rust_kernel.backend")

# One line per call the port handed back, with the reason -- which call, not
# how many, since `STATS` already aggregates. A call answered natively logs
# nothing: that is the path this is used to widen, and a line per row read is
# not a diagnostic.
_port_logger = logging.getLogger("odoo.rust_kernel.port")

#: Every member of `StorageBackend`. The conformance test reads this list out
#: of the fork's own protocol and fails when the two disagree, so a method
#: added upstream cannot reach a `RustBackend` that silently lacks it.
PROTOCOL_FLAGS = (
    "sequences",
    "columns",
    "supports_parent_store",
    "supports_record_rules",
    "supports_joined_m2m_read",
    "supports_column_scan",
    "supports_translation_terms",
    "supports_recursive_queries",
)

PROTOCOL_METHODS = (
    "create_rows",
    "update_rows",
    "fetch",
    "search",
    "as_query",
    "descendants",
    "read_group_rows",
    "get_existing_ids",
    "lock_for_update",
    "try_lock_for_update",
    "unlink_rows",
    "read_m2m_pairs",
    "link_m2m_pairs",
    "unlink_m2m_pairs",
)

STATS = {
    "native": collections.Counter(),
    "delegated": collections.Counter(),
    "reasons": collections.Counter(),
    "reasons_by_method": collections.Counter(),
}


def _count(bucket, key) -> None:
    STATS[bucket][key] += 1


def _delegated(method, reason) -> None:
    STATS["delegated"][method] += 1
    STATS["reasons"][reason] += 1
    STATS["reasons_by_method"][method, reason] += 1
    if _port_logger.isEnabledFor(logging.DEBUG):
        _port_logger.debug("%s -> delegate (%s)", method, reason)


def stats():
    out = {
        "native": dict(STATS["native"]),
        "delegated": dict(STATS["delegated"]),
        "reasons": dict(STATS["reasons"]),
        "reasons_by_method": {},
    }
    for (method, reason), n in STATS["reasons_by_method"].items():
        out["reasons_by_method"].setdefault(method, {})[reason] = n
    # The per-shape counters are a breakdown of `update_rows`, not calls of
    # their own: counting them again would make the share exceed one.
    native = sum(v for k, v in out["native"].items() if "." not in k)
    total = native + sum(out["delegated"].values())
    out["native_share"] = (native / total) if total else 0.0
    return out


def reset_stats() -> None:
    for counter in STATS.values():
        counter.clear()


class RustBackend:
    """A `StorageBackend` that answers natively where it can.

    The delegate is the backend the transaction would otherwise have used, so
    a `RustBackend` that implements nothing natively is behaviourally that
    backend. Native coverage is declared per method by `NATIVE`, which is
    empty until a method has a differential test behind it: a method absent
    from that set is delegated without ever being asked.
    """

    #: Methods this backend may answer without the delegate. Adding a name
    #: here is the arming step, and it is deliberately separate from writing
    #: the implementation: an implementation with no verification stays off.
    NATIVE: frozenset = frozenset({"update_rows", "create_rows"})

    __slots__ = ("_delegate",)

    def __init__(self, delegate) -> None:
        self._delegate = delegate

    def __repr__(self) -> str:
        return "RustBackend(%s)" % type(self._delegate).__name__

    @property
    def delegate(self):
        return self._delegate

    # The support flags describe what the STORAGE can do, and the storage is
    # the delegate's. Answering differently here would change ORM behaviour
    # (`supports_parent_store` gates whether `parent_path` is maintained at
    # all) for a reason that has nothing to do with who computes the SQL.
    # Sequence storage is a port of its own beside this one, and the port a
    # transaction uses is the one its row storage lives in.
    @property
    def sequences(self):
        return self._delegate.sequences

    @property
    def columns(self):
        return self._delegate.columns

    @property
    def supports_recursive_queries(self) -> bool:
        return self._delegate.supports_recursive_queries

    def __getattr__(self, name):
        if name == "_delegate" or name.startswith("__"):
            raise AttributeError(name)
        _delegated(name, "not in this port's protocol")
        return getattr(self._delegate, name)

    @property
    def supports_parent_store(self) -> bool:
        return self._delegate.supports_parent_store

    @property
    def supports_record_rules(self) -> bool:
        return self._delegate.supports_record_rules

    @property
    def supports_joined_m2m_read(self) -> bool:
        return self._delegate.supports_joined_m2m_read

    @property
    def supports_column_scan(self) -> bool:
        return self._delegate.supports_column_scan

    @property
    def supports_translation_terms(self) -> bool:
        return self._delegate.supports_translation_terms

    def create_rows(self, model, stored_list, columns, col_fields):
        if "create_rows" not in self.NATIVE:
            _delegated("create_rows", "not armed")
        else:
            ids = _create_rows_native(model, stored_list, columns, col_fields)
            if ids is not None:
                _count("native", "create_rows")
                return ids
        return self._delegate.create_rows(model, stored_list, columns, col_fields)

    def update_rows(self, model, fnames, rows) -> None:
        if "update_rows" not in self.NATIVE:
            _delegated("update_rows", "not armed")
        elif _update_rows_native(model, fnames, rows):
            _count("native", "update_rows")
            return None
        return self._delegate.update_rows(model, fnames, rows)

    def fetch(self, *args, **kwargs):
        _delegated("fetch", "not implemented natively")
        return self._delegate.fetch(*args, **kwargs)

    def search(
        self, model, domain, offset, limit, order, *, check_access=True, prof=None
    ):
        if "search" not in self.NATIVE:
            _delegated("search", "not armed")
        else:
            query = _search_native(model, domain, offset, limit, order, check_access)
            if query is not None:
                _count("native", "search")
                return query
        return self._delegate.search(
            model, domain, offset, limit, order, check_access=check_access, prof=prof
        )

    def as_query(self, *args, **kwargs):
        _delegated("as_query", "not implemented natively")
        return self._delegate.as_query(*args, **kwargs)

    def descendants(self, *args, **kwargs):
        _delegated("descendants", "not implemented natively")
        return self._delegate.descendants(*args, **kwargs)

    def read_group_rows(self, *args, **kwargs):
        _delegated("read_group_rows", "not implemented natively")
        return self._delegate.read_group_rows(*args, **kwargs)

    def get_existing_ids(self, *args, **kwargs):
        _delegated("get_existing_ids", "not implemented natively")
        return self._delegate.get_existing_ids(*args, **kwargs)

    def lock_for_update(self, *args, **kwargs):
        _delegated("lock_for_update", "not implemented natively")
        return self._delegate.lock_for_update(*args, **kwargs)

    def try_lock_for_update(self, *args, **kwargs):
        _delegated("try_lock_for_update", "not implemented natively")
        return self._delegate.try_lock_for_update(*args, **kwargs)

    def unlink_rows(self, *args, **kwargs):
        _delegated("unlink_rows", "not implemented natively")
        return self._delegate.unlink_rows(*args, **kwargs)

    def read_m2m_pairs(self, *args, **kwargs):
        _delegated("read_m2m_pairs", "not implemented natively")
        return self._delegate.read_m2m_pairs(*args, **kwargs)

    def link_m2m_pairs(self, *args, **kwargs):
        _delegated("link_m2m_pairs", "not implemented natively")
        return self._delegate.link_m2m_pairs(*args, **kwargs)

    def unlink_m2m_pairs(self, *args, **kwargs):
        _delegated("unlink_m2m_pairs", "not implemented natively")
        return self._delegate.unlink_m2m_pairs(*args, **kwargs)


#: Set by the addon to the same callable `rust_orm_shim` uses, so the port
#: and the method routing share one kernel per worker rather than building two.
KERNEL_FOR = None


def _kernel(env):
    """The kernel for this environment, or None to delegate.

    The port asks the routing shim rather than holding its own handle: one
    kernel per worker is what makes the registry watermark, the statement
    cache generation and the staleness flag mean the same thing on both
    paths.
    """
    if KERNEL_FOR is not None:
        return KERNEL_FOR(env)
    try:
        import rust_orm_shim
    except ImportError:
        return None
    if not rust_orm_shim._bound_db(env) or not rust_orm_shim._ensure_kernel(env):
        return None
    return rust_orm_shim.KERNEL


def _uniform_values(rows):
    """`PostgresBackend._resolve_uniform_update_values`, decided here.

    It compares Python values, so it stays in Python; what the answer selects
    is which of the two statements the kernel composes.
    """
    from odoo.orm.runtime.backend import _UNIFORM_UPDATE_TYPES

    if len(rows) < 2:
        return None
    values = rows[0][1:]
    if not all(isinstance(value, _UNIFORM_UPDATE_TYPES) for value in values):
        return None
    if any(row[1:] != values for row in rows):
        return None
    return values


def _update_rows_native(model, fnames, rows) -> bool:
    """Run the kernel's `UPDATE` for this column-group, or report that it did not.

    Returning False is not an error: every column the kernel refuses -- a
    company-dependent one, a field it does not carry, a registry that has
    moved under the process -- is one the delegate writes correctly, and the
    caller falls through to it.
    """
    from odoo.orm.primitives import UPDATE_BATCH_SIZE

    env = model.env
    kernel = _kernel(env)
    if kernel is None:
        _delegated("update_rows", "no kernel in this process")
        return False
    if getattr(env.registry, "registry_sequence", None) not in (
        None,
        kernel.registry_sequence,
    ):
        # The same guard the routed read path applies: an environment whose
        # Python registry has moved cannot use a kernel built from the old one.
        _delegated("update_rows", "the registry moved under this kernel")
        return False

    # Every statement is composed BEFORE any of them runs. A refusal on the
    # second batch after the first had already been written would leave the
    # caller to delegate a group half of which was written here, and the two
    # statements are only harmlessly idempotent by accident of what they
    # assign. Composing first makes the refusal atomic.
    values = _uniform_values(rows)
    try:
        statements = []
        if values is not None:
            sql, repeats = kernel.update_rows_sql(
                model._name, list(fnames), uniform=True, row_count=len(rows)
            )
            params = []
            for value, repeat in zip(values, repeats, strict=True):
                # A whole-value translated column names its value three times
                # in the merge expression, so it is bound three times.
                params.extend([value] * repeat)
            params.append([row[0] for row in rows])
            statements.append(("update_rows.uniform", sql, params))
        else:
            for start in range(0, len(rows), UPDATE_BATCH_SIZE):
                batch = rows[start : start + UPDATE_BATCH_SIZE]
                sql, _repeats = kernel.update_rows_sql(
                    model._name, list(fnames), uniform=False, row_count=len(batch)
                )
                statements.append(
                    (
                        "update_rows.values",
                        sql,
                        [value for row in batch for value in row],
                    )
                )
    except (KernelRefused, KernelRegistryStale) as exc:
        _delegated("update_rows", str(exc))
        return False

    for shape, sql, params in statements:
        env.cr.execute(sql, params)
        _count("native", shape)
    return True


def _create_rows_native(model, stored_list, columns, col_fields):
    """Run the kernel's `INSERT` for these rows, or return None to delegate.

    Only the INSERT strategy. `PostgresBackend.create_rows` sends ten rows or
    more as a binary `COPY` unless the cursor is in a pipeline, and that path
    is the cursor's: it preallocates the ids, resolves the column type OIDs and
    streams the rows, all through `RustCopy`, which already encodes the stream
    in Rust and is verified by `harness/copy_path.py`. The decision is taken
    with the fork's own constants so the two strategies split exactly where
    Python splits them.

    The values are converted by the fork's own `_prepare_insert_rows`: a
    translated or company-dependent column's jsonb is decided by
    `convert_to_column_insert`, which reads the environment's language and
    company, and that is Python's to decide.
    """
    from odoo.libs.sql.builder import SQL
    from odoo.orm.runtime.backend import (
        COPY_DISABLED,
        COPY_THRESHOLD,
        PostgresBackend,
    )

    env = model.env
    cr = env.cr
    if (
        not COPY_DISABLED
        and col_fields
        and len(stored_list) >= COPY_THRESHOLD
        and not cr.in_pipeline
    ):
        _delegated("create_rows", "COPY strategy: the cursor owns it")
        return None
    kernel = _kernel(env)
    if kernel is None:
        _delegated("create_rows", "no kernel in this process")
        return None
    if getattr(env.registry, "registry_sequence", None) not in (
        None,
        kernel.registry_sequence,
    ):
        _delegated("create_rows", "the registry moved under this kernel")
        return None

    if col_fields:
        rows = PostgresBackend._prepare_insert_rows(
            model, stored_list, columns, col_fields
        )
        params = []
        for row in rows:
            for value in row:
                # `SQL` inlines an `SQL` value as code and expands a tuple
                # into a parenthesised list, so neither is ONE parameter. No
                # column converter returns either today; one that starts to
                # would change the statement's shape, which is the delegate's.
                if isinstance(value, (SQL, tuple)):
                    _delegated(
                        "create_rows",
                        "a converted value is %s, not a parameter"
                        % type(value).__name__,
                    )
                    return None
                params.append(value)
        insert_columns = list(columns)
    else:
        params = []
        insert_columns = []

    try:
        sql = kernel.insert_rows_sql(model._name, insert_columns, len(stored_list))
    except (KernelRefused, KernelRegistryStale) as exc:
        _delegated("create_rows", str(exc))
        return None

    cr.execute(sql, params or None)
    return [id_ for (id_,) in cr.fetchall()]


def _revive(value):
    if isinstance(value, list):
        return [_revive(item) for item in value]
    if isinstance(value, dict):
        import datetime

        if "__date__" in value:
            return datetime.date.fromisoformat(value["__date__"])
        if "__datetime__" in value:
            return datetime.datetime.fromisoformat(value["__datetime__"])
    return value


class _NoWireForm(Exception):
    pass


def _domain_json(domain):
    """The optimized domain as the prefix list the kernel parses, or raise.

    `list(domain)` is Odoo's own serialisation and already thaws a sub-domain
    into a list. What it cannot express is a value the kernel has no wire form
    for -- a `Query` from a field's `search=` method, an `SQL` object, a
    custom SQL node -- and every one of those is a delegation, not a guess.
    """
    import json

    from odoo.orm.domain.ast import DomainCustom

    if any(isinstance(node, DomainCustom) for node in _walk(domain)):
        raise _NoWireForm("a custom SQL condition")

    import wire

    def refuse(value):
        raise _NoWireForm("a %s value" % type(value).__name__)

    return json.dumps(wire.ser(list(domain)), default=refuse)


def _walk(node):
    from odoo.orm.domain.ast import Domain, DomainCondition, DomainNary, DomainNot

    yield node
    if isinstance(node, DomainNary):
        for child in node.children:
            yield from _walk(child)
    elif isinstance(node, DomainNot):
        yield from _walk(node.child)
    elif isinstance(node, DomainCondition) and isinstance(node.value, Domain):
        yield from _walk(node.value)


def _security_written(env):
    """Why this transaction's security cannot be trusted to the kernel, or None.

    The kernel compiles record rules, group membership and company access from
    a snapshot keyed by Odoo's signalling watermark, and the watermark moves on
    COMMIT. A transaction that wrote an `ir.rule`, a group or a user sees its
    own change in Python and not in the kernel, so a native WHERE there would
    apply the rules as they were. The method shim already tracks exactly that
    per cursor (`DIRTY_CRS`, set by its create/write/unlink wrappers and
    cleared on commit and rollback); the port reads the same set. Without the
    shim nothing records the writes, and that is a delegation too.
    """
    try:
        import rust_orm_shim
    except ImportError:
        return "the method shim is not installed, so security writes are not tracked"
    if rust_orm_shim._INSTALLED is None:
        return "the method shim is not installed, so security writes are not tracked"
    try:
        if env.cr in rust_orm_shim.DIRTY_CRS:
            return "this transaction wrote a security model"
    except TypeError:
        return "the cursor cannot be tracked for security writes"
    return None


def _search_native(model, domain, offset, limit, order, check_access):
    """A `Query` whose WHERE the kernel compiled, or None to delegate.

    Everything after the WHERE is the fork's: `_order_to_sql` on the same
    query, the limit and the offset, so what differs from
    `_prepare_postgres_search_query` is exactly the part `domain._to_sql` and
    the rule domain's `_to_sql` contribute.
    """
    import json

    from odoo.libs.sql.builder import SQL
    from odoo.tools.query import Query

    env = model.env
    if not check_access and not env.su:
        # `_search(bypass_access=True)` without superuser: no rules on the
        # root, and the sub-queries still apply their comodels' rules. The
        # kernel's two modes are "rules everywhere" and "superuser", and
        # neither is this.
        _delegated("search", "bypass_access without superuser")
        return None
    why = _security_written(env)
    if why:
        _delegated("search", why)
        return None
    kernel = _kernel(env)
    if kernel is None:
        _delegated("search", "no kernel in this process")
        return None
    if getattr(env.registry, "registry_sequence", None) not in (
        None,
        kernel.registry_sequence,
    ):
        _delegated("search", "the registry moved under this kernel")
        return None
    try:
        domain_json = _domain_json(domain)
    except _NoWireForm as exc:
        _delegated("search", "the domain carries %s" % exc)
        return None

    context = env.context
    request = json.dumps(
        {
            "model": model._name,
            "method": "search",
            "registry_sequence": env.registry.registry_sequence,
            "uid": env.uid,
            "su": bool(env.su),
            "lang": context.get("lang") or None,
            "allowed_company_ids": context.get("allowed_company_ids") or None,
            "active_test": bool(context.get("active_test", True)),
            "tz": context.get("tz") or None,
            "root_active_test": False,
            "trusted_domain": True,
        }
    )
    request = request[:-1] + ', "domain": ' + domain_json + "}"
    try:
        import rust_orm_shim

        conn = rust_orm_shim._rust_conn(env)
        # Offline first: with the watermark checked earlier in this
        # transaction and the identity and rules cached, the compile sends no
        # statement, so nothing can fail on the server and no savepoint is
        # needed. Only a compile that has to ask the database runs online, and
        # that one does need the savepoint -- a database error in it must roll
        # back to here rather than abort the transaction Python would finish.
        answer = kernel.search_where(conn, request, offline=True)
        if answer is None:
            _count("native", "search.online")
            with env.cr.savepoint(flush=False):
                answer = kernel.search_where(conn, request, offline=False)
        fragment, payload = answer
    except (KernelRefused, KernelRegistryStale) as exc:
        _delegated("search", str(exc))
        return None

    payload = json.loads(payload)
    # Odoo's WHERE carries `to_flush`, the fields the cursor writes before the
    # statement runs; without them a search after a write in the same
    # transaction reads the old row. The kernel reports every column its SQL
    # reads -- the domain's, its sub-queries' `active` and field domains, the
    # comodels' rules -- which is that set by construction rather than by a
    # walk that predicts it. A field the Python registry does not know is a
    # disagreement about the model, and not one to flush around.
    to_flush = []
    for name, fname in payload["touched"]:
        field = env[name]._fields.get(fname) if name in env else None
        if field is None:
            _delegated(
                "search",
                "the kernel read %s.%s, which python does not know" % (name, fname),
            )
            return None
        to_flush.append(field)

    query = Query(env, model._table, model._table_sql)
    if fragment != "TRUE":
        query.add_where(SQL(fragment, *_revive(payload["params"]), to_flush=to_flush))
    if order:
        query.order = model._order_to_sql(order, query) or SQL.identifier(
            model._table, "id"
        )
    if limit is not None and limit is not False:
        query.limit = 1 if limit is True else limit
    if offset is not None and offset is not False:
        query.offset = 1 if offset is True else offset
    return query


_INSTALLED = None
DBNAME = None


def installed():
    return _INSTALLED


def install(dbname=None):
    """Put a `RustBackend` in front of every PostgreSQL transaction.

    `Transaction.__init__` is the only place a backend is chosen -- one site,
    `environment.py:121` being the only caller -- so wrapping it covers every
    transaction without touching the ORM. A transaction that chose
    `InMemoryBackend` keeps it: that one is how the ORM runs with no database
    at all, and wrapping it would put a kernel that needs a connection in
    front of the case defined by not having one.
    """
    global _INSTALLED, DBNAME
    if dbname is not None:
        DBNAME = dbname
    if _INSTALLED is not None:
        return _INSTALLED

    from odoo.orm.runtime.backend import POSTGRES_BACKEND
    from odoo.orm.runtime.transaction import Transaction

    orig_init = Transaction.__init__

    def __init__(self, registry, storage=None) -> None:
        orig_init(self, registry, storage)
        if self.backend is POSTGRES_BACKEND and (
            DBNAME is None or registry.db_name == DBNAME
        ):
            self.backend = RustBackend(POSTGRES_BACKEND)

    Transaction.__init__ = __init__
    _INSTALLED = {"orig_init": orig_init}
    _logger.info("rust_backend: installed on the persistence port")
    return _INSTALLED


def uninstall() -> None:
    global _INSTALLED
    if _INSTALLED is None:
        return
    from odoo.orm.runtime.transaction import Transaction

    Transaction.__init__ = _INSTALLED["orig_init"]
    _INSTALLED = None
