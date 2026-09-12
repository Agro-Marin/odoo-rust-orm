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
    "supports_parent_store",
    "supports_record_rules",
    "supports_joined_m2m_read",
    "supports_column_scan",
    "supports_translation_terms",
)

PROTOCOL_METHODS = (
    "create_rows",
    "update_rows",
    "fetch",
    "search",
    "as_query",
    "get_existing_ids",
    "lock_for_update",
    "try_lock_for_update",
    "unlink_rows",
    "read_m2m_pairs",
    "link_m2m_pairs",
    "unlink_m2m_pairs",
)

#: Every call is counted, and `fetch` and `search` arrive on the hottest path
#: in the ORM -- so the counters are plain `Counter` increments under the GIL
#: with NO lock. Two threads incrementing the same key can lose one of the
#: two; nothing is corrupted, and a lost unit of instrumentation is the right
#: trade against serialising every row read in the process on one mutex. A
#: figure read out of here is therefore a lower bound under concurrency.
STATS = {
    "native": collections.Counter(),
    "delegated": collections.Counter(),
    "reasons": collections.Counter(),
}


def _count(bucket, key) -> None:
    STATS[bucket][key] += 1


def _delegated(method, reason) -> None:
    STATS["delegated"][method] += 1
    STATS["reasons"][reason] += 1
    if _port_logger.isEnabledFor(logging.DEBUG):
        _port_logger.debug("%s -> delegate (%s)", method, reason)


def stats():
    out = {
        "native": dict(STATS["native"]),
        "delegated": dict(STATS["delegated"]),
        "reasons": dict(STATS["reasons"]),
    }
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
    NATIVE: frozenset = frozenset({"update_rows"})

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
        _delegated("create_rows", "not implemented natively")
        return self._delegate.create_rows(model, stored_list, columns, col_fields)

    def update_rows(self, model, fnames, rows) -> None:
        if "update_rows" not in self.NATIVE:
            _delegated("update_rows", "not armed")
        elif _update_rows_native(model, fnames, rows):
            _count("native", "update_rows")
            return None
        return self._delegate.update_rows(model, fnames, rows)

    def fetch(self, model, query, column_fields, other_fields):
        _delegated("fetch", "not implemented natively")
        return self._delegate.fetch(model, query, column_fields, other_fields)

    def search(
        self, model, domain, offset, limit, order, *, check_access=True, prof=None
    ):
        _delegated("search", "not implemented natively")
        return self._delegate.search(
            model, domain, offset, limit, order, check_access=check_access, prof=prof
        )

    def as_query(self, model, ordered=True):
        _delegated("as_query", "not implemented natively")
        return self._delegate.as_query(model, ordered)

    def get_existing_ids(self, model, ids):
        _delegated("get_existing_ids", "not implemented natively")
        return self._delegate.get_existing_ids(model, ids)

    def lock_for_update(self, model, *, allow_referencing=False) -> None:
        _delegated("lock_for_update", "not implemented natively")
        return self._delegate.lock_for_update(
            model, allow_referencing=allow_referencing
        )

    def try_lock_for_update(self, model, *, allow_referencing=False, limit=None):
        _delegated("try_lock_for_update", "not implemented natively")
        return self._delegate.try_lock_for_update(
            model, allow_referencing=allow_referencing, limit=limit
        )

    def unlink_rows(self, model, sub_ids, Data, Defaults, Attachment):
        _delegated("unlink_rows", "not implemented natively")
        return self._delegate.unlink_rows(model, sub_ids, Data, Defaults, Attachment)

    def read_m2m_pairs(self, model, relation, column1, column2, ids):
        _delegated("read_m2m_pairs", "not implemented natively")
        return self._delegate.read_m2m_pairs(model, relation, column1, column2, ids)

    def link_m2m_pairs(self, model, relation, column1, column2, pairs) -> None:
        _delegated("link_m2m_pairs", "not implemented natively")
        return self._delegate.link_m2m_pairs(model, relation, column1, column2, pairs)

    def unlink_m2m_pairs(self, model, relation, column1, column2, pairs) -> None:
        _delegated("unlink_m2m_pairs", "not implemented natively")
        return self._delegate.unlink_m2m_pairs(model, relation, column1, column2, pairs)


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
