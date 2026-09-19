import collections
import contextlib
import logging
import weakref

from rust_engine_errors import KernelRefused, KernelRegistryStale

_logger = logging.getLogger("odoo.rust_kernel.backend")

_port_logger = logging.getLogger("odoo.rust_kernel.port")

PROTOCOL_FLAGS = (
    "sequences",
    "columns",
)

PROTOCOL_METHODS = (
    "create_rows",
    "update_rows",
    "fetch",
    "search",
    "search_raw",
    "as_query",
    "ancestors",
    "descendants",
    "read_group_rows",
    "get_existing_ids",
    "lock_for_update",
    "try_lock_for_update",
    "unlink_rows",
    "link_m2m_pairs",
    "unlink_m2m_pairs",
    "read_m2m_groups",
    "set_parent_paths",
    "move_parent_paths",
    "records_with_parent_changed",
    "timezone_names",
    "count_m2m_groups",
    "read_grouping_sets_rows",
    "has_rows_beyond",
    "has_cycle",
    "increment_columns_skip_locked",
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
    native = sum(v for k, v in out["native"].items() if "." not in k)
    total = native + sum(out["delegated"].values())
    out["native_share"] = (native / total) if total else 0.0
    return out


def reset_stats() -> None:
    for counter in STATS.values():
        counter.clear()


class RustBackend:
    NATIVE: frozenset = frozenset({"update_rows", "create_rows", "search_raw"})

    __slots__ = ("_delegate",)

    def __init__(self, delegate) -> None:
        self._delegate = delegate

    def __repr__(self) -> str:
        return "RustBackend(%s)" % type(self._delegate).__name__

    @property
    def delegate(self):
        return self._delegate

    @property
    def sequences(self):
        return self._delegate.sequences

    @property
    def columns(self):
        return self._delegate.columns

    def __getattr__(self, name):
        if name == "_delegate" or name.startswith("__"):
            raise AttributeError(name)
        _delegated(name, "not in this port's protocol")
        return getattr(self._delegate, name)

    def _armed(self, method, model) -> bool:
        if method not in self.NATIVE:
            _delegated(method, "not armed")
            return False
        registry = getattr(getattr(model, "env", None), "registry", None)
        if registry is not None and not getattr(registry, "ready", True):
            _delegated(method, "the registry is loading")
            return False
        return _routing_allows(model, method)

    def create_rows(self, model, stored_list, columns, col_fields):
        ids = None
        if self._armed("create_rows", model):
            ids = _create_rows_native(model, stored_list, columns, col_fields)
            if ids is not None:
                _count("native", "create_rows")
        if ids is None:
            ids = self._delegate.create_rows(model, stored_list, columns, col_fields)
        note_created(model, ids)
        return ids

    def update_rows(self, model, fnames, rows) -> None:
        if self._armed("update_rows", model) and _update_rows_native(
            model, fnames, rows
        ):
            _count("native", "update_rows")
            return None
        return self._delegate.update_rows(model, fnames, rows)

    def search_raw(self, model, domain, offset, limit, order, check_access=True):
        if self._armed("search_raw", model):
            query = empty_by_construction(model, domain)
            if query is not None:
                _count("native", "search_raw.empty")
                return query
            query = _search_native(model, domain, offset, limit, order, check_access)
            if query is not None:
                _count("native", "search_raw")
                return query
        return self._delegate.search_raw(
            model, domain, offset, limit, order, check_access=check_access
        )

    def fetch(self, *args, **kwargs):
        _delegated("fetch", "not implemented natively")
        return self._delegate.fetch(*args, **kwargs)

    def search(
        self, model, domain, offset, limit, order, *, check_access=True, prof=None
    ):
        if self._armed("search", model):
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

    def link_m2m_pairs(self, *args, **kwargs):
        _delegated("link_m2m_pairs", "not implemented natively")
        return self._delegate.link_m2m_pairs(*args, **kwargs)

    def unlink_m2m_pairs(self, *args, **kwargs):
        _delegated("unlink_m2m_pairs", "not implemented natively")
        return self._delegate.unlink_m2m_pairs(*args, **kwargs)

    def read_m2m_groups(self, *args, **kwargs):
        _delegated("read_m2m_groups", "not implemented natively")
        return self._delegate.read_m2m_groups(*args, **kwargs)

    def set_parent_paths(self, *args, **kwargs):
        _delegated("set_parent_paths", "not implemented natively")
        return self._delegate.set_parent_paths(*args, **kwargs)

    def move_parent_paths(self, *args, **kwargs):
        _delegated("move_parent_paths", "not implemented natively")
        return self._delegate.move_parent_paths(*args, **kwargs)

    def records_with_parent_changed(self, *args, **kwargs):
        _delegated("records_with_parent_changed", "not implemented natively")
        return self._delegate.records_with_parent_changed(*args, **kwargs)

    def timezone_names(self, *args, **kwargs):
        _delegated("timezone_names", "not implemented natively")
        return self._delegate.timezone_names(*args, **kwargs)

    def count_m2m_groups(self, *args, **kwargs):
        _delegated("count_m2m_groups", "not implemented natively")
        return self._delegate.count_m2m_groups(*args, **kwargs)

    def ancestors(self, *args, **kwargs):
        _delegated("ancestors", "not implemented natively")
        return self._delegate.ancestors(*args, **kwargs)

    def read_grouping_sets_rows(self, *args, **kwargs):
        _delegated("read_grouping_sets_rows", "not implemented natively")
        return self._delegate.read_grouping_sets_rows(*args, **kwargs)

    def has_rows_beyond(self, *args, **kwargs):
        _delegated("has_rows_beyond", "not implemented natively")
        return self._delegate.has_rows_beyond(*args, **kwargs)

    def has_cycle(self, *args, **kwargs):
        _delegated("has_cycle", "not implemented natively")
        return self._delegate.has_cycle(*args, **kwargs)

    def increment_columns_skip_locked(self, *args, **kwargs):
        _delegated("increment_columns_skip_locked", "not implemented natively")
        return self._delegate.increment_columns_skip_locked(*args, **kwargs)


KERNEL_FOR = None


def _kernel(env):
    if KERNEL_FOR is not None:
        return KERNEL_FOR(env)
    try:
        import rust_orm_shim
    except ImportError:
        return None
    if not rust_orm_shim._bound_db(env) or not rust_orm_shim._ensure_kernel(env):
        return None
    return rust_orm_shim.KERNEL


_SHIM = None


def _routing_allows(model, method) -> bool:
    global _SHIM
    if _SHIM is None:
        try:
            import rust_orm_shim
        except ImportError:
            _delegated(method, "no routing shim in this process")
            return False
        _SHIM = rust_orm_shim
    if _SHIM.MODE != "on":
        _delegated(method, "routing mode is %s" % _SHIM.MODE)
        return False
    if not _SHIM._policy_allows(model._name):
        _delegated(method, "routing policy excludes the model")
        return False
    return True


def _uniform_values(rows):
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
        _delegated("update_rows", "the registry moved under this kernel")
        return False

    values = _uniform_values(rows)
    try:
        statements = []
        if values is not None:
            sql, repeats = kernel.update_rows_sql(
                model._name, list(fnames), uniform=True, row_count=len(rows)
            )
            params = []
            for value, repeat in zip(values, repeats, strict=True):
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


#: Per cursor, the ids each model created in the open transaction. Cleared
#: with the transaction (the method shim's untaint hook), never on a
#: savepoint rollback: an id rolled back exists nowhere, so a search for
#: references to it is empty either way.
_CREATED = weakref.WeakKeyDictionary()


def note_created(model, ids):
    env = getattr(model, "env", None)
    if not ids or env is None:
        return
    try:
        created = _CREATED.setdefault(env.cr, {})
    except TypeError:
        return
    created.setdefault(model._name, set()).update(ids)


def forget_created(cr) -> None:
    with contextlib.suppress(TypeError):
        _CREATED.pop(cr, None)


def _ids_of(value):
    if isinstance(value, int) and not isinstance(value, bool):
        return {value}
    if isinstance(value, (list, tuple, set, frozenset)) and value:
        if all(isinstance(v, int) and not isinstance(v, bool) for v in value):
            return set(value)
    return None


def empty_by_construction(model, domain):
    """A Query with no rows, when the domain can match none: it asks a
    table this transaction has not written for rows that reference an id
    this transaction created. No other transaction can see that id, so a
    row naming it can only have been written here -- and none was.

    The shape is deliberately narrow: an AND-only domain, one leaf on a
    many2one to the created model (or the `model`/`res_id` pair), every id
    in the leaf created here, the searched table absent from the
    connection's written set, and no write the scanner could not read.
    Anything else compiles as usual. A trigger writing one table on an
    insert into another is the one path outside this reasoning, and Odoo
    defines none.
    """
    from odoo.orm.domain.ast import DomainCondition, DomainNot, DomainOr

    env = model.env
    try:
        created = _CREATED.get(env.cr)
    except TypeError:
        return None
    if not created:
        return None
    try:
        import rust_orm_shim

        conn = rust_orm_shim._rust_conn(env)
    except Exception:
        return None
    if conn.writes_untracked:
        return None
    table = getattr(model, "_table", None)
    if not table or not model._auto or table in conn.written_tables:
        return None

    leaves = []
    for node in _walk(domain):
        if isinstance(node, (DomainOr, DomainNot)):
            return None
        if isinstance(node, DomainCondition):
            if isinstance(node.value, type(domain)):
                return None  # a sub-domain: `any`/`not any`, out of shape
            leaves.append(node)
    named_model = None
    for leaf in leaves:
        if leaf.field_expr in ("model", "res_model") and leaf.operator == "=":
            named_model = leaf.value
    for leaf in leaves:
        if leaf.operator not in ("in", "="):
            continue
        ids = _ids_of(leaf.value)
        if not ids:
            continue
        field = model._fields.get(leaf.field_expr)
        if field is None:
            continue
        if field.type == "many2one":
            target = field.comodel_name
        elif leaf.field_expr == "res_id" and named_model:
            target = named_model
        else:
            continue
        mine = created.get(target)
        if mine and ids <= mine:
            return model.browse()._as_query()
    return None


def _search_native(model, domain, offset, limit, order, check_access):
    import json

    from odoo.libs.sql.builder import SQL
    from odoo.tools.query import Query

    env = model.env
    if not check_access and not env.su:
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
