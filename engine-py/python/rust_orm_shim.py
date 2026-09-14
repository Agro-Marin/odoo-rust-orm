import collections
import contextlib
import datetime
import json
import logging
import math
import operator
import os
import random
import secrets
import threading
import time
import weakref

from rust_engine_errors import KernelRefused, KernelRegistryStale

_logger = logging.getLogger("odoo.rust_kernel.routing")
# Every call the gate turns away, one line each, with the reason. `STATS` and
# `GATE_REASONS` are the same information aggregated; this logger is what says
# WHICH call, which is what a session widening the routed share needs.
_gate_logger = logging.getLogger("odoo.rust_kernel.gate")
# One line per call that reached the kernel, with the wire sizes and the split
# between the kernel's own time and the revive/warm work this shim does after.
_call_logger = logging.getLogger("odoo.rust_kernel.call")

KERNEL = None
DBNAME = None

KERNEL_FACTORY = None
_KERNEL_TRIED_PID = None
_KERNEL_LOCK = threading.Lock()
_KERNEL_INCOMPLETE = [None]

PROCESS_HOOK = None
_PROCESS_PID = None
STATS = {
    "kernel": 0,
    "fallback_gate": 0,
    "fallback_error": 0,
    "fallback_flush": 0,
    "errors": {},
    "errors_by_model": {},
    "shadow_ok": 0,
    "shadow_diff": 0,
    "tripped": [],
    "quarantined": [],
}


def _env_set(name):
    return frozenset(
        x.strip() for x in os.environ.get(name, "").split(",") if x.strip()
    )


MODE = os.environ.get("RUSTORM_ROUTE", "on").strip().lower()
ONLY = _env_set("RUSTORM_ROUTE_ONLY")
EXCEPT = _env_set("RUSTORM_ROUTE_EXCEPT")
BREAKER = int(os.environ.get("RUSTORM_ROUTE_BREAKER", "0") or 0)

SAMPLE = float(os.environ.get("RUSTORM_ROUTE_SAMPLE", "0") or 0)
_RNG = random.Random()


def set_sample(fraction):
    global SAMPLE
    fraction = float(fraction)
    if not 0.0 <= fraction <= 1.0:
        raise ValueError("the sample fraction must be between 0 and 1")
    SAMPLE = fraction
    _logger.info("rust kernel verification sample is now %.4f", SAMPLE)
    return SAMPLE


def set_mode(mode):
    global MODE
    if mode not in ("on", "off", "shadow"):
        raise ValueError("mode must be on, off or shadow")
    MODE = mode
    _logger.info("rust kernel routing mode is now %r", mode)
    return MODE


def stats():
    out = dict(STATS)
    out["mode"] = MODE
    out["sample"] = SAMPLE
    total = out["kernel"] + out["fallback_gate"] + out["fallback_error"]
    out["routed_share"] = (out["kernel"] / total) if total else 0.0
    out["gate_reasons"] = GATE_REASONS.most_common(8)
    return out


def reset_breaker(model_name=None) -> None:
    if model_name is None:
        STATS["errors_by_model"].clear()
        STATS["tripped"].clear()
        STATS["quarantined"].clear()
    else:
        STATS["errors_by_model"].pop(model_name, None)
        if model_name in STATS["tripped"]:
            STATS["tripped"].remove(model_name)
        if model_name in STATS["quarantined"]:
            STATS["quarantined"].remove(model_name)


def _bound_db(env):
    return DBNAME is None or env.registry.db_name == DBNAME


def _ensure_kernel(env):
    global KERNEL, _KERNEL_TRIED_PID, _PROCESS_PID
    if os.getpid() != _PROCESS_PID and PROCESS_HOOK is not None:
        _PROCESS_PID = os.getpid()
        try:
            PROCESS_HOOK()
        except Exception:
            _logger.exception("the rust engine process hook failed")
    if KERNEL is not None:
        return True
    if KERNEL_FACTORY is None:
        _logger.debug("no kernel factory yet; the addon has not armed this process")
        return False
    if os.getpid() == _KERNEL_TRIED_PID:
        # one attempt per process: a worker that could not build the kernel
        # serves from Python for its whole life rather than retrying per call
        return False
    loading = (os.getpid(), id(env.registry), len(env.registry.models))
    if loading == _KERNEL_INCOMPLETE[0]:
        return False
    with _KERNEL_LOCK:
        if KERNEL is not None:
            return True
        if os.getpid() == _KERNEL_TRIED_PID:
            return False
        started = time.monotonic()
        try:
            KERNEL = KERNEL_FACTORY(env.registry)
            forget_gates()
            _logger.info(
                "built the rust kernel in pid %d in %.1f ms",
                os.getpid(),
                (time.monotonic() - started) * 1000,
            )
        except Exception as exc:
            if "incomplete export" in str(exc):
                _KERNEL_INCOMPLETE[0] = loading
                _logger.debug(
                    "not building the rust kernel yet: %s models loaded so far",
                    loading[2],
                )
                return False
            _KERNEL_TRIED_PID = os.getpid()
            _logger.exception(
                "could not build the rust kernel in this process; "
                "it will serve from python"
            )
            return False
    return KERNEL is not None


_BASELINE = threading.local()


def _in_baseline():
    return getattr(_BASELINE, "depth", 0) > 0


def _baseline(fn, *args, **kwargs):
    _BASELINE.depth = getattr(_BASELINE, "depth", 0) + 1
    try:
        return fn(*args, **kwargs)
    finally:
        _BASELINE.depth -= 1


def _policy_allows(name) -> bool:
    if MODE == "off":
        return _refuse("routing mode is off")
    if _in_baseline():
        # the shadow comparison is running Python's own answer; routing it
        # again would compare the kernel with itself
        return _refuse("inside the shadow baseline")
    if ONLY and name not in ONLY:
        return _refuse("not in RUSTORM_ROUTE_ONLY")
    if name in EXCEPT:
        return _refuse("in RUSTORM_ROUTE_EXCEPT")
    if name in STATS["quarantined"]:
        return _refuse("quarantined after a shadow divergence")
    if BREAKER and STATS["errors_by_model"].get(name, 0) >= BREAKER:
        if name not in STATS["tripped"]:
            STATS["tripped"].append(name)
            _logger.warning(
                "rust kernel routing disabled for %s after %d errors; "
                "reset_breaker(%r) to retry",
                name,
                BREAKER,
                name,
            )
        return _refuse("breaker tripped after %d errors" % BREAKER)
    return True


def _verify_this_one():
    return SAMPLE > 0 and _RNG.random() < SAMPLE


def _shadow(model, method, kernel_result, python_result, same=operator.eq) -> None:
    if same(kernel_result, python_result):
        STATS["shadow_ok"] += 1
        _call_logger.debug("%s.%s verified against python", model._name, method)
        return
    _quarantine(model, method, kernel_result, python_result)


ORDER_DEPENDENT_AGGREGATES = frozenset({"sum", "avg"})


def _order_dependent_aggregates(model, aggregates):
    positions = set()
    for i, spec in enumerate(aggregates):
        fname, _sep, func = spec.rpartition(":")
        f = model._fields.get(fname)
        column_type = getattr(f, "column_type", None)
        if (
            func in ORDER_DEPENDENT_AGGREGATES
            and column_type
            and column_type[0] == "float8"
        ):
            positions.add(i)
    return positions


def _float_aggregates_agree(kernel_value, python_value):
    if isinstance(kernel_value, float) and isinstance(python_value, float):
        return math.isclose(kernel_value, python_value, rel_tol=1e-12, abs_tol=1e-9)
    return kernel_value == python_value


def _group_rows_agree(model, groupby, aggregates):
    order_dependent = {
        len(groupby) + i for i in _order_dependent_aggregates(model, aggregates)
    }
    if not order_dependent:
        return operator.eq

    def same(kernel_rows, python_rows):
        return len(kernel_rows) == len(python_rows) and all(
            len(k) == len(p)
            and all(
                _float_aggregates_agree(kv, pv) if i in order_dependent else kv == pv
                for i, (kv, pv) in enumerate(zip(k, p, strict=True))
            )
            for k, p in zip(kernel_rows, python_rows, strict=True)
        )

    return same


def _formatted_groups_agree(model, aggregates):
    order_dependent = {
        aggregates[i] for i in _order_dependent_aggregates(model, aggregates)
    }
    if not order_dependent:
        return operator.eq

    def same(kernel_groups, python_groups):
        return len(kernel_groups) == len(python_groups) and all(
            k.keys() == p.keys()
            and all(
                _float_aggregates_agree(v, p[key])
                if key in order_dependent
                else v == p[key]
                for key, v in k.items()
            )
            for k, p in zip(kernel_groups, python_groups, strict=True)
        )

    return same


def _quarantine(model, method, kernel_result, python_result) -> None:
    STATS["shadow_diff"] += 1
    if model._name not in STATS["quarantined"]:
        STATS["quarantined"].append(model._name)
    _logger.error(
        "SHADOW DIVERGENCE %s.%s; routing disabled until reset_breaker\n"
        "  python: %.400s\n  kernel: %.400s",
        model._name,
        method,
        python_result,
        kernel_result,
    )


def _verified_baseline(model, method, kernel_result, fn, *args, **kwargs):
    try:
        return _baseline(fn, *args, **kwargs)
    except Exception as exc:
        # A native value and a Python exception are different outcomes. Keep
        # Python's exception, quarantine immediately, and never invoke it twice.
        _quarantine(model, method, kernel_result, {"python_error": repr(exc)})
        raise


SECURITY_MODELS = frozenset(
    {
        "ir.rule",
        "ir.model.access",
        "res.groups",
        "res.users",
        "ir.default",
        "res.company",
        "res.lang",
        "ir.model",
        "ir.model.fields",
    }
)
DIRTY_CRS = weakref.WeakSet()


def _harmless_user_write(records, vals) -> bool:
    """A write to res.users that changes nothing Python's security caches read.

    Every write to res.users used to taint the cursor, and in the browser tours
    that was the largest single reason a read fell back to Python -- through
    `odoobot_state`, `image_1920`, a notification preference. Odoo itself
    names the fields whose change invalidates what it caches about a user:
    `_get_fields_invalidation()` (groups, active, lang, tz, companies, the
    session-token fields), overridable by addons, and a write outside that set
    leaves the rule domains Python answers from as they were. The kernel's
    snapshot of that user can stay in step with it. Creates and unlinks, and
    every other security model, still taint.
    """
    if records._name != "res.users" or not isinstance(vals, dict) or not vals:
        return False
    invalidating = getattr(records, "_get_fields_invalidation", None)
    if invalidating is None:
        return False
    touched = set(vals) & set(invalidating())
    if touched:
        return False
    if _gate_logger.isEnabledFor(logging.DEBUG):
        _gate_logger.debug(
            "a write to res.users (%s) touches none of its invalidation fields; "
            "the cursor stays routable",
            ", ".join(sorted(vals)),
        )
    return True


_BASE_METHODS = {}
_GATE_CACHE = {}


def _cache_key(model, *parts):
    return (model.env.registry.db_name, model._name) + parts


def forget_gates() -> None:
    _GATE_CACHE.clear()


ORDERS = {}


def snapshot_orders(registry) -> None:
    ORDERS.clear()
    ORDERS.update({name: registry[name]._order for name in registry})


def _order_names(order):
    for part in (order or "").split(","):
        name = part.strip().split(" ")[0].split(":")[0]
        if name:
            yield name


def _order_drifted(model, order, groupby=()):
    env = model.env
    todo = [(model, (*_order_names(order), *groupby))]
    seen = set()
    while todo:
        current, names = todo.pop()
        if current._name in seen:
            continue
        seen.add(current._name)
        exported = ORDERS.get(current._name)
        if exported is not None and exported != current._order:
            return True
        for name in (*names, *_order_names(current._order)):
            f = current._fields.get(name)
            if f is not None and f.type == "many2one" and f.comodel_name in env:
                todo.append((env[f.comodel_name], ()))
    return False


# Python's search_read, search_count and _read_group never call read(), so a
# read() override (res.users reading its own record under sudo) leaves them on
# the kernel; the ids of a read() travel as a search_read
# Every row a routed read returns is a row Python would have produced through
# `_fetch_query` (`search_fetch`, `read` -> `fetch`), and every grouped cell
# through the `_read_group_*` hooks that compose and post-process the SQL. A
# model that overrides one of them answers differently from its columns --
# `calendar.event._fetch_query` masks a private event the caller does not
# attend, `mail.message.fetch` decides a portal user's access before reading as
# sudo, `stock.quant._read_group_select` turns an aggregate into NULL under a
# context key -- and the gate must see that, or the kernel serves the column.
_FETCH_HOOKS = ("_fetch_query",)
_READ_GROUP_HOOKS = (
    "_read_group_select",
    "_read_group_groupby",
    "_read_group_orderby",
    "_read_group_having",
    "_read_group_postprocess_groupby",
    "_read_group_postprocess_aggregate",
    "_read_group_empty_value",
)
_READ_PATH_NEEDS = {
    "read": ("read", "_check_access", "fetch", *_FETCH_HOOKS),
    "search_read": ("_search", "search_read", "search_fetch", *_FETCH_HOOKS),
    "web_search_read": (
        "_search",
        "search_read",
        "search_fetch",
        "search_count",
        *_FETCH_HOOKS,
    ),
    "search_count": ("_search", "search_count"),
    "_read_group": ("_search", "_read_group", *_READ_GROUP_HOOKS),
    "name_search": ("_search", *_FETCH_HOOKS),
}


def _clean_model(model, method=None):
    needs = _READ_PATH_NEEDS.get(method) or (
        "_search",
        "read",
        "search_read",
        "_read_group",
        "search_count",
    )
    cls = type(model.sudo())
    methods = tuple(getattr(cls, name) for name in needs)
    key = _cache_key(model, "rp:" + (method or "*"), tuple(map(id, methods)))
    cached = _GATE_CACHE.get(key)
    if cached is not None:
        return cached
    ok = all(m is _BASE_METHODS[name] for m, name in zip(methods, needs, strict=True))
    _GATE_CACHE[key] = ok
    return ok


def _renders_column(model, name):
    f = model._fields.get(name)
    if f is None:
        return False
    if f.store:
        return True
    # a delegated column under _inherits reads through the parent's row
    head, _dot, tail = (f.related or "").partition(".")
    hf = model._fields.get(head) if tail and "." not in tail else None
    if hf is None or hf.type != "many2one" or not hf.store:
        return False
    tf = model.env[hf.comodel_name]._fields.get(tail)
    return tf is not None and bool(tf.store)


def _display_clean(model):
    key = _cache_key(model, "dn")
    cached = _GATE_CACHE.get(key)
    if cached is not None:
        return cached
    cls = type(model.sudo())
    ok = cls._compute_display_name is _BASE_METHODS["_compute_display_name"]
    if ok:
        f = model._fields.get("display_name")
        compute = getattr(f, "compute", None) if f is not None else None
        ok = compute in (None, "_compute_display_name")
    if ok and model._rec_name:
        f = model._fields.get(model._rec_name)
        ok = f is not None and (bool(f.store) or bool(f.related))
    if not ok and getattr(cls, "_display_name_column", None):
        cols = cls._display_name_column
        cols = cols if isinstance(cols, tuple) else (cols,)
        ok = all(_renders_column(model, c) for c in cols)
    _GATE_CACHE[key] = ok
    return ok


def _display_ok(model) -> bool:
    if not _display_clean(model):
        return False
    keys = getattr(type(model), "_display_name_context_keys", ())
    ctx = model.env.context
    return not any(k in ctx for k in keys)


GATE_REASONS = collections.Counter()
_GATE_TL = threading.local()


def _refuse(reason) -> bool:
    _GATE_TL.reason = reason
    return False


def _gated(model, method) -> None:
    STATS["fallback_gate"] += 1
    reason = getattr(_GATE_TL, "reason", None) or "call shape"
    _GATE_TL.reason = None
    GATE_REASONS[(model._name, method, reason)] += 1
    _gate_logger.debug("%s.%s not routed: %s", model._name, method, reason)


def _kernel_labels(comodel) -> bool:
    if not _display_ok(comodel):
        return False
    key = _cache_key(comodel, "ca")
    pure = _GATE_CACHE.get(key)
    if pure is None:
        pure = type(comodel.sudo())._check_access is _BASE_METHODS["_check_access"]
        _GATE_CACHE[key] = pure
    return pure


def _python_labelled(model, fields):
    out = []
    for fname in fields:
        f = model._fields.get(fname)
        if f is not None and f.type == "many2one":
            if not _kernel_labels(model.env[f.comodel_name]):
                out.append(fname)
    return out


def _label_in_python(model, records, names):
    if not names or not records:
        return records
    owners = model.browse([rec["id"] for rec in records])
    for name in names:
        field = model._fields[name]
        comodel = model.env[field.comodel_name]
        targets = [rec[name] for rec in records]
        prefetch = [t for t in targets if t]
        values = [
            comodel.browse(t).with_prefetch(prefetch) if t else comodel.browse()
            for t in targets
        ]
        labels = field.convert_to_read_multi(values, owners)
        for rec, label in zip(records, labels, strict=True):
            rec[name] = label
    return records


def _rules_read_through_x2many(model, fields):
    if model.env.su:
        return None
    x2many = {
        name
        for name in fields or ()
        if getattr(model._fields.get(name), "type", None) in ("one2many", "many2many")
    }
    if not x2many:
        return None
    domain = model.env["ir.rule"]._get_domain_accessible_records(model._name, "read")
    return next(
        (
            head
            for condition in domain.iter_conditions()
            if (head := condition.field_expr.split(".", 1)[0]) in x2many
        ),
        None,
    )


def _gate(model, fields=None, order=None, domain=None, method=None):  # noqa: ARG001  order and domain are the routed call shape, kept for the reasons log
    if not _policy_allows(model._name):
        # _policy_allows has already recorded which policy it was
        return False
    if not _bound_db(model.env):
        return _refuse("another database")
    if not _ensure_kernel(model.env):
        return _refuse("kernel not built")
    try:
        if model.env.cr in DIRTY_CRS:
            return _refuse("cursor wrote a security model")
    except TypeError:
        return _refuse("unhashable cursor")
    if not _clean_model(model, method):
        return _refuse("read path overridden in python")
    if method == "read" and (through := _rules_read_through_x2many(model, fields)):
        return _refuse(
            f"{through}: the record rules read it under sudo before the fetch, "
            "so python answers from that unfiltered cache"
        )
    if ORDERS and _order_drifted(
        model, order, fields if method == "_read_group" else ()
    ):
        return _refuse("an _order changed since the kernel was built")
    ctx = model.env.context
    if ctx.get("prefetch_langs") or ctx.get("edit_translations"):
        return _refuse("translation context")
    for fname in fields or ():
        if fname == "display_name" and not _display_ok(model):
            return _refuse("display_name computed in python")
        f = model._fields.get(fname)
        if f is None:
            return _refuse(f"unknown field {fname}")
        if f.type in ("one2many", "many2many") and _x2many_cached(model, f):
            return _refuse(f"{fname} written in this transaction")
    return True


WRITTEN_X2MANY = weakref.WeakKeyDictionary()


def _written_x2many(model, vals_list):
    out = set()
    fields = model._fields
    inverses = getattr(getattr(model, "pool", None), "field_inverses", None)
    for vals in vals_list:
        # a create() may receive shapes the ORM normalises later; only a dict
        # of field names says which x2many fields this transaction touched
        if not isinstance(vals, dict):
            continue
        for name in vals:
            if not isinstance(name, str):
                continue
            f = fields.get(name)
            if f is None:
                continue
            if f.type in ("one2many", "many2many"):
                out.add((model._name, name))
            if f.type in ("many2one", "many2many") and inverses is not None:
                out.update(
                    (inv.model_name, inv.name)
                    for inv in (inverses.get(f, ()) if hasattr(inverses, "get") else ())
                )
    return out


def _unhashable_cursor(cr, what) -> None:
    # These three bookkeepers are the SAFETY NET: they record that this
    # transaction wrote something the kernel would otherwise read staleley, and
    # the gate refuses on what they recorded. A cursor that cannot go in a
    # WeakSet silently records nothing, and the gate then sees a clean
    # transaction -- so the failure is louder than the thing it guards.
    _logger.warning(
        "cursor %r is unhashable, so %s was not recorded; the gate cannot "
        "refuse on it and a routed read may answer from before the write",
        type(cr).__name__,
        what,
    )


def _note_written(model, vals_list) -> None:
    pairs = _written_x2many(model, vals_list)
    if not pairs:
        return
    try:
        WRITTEN_X2MANY.setdefault(model.env.cr, set()).update(pairs)
    except TypeError:
        _unhashable_cursor(model.env.cr, "an x2many write on %s" % model._name)


def _x2many_cached(model, field):
    try:
        written = WRITTEN_X2MANY.get(model.env.cr)
    except TypeError:
        return True
    if not written or (model._name, field.name) not in written:
        return False
    try:
        return bool(model.env.cache.get_records(model, field, all_contexts=True))
    except Exception:
        return True


def _needs_flush(env):
    try:
        tx = env.transaction
        return tx._cache_store.is_any_dirty() or bool(tx._compute_engine.pending)
    except Exception:
        return True


def _label_fields(model):
    names = set()
    if model._rec_name:
        names.add(model._rec_name)
    names.update(
        name.split(".", 1)[0]
        for name in getattr(model, "_rec_names_search", None) or ()
    )
    return {n for n in names if n in model._fields}


def _label_dependencies(model, fields, deps):
    env = model.env
    for fname in fields or ():
        if fname == "display_name":
            deps.setdefault(model._name, set()).update(_label_fields(model))
            continue
        f = model._fields.get(fname)
        if f is not None and f.type == "many2one":
            comodel = env[f.comodel_name]
            deps.setdefault(comodel._name, set()).update(_label_fields(comodel))
    return deps


def _read_dependencies(model, domain, order, fields):
    from odoo.fields import Domain
    from odoo.orm.runtime._search_flush import _DependencyCollector

    env = model.env
    dom = Domain(domain or [])
    if not env.su:
        dom &= env["ir.rule"]._get_domain_accessible_records(model._name, "read")
    collector = _DependencyCollector()
    collector.collect_domain(model, dom)
    collector.collect_order(model, order or model._order)
    own = collector.fields_by_model[model._name]
    if model._active_name:
        own.add(model._active_name)
    for fname in fields or ():
        f = model._fields.get(fname)
        if f is None or fname == "display_name":
            continue
        collector.collect_field(model, fname)
        if f.type in ("one2many", "many2many"):
            comodel = env[f.comodel_name]
            if comodel._active_name:
                collector.fields_by_model[comodel._name].add(comodel._active_name)
            collector.collect_order(comodel, comodel._order)
    return _label_dependencies(model, fields, collector.fields_by_model)


def _flush_if_needed(env, model, domain=None, order=None, fields=None) -> bool | None:
    if not _needs_flush(env):
        return True
    # The kernel reads the database, so anything this transaction has computed
    # but not written has to land first. Flushing more than the read needs is
    # correct but costs; `deps` is the narrow set, and falling back to
    # `flush_all` is the wide one.
    started = time.monotonic()
    try:
        try:
            deps = _read_dependencies(model, domain, order, fields)
        except Exception as e:
            _logger.debug("flushing everything: %s", e)
            env.flush_all()
            _call_logger.debug(
                "%s: flushed everything in %.1f ms before routing",
                model._name,
                (time.monotonic() - started) * 1000,
            )
            return True
        for mname, fnames in deps.items():
            env[mname].flush_model(fnames)
        _call_logger.debug(
            "%s: flushed %d model(s) in %.1f ms before routing",
            model._name,
            len(deps),
            (time.monotonic() - started) * 1000,
        )
        return True
    except Exception as e:
        STATS["fallback_flush"] += 1
        _logger.info("not routing: the flush the kernel needs raised %s", e)
        return False


def _rust_conn(env):
    return env.cr._cnx._rust


def _default_name_fields(model):
    rns = getattr(model, "_rec_names_search", None)
    if callable(rns):
        rns = None
    fnames = list(rns or ([model._rec_name] if model._rec_name else []))
    return [
        f
        for f in fnames
        if f != "display_name" and not model._is_rec_names_search_cyclic(f)
    ]


def _resolve_display_name_exact(model, domain):
    # _search_display_name on a model declaring _display_name_search_exact:
    # an "in"/"ilike" with a value first searches those fields exactly, and
    # any hit is the whole answer; a miss is the default composition. Both
    # halves are plain domains the kernel compiles, so the leaf is rewritten
    # here and the kernel refuses one that was not
    exact = getattr(type(model), "_display_name_search_exact", ())
    if not exact:
        return domain

    def any_of(leaves):
        return ["|"] * (len(leaves) - 1) + leaves

    out = []
    for item in domain:
        if not (
            isinstance(item, (list, tuple))
            and len(item) == 3
            and item[0] == "display_name"
            and item[1] in ("in", "ilike")
            and item[2]
        ):
            out.append(item)
            continue
        value = item[2]
        values = [value] if isinstance(value, str) else list(value)
        if not all(isinstance(v, str) and v for v in values):
            out.append(item)
            continue
        hits = model.search(any_of([(f, "in", values) for f in exact]))
        if hits:
            out.append(("id", "in", hits.ids))
            continue
        terms = []
        for f in _default_name_fields(model):
            fl = model._fields.get(f)
            name = f"{f}.display_name" if fl is not None and fl.relational else f
            terms.append((name, item[1], value))
        out.extend(any_of(terms) if terms else [item])
    return out


def _needs_python_search(model, domain, depth=0):
    if depth > 4 or not isinstance(domain, (list, tuple)):
        return False
    for item in domain:
        if not isinstance(item, (list, tuple)) or len(item) != 3:
            continue
        path, operator, value = item
        if not isinstance(path, str):
            continue
        current = model
        names = path.split(".")
        for i, name in enumerate(names):
            field = getattr(current, "_fields", {}).get(name)
            if field is None:
                return False
            if (
                not field.store
                and field.search
                and not field.related
                and name != "display_name"
            ):
                return True
            if not field.relational:
                break
            current = model.env[field.comodel_name]
            last = i == len(names) - 1
            if last and operator in ("any", "not any", "any!", "not any!"):
                if _needs_python_search(current, value, depth + 1):
                    return True
    return False


def _domain_needs_python_search(model, domain, depth=0):
    from odoo.fields import Domain

    for condition in domain.iter_conditions():
        current, field = model, None
        for name in condition.field_expr.split("."):
            field = current._fields.get(name)
            if field is None:
                return False
            if (
                not field.store
                and field.search
                and not field.related
                and name != "display_name"
            ):
                return True
            if not field.relational:
                break
            current = model.env[field.comodel_name]
        if (
            depth < 4
            and field is not None
            and field.relational
            and isinstance(condition.value, Domain)
            and _domain_needs_python_search(current, condition.value, depth + 1)
        ):
            return True
    return False


_RULES_NEED_PYTHON = {}


def _rules_key(env, name):
    lru = env.registry.ormcache_lrus.get("default")
    return (
        id(env.registry),
        lru.generation if lru is not None else None,
        env.uid,
        tuple(env.context.get("allowed_company_ids") or ()),
        name,
    )


def _rules_need_python(env, name):
    key = _rules_key(env, name)
    needed = _RULES_NEED_PYTHON.get(key)
    if needed is None:
        domain = env["ir.rule"]._get_domain_accessible_records(name, "read")
        needed = not domain.is_true() and _domain_needs_python_search(env[name], domain)
        if len(_RULES_NEED_PYTHON) >= 4096:
            _RULES_NEED_PYTHON.clear()
        _RULES_NEED_PYTHON[key] = needed
    return needed


def _fragment_param_ok(param) -> bool:
    if isinstance(param, (bool, int, float, str)):
        return True
    if isinstance(param, (list, tuple)) and param:
        return all(
            isinstance(item, int) and not isinstance(item, bool) for item in param
        ) or all(isinstance(item, str) for item in param)
    return False


def _wire_encoder(nonce):
    from odoo.tools import SQL, Query

    def encode(value):
        if isinstance(value, Query):
            value = value.subselect()
        if isinstance(value, SQL):
            params = list(value.params)
            for param in params:
                if not _fragment_param_ok(param):
                    raise KernelRefused(
                        "a SQL comparand binds a %s the kernel cannot type"
                        % type(param).__name__
                    )
            return {"$sql": value.code, "$params": params, "$nonce": nonce}
        if isinstance(value, datetime.datetime):
            if value.tzinfo is not None:
                value = value.astimezone(datetime.UTC).replace(tzinfo=None)
            return value.isoformat(" ")
        if isinstance(value, datetime.date):
            return value.isoformat()
        raise KernelRefused(
            "domain carries a %s the kernel cannot receive" % type(value).__name__
        )

    return encode


_ORDER_WORDS = {("asc",), ("desc",), ("nulls", "first"), ("nulls", "last")}


def _order_fragments(model, order):
    from odoo.tools import SQL, Query

    terms, fragments = [], []
    for part in order.split(","):
        words = part.split()
        if not words:
            continue
        name, rest = words[0], [w.lower() for w in words[1:]]
        if rest[:1] in (["asc"], ["desc"]):
            tail = rest[1:]
        else:
            tail = rest
        if tail and tuple(tail) not in _ORDER_WORDS:
            return order, []
        field = model._fields.get(name)
        if field is None or field.store or field.related:
            terms.append(part.strip())
            continue
        query = Query(model.env, model._table, model._table_sql)
        try:
            expr = model._order_field_to_sql(model._table, name, SQL(), SQL(), query)
        except ValueError:
            return order, []
        if query._joins or not expr:
            return order, []
        terms.append(" ".join(["$%d" % len(fragments), *words[1:]]))
        fragments.append(expr)
    return ", ".join(terms), fragments


def _resolved_rules(model, kw, encode):
    env = model.env
    if env.su:
        return {}
    names = {model._name}
    for spec in [*(kw.get("fields") or ()), *(kw.get("groupby") or ())]:
        field = (
            model._fields.get(spec.split(":")[0].split(".")[0])
            if isinstance(spec, str)
            else None
        )
        if field is not None and field.relational:
            names.add(field.comodel_name)
    resolved = {}
    for name in sorted(names):
        try:
            if not _rules_need_python(env, name):
                continue
            domain = env["ir.rule"]._get_domain_accessible_records(name, "read")
        except Exception as exc:
            raise KernelRefused(
                "computing the record rules on %s raised %s"
                % (name, type(exc).__name__)
            ) from exc
        try:
            rules = list(
                domain.optimize_full(env[name].sudo().with_context(active_test=False))
            )
        except Exception as exc:
            raise KernelRefused(
                "resolving the record rules on %s raised %s"
                % (name, type(exc).__name__)
            ) from exc
        try:
            json.dumps(rules, default=encode)
        except (TypeError, ValueError, KernelRefused) as exc:
            _logger.debug("record rules on %s do not resolve to data: %s", name, exc)
            _RULES_NEED_PYTHON[_rules_key(env, name)] = False
            continue
        resolved[name] = rules
    return resolved


def _sql_tz(env):
    tz = env.context.get("tz") or None
    if tz is None:
        return None
    from odoo.orm.fields.temporal import _resolve_sql_timezone_name

    resolved = _resolve_sql_timezone_name(env, tz)
    if resolved is None:
        raise KernelRefused("timezone %r is unknown to the database" % tz)
    return resolved


def _request(model, method, **kw):
    env = model.env
    req = {
        "model": model._name,
        "registry_sequence": env.registry.registry_sequence,
        "method": method,
        "uid": env.uid,
        "su": bool(env.su),
        # `env._lang`, not `context['lang']`: the two agree except under
        # `edit_translations` / `check_translations`, where Odoo reads the
        # translated columns through the `_xx_XX` pseudo-language. The kernel
        # validates the code against `res_lang` and refuses one it does not
        # have, which is exactly the fallback that mode needs; the context
        # value would have been served as the plain language.
        "lang": env._lang,
        "allowed_company_ids": env.context.get("allowed_company_ids") or None,
        "active_test": bool(env.context.get("active_test", True)),
        "tz": _sql_tz(env),
    }
    if kw.get("domain") is not None:
        if not isinstance(kw["domain"], list):
            from odoo.fields import Domain

            kw["domain"] = list(Domain(kw["domain"]))
        kw["domain"] = _resolve_display_name_exact(model, kw["domain"])
        if _needs_python_search(model, kw["domain"]):
            from odoo.fields import Domain

            try:
                env.flush_all()
                kw["domain"] = list(Domain(kw["domain"]).optimize_full(model))
            except Exception as exc:
                raise KernelRefused(
                    "resolving a Python search method raised %s" % type(exc).__name__
                ) from exc
            kw["trusted_domain"] = True
    if method == "search_read" and kw.get("order"):
        kw["order"], fragments = _order_fragments(model, kw["order"])
        if fragments:
            kw["order_fragments"] = fragments
    nonce = secrets.token_hex(16)
    encode = _wire_encoder(nonce)
    if resolved := _resolved_rules(model, kw, encode):
        kw["resolved_rules"] = resolved
    req.update(kw)
    req["sql_nonce"] = nonce
    return json.dumps(req, default=encode)


def _dispatch(model, method, **kw):
    # RustKernel.dispatch reads inside a savepoint of its own, so a refusal
    # leaves the caller's transaction usable for Python to answer the call.
    request = _request(model, method, **kw)
    started = time.monotonic()
    raw = KERNEL.dispatch(_rust_conn(model.env), request)
    kernel_ms = (time.monotonic() - started) * 1000
    _call_logger.debug(
        "%s.%s routed: %d request bytes, %d answer bytes, %.2f ms in the kernel",
        model._name,
        method,
        len(request),
        len(raw),
        kernel_ms,
    )
    return json.loads(raw)


# fromisoformat, not strptime: strptime re-normalises the locale on every call
# and cost 200 ms of a 5000-row read; the kernel writes ISO text with a space
_parse_dt = datetime.datetime.fromisoformat
_parse_date = datetime.date.fromisoformat


COUNT_AGGREGATES = frozenset({"count", "count_distinct"})


def _revive_temporal(field, value):
    # the kernel serialises temporal columns as text; a numeric granularity
    # (day_of_week, week_number, ...) and a count reach here as numbers
    if field is None:
        return value
    if isinstance(value, list):
        return [_revive_temporal(field, x) for x in value]
    if not isinstance(value, str):
        return value
    if field.type == "datetime":
        return _parse_dt(value)
    if field.type == "date":
        return _parse_date(value)
    return value


def _python_key_order(model, records):
    # read() answers the scalar columns first and the relational fields after
    # them (_read_format), whatever order the caller asked; a routed answer
    # keeps the same key order so the two serialise byte for byte
    if not records:
        return records
    from odoo.orm.models.mixins._cache_scan import can_scan_read

    fields = model._fields
    names = [n for n in records[0] if n != "id"]
    scalars = [
        n for n in names if (f := fields.get(n)) is not None and can_scan_read(f)
    ]
    if scalars == names[: len(scalars)]:
        return records
    order = ["id", *scalars, *[n for n in names if n not in scalars]]
    return [{k: r[k] for k in order if k in r} for r in records]


def _revive_records(model, records):
    fields = model._fields
    for rec in records:
        for name, val in rec.items():
            if val is False or name == "id":
                continue
            f = fields.get(name)
            if f is None:
                continue
            if f.type == "datetime" and isinstance(val, str):
                rec[name] = _parse_dt(val)
            elif f.type == "date" and isinstance(val, str):
                rec[name] = _parse_date(val)
            elif f.type == "many2one" and isinstance(val, list):
                rec[name] = (val[0], val[1])
    return _python_key_order(model, records)


def _warm_cache(model, records) -> None:
    if not records:
        return
    # A routed read bypasses the ORM cache, so the records it answered with are
    # written back into it; skipping this makes the NEXT access re-query, which
    # is how a routed read can look fast and still lose overall.
    started = time.monotonic()
    try:
        recs = model.browse([r["id"] for r in records])
        # one recordset walk for every field, not one per field
        singles = list(recs)
        cache = model.env.cache
        for name in records[0]:
            if name in ("id", "display_name"):
                continue
            f = model._fields.get(name)
            if f is None or f.type in ("one2many", "many2many"):
                continue
            convert = f.convert_to_cache
            if f.type == "many2one":
                values = []
                ids = []
                for r, rec in zip(records, singles, strict=False):
                    v = r.get(name, False)
                    if isinstance(v, (list, tuple)):
                        v = v[0]
                    elif isinstance(v, dict):
                        v = v.get("id", False)
                    # False is also the public redaction of an unreadable
                    # target. It cannot replace the internal foreign key.
                    if not v:
                        continue
                    ids.append(rec.id)
                    values.append(convert(v, rec, validate=False))
                cache.update(model.browse(ids), f, values)
            else:
                values = [
                    convert(r.get(name, False), rec, validate=False)
                    for r, rec in zip(records, singles, strict=False)
                ]
                cache.update(recs, f, values)
        _call_logger.debug(
            "%s: warmed %d record(s) into the ORM cache in %.1f ms",
            model._name,
            len(records),
            (time.monotonic() - started) * 1000,
        )
    except Exception as e:
        _logger.debug("not warming the cache after a routed read: %s", e)


def _record_error(model, e) -> None:
    unexpected = not isinstance(e, KernelRefused)
    if isinstance(e, KernelRegistryStale):
        STATS["registry_stale"] = STATS.get("registry_stale", 0) + 1
    msg = "%s: %s" % (type(e).__name__, e) if unexpected else str(e)
    STATS["fallback_error"] += 1
    if not unexpected:
        # the call reached the kernel and it declined: a refusal, which a
        # stage reads apart from a shim or kernel exception
        STATS["kernel_refused"] = STATS.get("kernel_refused", 0) + 1
    if unexpected:
        STATS["errors_by_model"][model._name] = (
            STATS["errors_by_model"].get(model._name, 0) + 1
        )
    first = (
        STATS["errors_by_model"][model._name] == 1
        if unexpected
        else model._name not in STATS["errors"]
    )
    if first:
        STATS["errors"][model._name] = msg[:160]
    if unexpected:
        _logger.log(
            logging.WARNING if first else logging.DEBUG,
            "the rust routing path raised for %s; answering from python. "
            "Database or internal failure: %s",
            model._name,
            msg[:200],
            exc_info=first,
        )
    else:
        _logger.log(
            logging.INFO if first else logging.DEBUG,
            "falling back to Python for %s: %s",
            model._name,
            msg[:200],
        )


_WEB_METHODS = {}
_WEB_HOOKS = (
    "web_read",
    "_web_read",
    "_format_web_search_read_results",
    "_screen_fields_spec",
    "_web_read_resolve_many2one",
    "_web_read_resolve_x2many",
)


def _web_clean(model):
    key = _cache_key(model, "web")
    cached = _GATE_CACHE.get(key)
    if cached is not None:
        return cached
    cls = type(model.sudo())
    ok = bool(_WEB_METHODS) and all(
        getattr(cls, name, None) is _WEB_METHODS[name] for name in _WEB_HOOKS
    )
    _GATE_CACHE[key] = ok
    return ok


def _no_plan(reason) -> None:
    _refuse(reason)


def _related_reads_columns(model, f) -> bool:
    current = model
    for segment in f.related.split("."):
        link = current._fields.get(segment)
        if link is None:
            return False
        if not link.store and not (
            link.related and _related_reads_columns(current, link)
        ):
            return False
        if link.comodel_name:
            current = model.env[link.comodel_name]
    return True


def _web_field_in_python(model, f, name, spec):
    if f.type in READ_SKIP_TYPES:
        return f"{f.type} is read in python"
    if name != "display_name" and not (f.store or f.related):
        return "computed and not stored"
    if f.related and not f.store and not _related_reads_columns(model, f):
        return "related through a field computed in python"
    if f.type == "many2one":
        if "context" in spec:
            return "specification carries a context"
        sub = spec.get("fields")
        if sub is not None and not (
            isinstance(sub, dict) and set(sub) == {"display_name"}
        ):
            return "sub-fields other than display_name"
    elif f.type in ("one2many", "many2many"):
        if spec:
            return "x2many with a sub-specification"
        if callable(f.domain):
            return "x2many whose domain is computed per record"
        comodel = type(model.env[f.comodel_name].sudo())
        if comodel._search is not _BASE_METHODS["_search"]:
            return "x2many through a comodel that searches in python"
    return None


def _web_spec_plan(model, specification):
    fields, many2ones, python_spec = [], [], {}
    for name, spec in specification.items():
        f = model._fields.get(name)
        if f is None:
            return _no_plan(f"unknown field {name}")
        if not isinstance(spec or {}, dict):
            return _no_plan(f"{name}: specification is not a dict")
        if name == "display_name" and not _display_ok(model):
            reason = "display_name computed in python"
        else:
            reason = _web_field_in_python(model, f, name, spec or {})
        if reason:
            _call_logger.debug(
                "%s.%s: %s; web_read answers it", model._name, name, reason
            )
            python_spec[name] = spec
            continue
        if f.type == "many2one":
            many2ones.append(name)
        fields.append(name)
    return fields or ["id"], many2ones, python_spec


def _web_merge(specification, records, python_records):
    if not python_records:
        return records
    by_id = {rec["id"]: rec for rec in python_records}
    merged = []
    for rec in records:
        extra = by_id[rec["id"]]
        merged.append(
            {
                "id": rec["id"],
                **{
                    name: rec[name] if name in rec else extra[name]
                    for name in specification
                    if name != "id"
                },
            }
        )
    return merged


def _web_split_many2ones(model, specification, many2ones):
    # A plain many2one is web_read's raw foreign key and needs no label. A
    # named one keeps the kernel's label when the user may see the target;
    # a target the label query hid comes back as its bare id for web's own
    # resolver, and a comodel Python names or guards goes there whole.
    raw, unredacted = [], []
    for name in many2ones:
        named = "fields" in (specification[name] or {})
        comodel = model.env[model._fields[name].comodel_name]
        (unredacted if named and _kernel_labels(comodel) else raw).append(name)
    return raw, unredacted


def _web_resolve_many2ones(model, records, specification, raw, unredacted):
    # web_read decides a many2one's value with rules of its own: an unreadable
    # target is still its id, or {"id": id} when a name was asked for, where
    # read() redacts it to False.
    for name in raw:
        spec = specification[name] or {}
        if "fields" in spec:
            model._web_read_resolve_many2one(records, model._fields[name], name, spec)
    for name in unredacted:
        hidden = []
        for rec in records:
            val = rec[name]
            if isinstance(val, tuple):
                rec[name] = {"id": val[0], "display_name": val[1]}
            elif val:
                hidden.append(rec)
        if hidden:
            model._web_read_resolve_many2one(
                hidden, model._fields[name], name, specification[name]
            )
    return records


def _web_length(n_records, offset, limit, count_limit, force_count, count):
    if not n_records:
        return 0 if not offset else count(count_limit)
    current_length = n_records + offset
    limit_reached = n_records == limit
    count_limit_reached = bool(count_limit) and count_limit <= current_length
    if limit and ((limit_reached and not count_limit_reached) or force_count):
        return count(count_limit)
    return current_length


STAMPS = ("_api_model", "_api_private", "_readonly")


READ_SKIP_TYPES = frozenset(
    {"binary", "properties", "properties_definition", "reference", "many2one_reference"}
)
AGGREGATE_FUNCS = frozenset(
    {
        "sum",
        "avg",
        "min",
        "max",
        "count",
        "count_distinct",
        "bool_and",
        "bool_or",
        "array_agg",
        "array_agg_distinct",
    }
)


def _aggregates_ok(aggregates):
    return all(
        a == "__count" or a.rsplit(":", 1)[-1] in AGGREGATE_FUNCS for a in aggregates
    )


def _read_fields_ok(model, fields):
    for name in fields:
        f = model._fields.get(name)
        if f is None:
            return _refuse(f"unknown field {name}")
        if f.type in READ_SKIP_TYPES:
            return _refuse(f"{name} is {f.type}")
        if name == "display_name":
            continue
        if not (f.store or f.related):
            return _refuse(f"{name} is computed and not stored")
    return True


def _read_reorder(model, ids, records, load, fields):
    by_id = {rec["id"]: rec for rec in records}
    absent = sorted({i for i in ids if i not in by_id})
    if absent:
        reads_a_column = any(
            f is not None and f.store and f.column_type
            for f in map(model._fields.get, fields)
        )
        if not reads_a_column or model.browse(absent).exists():
            return None
    out = []
    for i in ids:
        if i not in by_id:
            continue
        rec = dict(by_id[i])
        if load != "_classic_read":
            for name, val in rec.items():
                if isinstance(val, tuple):
                    rec[name] = val[0]
        out.append(rec)
    return out


def _name_search_clean(model):
    key = _cache_key(model, "ns")
    reason = _GATE_CACHE.get(key)
    if reason is None:
        cls = type(model.sudo())
        if cls.name_search is not _BASE_METHODS["name_search"]:
            reason = "name_search overridden in python"
        elif cls._search_display_name is not _BASE_METHODS["_search_display_name"]:
            reason = "_search_display_name overridden in python"
        elif not _display_ok(model):
            reason = "display_name computed in python"
        else:
            reason = ""
        _GATE_CACHE[key] = reason
    return not reason or _refuse(reason)


def _restamp(new, orig):
    for name, value in vars(orig).items():
        if name in STAMPS or name.startswith("_api"):
            setattr(new, name, value)
    return new


_INSTALLED = None


def _untaint(cr) -> None:
    # a commit or rollback ends the window the two bookkeepers guard; failing
    # to clear is harmless (it only keeps the gate refusing), so this one stays
    # quiet
    DIRTY_CRS.discard(cr)
    with contextlib.suppress(TypeError):
        WRITTEN_X2MANY.pop(cr, None)


def install():
    global DBNAME, _INSTALLED
    if _INSTALLED is not None:
        return _INSTALLED
    if DBNAME is None:
        try:
            import rust_db_shim

            DBNAME = rust_db_shim._dbname(rust_db_shim.CONNINFO)
        except Exception as exc:
            _logger.debug("no database name from the DSN: %s", exc)
        if DBNAME is None:
            _logger.warning(
                "rust_orm_shim: DBNAME is unset and could not be derived; "
                "the kernel will be offered cursors from every database in "
                "this process and decline them one error at a time"
            )

    import odoo.orm.models.base as base_mod

    BaseModel = base_mod.BaseModel
    for name in (
        "_search",
        "read",
        "search_read",
        "_read_group",
        "search_count",
        "_compute_display_name",
        "_search_display_name",
        "name_search",
        "_check_access",
        "search_fetch",
        "fetch",
        *_FETCH_HOOKS,
        *_READ_GROUP_HOOKS,
    ):
        _BASE_METHODS[name] = getattr(BaseModel, name)

    orig_search_read = BaseModel.search_read
    orig_search_count = BaseModel.search_count
    orig_read_group = BaseModel._read_group

    def search_read(
        self, domain=None, fields=None, offset=0, limit=None, order=None, **kw
    ):
        if (
            not kw
            and fields
            and _gate(self, fields, order=order, domain=domain, method="search_read")
        ):
            try:
                if not _flush_if_needed(self.env, self, domain, order, fields):
                    raise KernelRefused("flush failed; not routing")
                python_labelled = _python_labelled(self, fields)
                recs = _dispatch(
                    self,
                    "search_read",
                    domain=domain or [],
                    fields=list(fields),
                    offset=offset or 0,
                    limit=limit,
                    order=order,
                    raw_many2one=python_labelled,
                )
                STATS["kernel"] += 1
                revived = _label_in_python(
                    self, _revive_records(self, recs), python_labelled
                )
                if MODE != "shadow" and not _verify_this_one():
                    _warm_cache(self, revived)
                    return revived
            except Exception as e:
                _record_error(self, e)
            else:
                original = _verified_baseline(
                    self,
                    "search_read",
                    revived,
                    orig_search_read,
                    self,
                    domain=domain,
                    fields=fields,
                    offset=offset,
                    limit=limit,
                    order=order,
                    **kw,
                )
                _shadow(self, "search_read", revived, original)
                return original
        else:
            _gated(self, "search_read")
        return orig_search_read(
            self,
            domain=domain,
            fields=fields,
            offset=offset,
            limit=limit,
            order=order,
            **kw,
        )

    def search_count(self, domain, limit=None):
        if _gate(self, domain=domain, method="search_count"):
            try:
                if not _flush_if_needed(self.env, self, domain, None, ()):
                    raise KernelRefused("flush failed; not routing")
                n = _dispatch(self, "search_count", domain=domain or [], limit=limit)
                STATS["kernel"] += 1
                if MODE != "shadow" and not _verify_this_one():
                    return n
            except Exception as e:
                _record_error(self, e)
            else:
                original = _verified_baseline(
                    self,
                    "search_count",
                    n,
                    orig_search_count,
                    self,
                    domain,
                    limit=limit,
                )
                _shadow(self, "search_count", n, original)
                return original
        else:
            _gated(self, "search_count")
        return orig_search_count(self, domain, limit=limit)

    def _read_group(
        self,
        domain,
        groupby=(),
        aggregates=(),
        having=(),
        offset=0,
        limit=None,
        order=None,
    ):
        bad_aggregate = next(
            (
                a
                for a in aggregates
                if a != "__count" and a.rsplit(":", 1)[-1] not in AGGREGATE_FUNCS
            ),
            None,
        )
        if having:
            supported = _refuse("having")
        elif not groupby:
            supported = _refuse("no groupby")
        elif bad_aggregate is not None:
            supported = _refuse(f"aggregate {bad_aggregate}")
        else:
            supported = _gate(
                self,
                [g.split(":")[0] for g in groupby],
                order=order,
                domain=domain,
                method="_read_group",
            )
        if supported:
            try:
                if not _flush_if_needed(
                    self.env,
                    self,
                    domain,
                    order,
                    [g.split(":")[0] for g in groupby]
                    + [a.rsplit(":", 1)[0] for a in aggregates if a != "__count"],
                ):
                    raise KernelRefused("flush failed; not routing")
                rows = _dispatch(
                    self,
                    "read_group",
                    domain=domain or [],
                    groupby=list(groupby),
                    aggregates=list(aggregates),
                    order=order or None,
                    offset=offset or 0,
                    limit=limit,
                    groupby_labels=False,
                )
                STATS["kernel"] += 1
                out = []
                gb_fields = [self._fields[g.split(":")[0]] for g in groupby]
                # Every group record of a column prefetches with the others,
                # as `_read_group_postprocess_groupby` builds them. Browsed one
                # by one, each was its own prefetch set, and a caller reading a
                # field of the groups -- web_read_group reads their names --
                # fetched once per record per field: on the captured traffic
                # 5,372 single-field fetches and routed web_read_group 3.95x
                # slower than Python.
                prefetch = [
                    tuple(row[i] for row in rows if row[i])
                    if f.type in ("many2one", "many2many")
                    else ()
                    for i, f in enumerate(gb_fields)
                ]
                agg_fields = [
                    None
                    if a == "__count" or a.rsplit(":", 1)[-1] in COUNT_AGGREGATES
                    else self._fields.get(a.rsplit(":", 1)[0])
                    for a in aggregates
                ]
                for row in rows:
                    item = []
                    for i, f in enumerate(gb_fields):
                        v = row[i]
                        if f.type in ("many2one", "many2many"):
                            item.append(
                                self.env[f.comodel_name]
                                .browse(v)
                                .with_prefetch(prefetch[i])
                                if v
                                else self.env[f.comodel_name]
                            )
                        else:
                            item.append(_revive_temporal(f, v))
                    for f, v in zip(agg_fields, row[len(gb_fields) :], strict=False):
                        item.append(_revive_temporal(f, v))
                    out.append(tuple(item))
                if MODE != "shadow" and not _verify_this_one():
                    return out
            except Exception as e:
                _record_error(self, e)
            else:
                original = _verified_baseline(
                    self,
                    "_read_group",
                    out,
                    orig_read_group,
                    self,
                    domain,
                    groupby=groupby,
                    aggregates=aggregates,
                    having=having,
                    offset=offset,
                    limit=limit,
                    order=order,
                )
                _shadow(
                    self,
                    "_read_group",
                    out,
                    original,
                    _group_rows_agree(self, groupby, aggregates),
                )
                return original
        else:
            _gated(self, "_read_group")
        return orig_read_group(
            self,
            domain,
            groupby=groupby,
            aggregates=aggregates,
            having=having,
            offset=offset,
            limit=limit,
            order=order,
        )

    orig_create = BaseModel.create
    orig_write = BaseModel.write
    orig_unlink = BaseModel.unlink

    def _taint(self, vals=None) -> None:
        if _harmless_user_write(self, vals):
            return
        if self._name in SECURITY_MODELS:
            if _gate_logger.isEnabledFor(logging.DEBUG):
                _gate_logger.debug(
                    "cursor tainted by a write to %s (%s)",
                    self._name,
                    ", ".join(sorted(vals))
                    if isinstance(vals, dict)
                    else "create/unlink",
                )
            try:
                DIRTY_CRS.add(self.env.cr)
            except TypeError:
                _unhashable_cursor(self.env.cr, "a write to %s" % self._name)

    def create(self, vals_list):
        _taint(self)
        _note_written(self, vals_list if isinstance(vals_list, list) else [vals_list])
        return orig_create(self, vals_list)

    def write(self, vals):
        _taint(self, vals)
        _note_written(self, [vals])
        return orig_write(self, vals)

    def unlink(self):
        _taint(self)
        return orig_unlink(self)

    BaseModel.create = _restamp(create, orig_create)
    BaseModel.write = _restamp(write, orig_write)
    BaseModel.unlink = _restamp(unlink, orig_unlink)

    import odoo.db.cursor as _cursor_mod

    orig_rollback = _cursor_mod.Cursor.rollback
    orig_commit = _cursor_mod.Cursor.commit

    def rollback(self):
        _untaint(self)
        return orig_rollback(self)

    def commit(self):
        result = orig_commit(self)
        _untaint(self)
        return result

    _cursor_mod.Cursor.rollback = rollback
    _cursor_mod.Cursor.commit = commit

    try:
        from odoo.addons.web.models import web_onchange as _web_onchange_mod
        from odoo.addons.web.models import web_read as _web_read_mod
    except ImportError:
        _web_read_mod = None
    if _web_read_mod is not None:
        WebBase = _web_read_mod.Base
        for name in _WEB_HOOKS:
            for holder in (WebBase, _web_onchange_mod.Base):
                if name in vars(holder):
                    _WEB_METHODS[name] = getattr(holder, name)
                    break
            else:
                raise RuntimeError("web hook %s not found on web's Base classes" % name)
        orig_web_search_read = WebBase.web_search_read
        from odoo.tools.cache_version import _canonical_digest

        def _stamp_envelope(records) -> None:
            try:
                from odoo.http import request
            except ModuleNotFoundError:
                return
            if request:
                request._response_version = _canonical_digest(records)

        def web_search_read(
            self,
            domain,
            specification,
            offset=0,
            limit=None,
            order=None,
            count_limit=None,
        ):
            plan = None
            if not _web_clean(self):
                _refuse("web read hooks overridden in python")
            elif not specification:
                _refuse("empty specification")
            else:
                plan = _web_spec_plan(self, specification)
            if plan and _gate(
                self,
                plan[0],
                order=order,
                domain=domain,
                method="web_search_read",
            ):
                fields, many2ones, python_spec = plan
                raw, unredacted = _web_split_many2ones(self, specification, many2ones)
                try:
                    for name in python_spec:
                        self._check_field_access(self._fields[name], "read")
                    if not _flush_if_needed(self.env, self, domain, order, fields):
                        raise KernelRefused("flush failed; not routing")
                    recs = _dispatch(
                        self,
                        "search_read",
                        domain=domain or [],
                        fields=fields,
                        offset=offset or 0,
                        limit=limit,
                        order=order,
                        x2many_active_test=bool(
                            self.env.context.get("active_test", True)
                        ),
                        raw_many2one=raw,
                        unredacted_many2one=unredacted,
                    )
                    STATS["kernel"] += 1

                    def count(cap):
                        n = _dispatch(
                            self, "search_count", domain=domain or [], limit=cap
                        )
                        STATS["kernel"] += 1
                        return n

                    length = _web_length(
                        len(recs),
                        offset or 0,
                        limit,
                        count_limit,
                        bool(self.env.context.get("force_search_count")),
                        count,
                    )
                    records = _web_resolve_many2ones(
                        self,
                        _revive_records(self, recs),
                        specification,
                        raw,
                        unredacted,
                    )
                    if python_spec and records:
                        records = _web_merge(
                            specification,
                            records,
                            orig_web_read(
                                self.browse([rec["id"] for rec in records]),
                                python_spec,
                            ),
                        )
                    result = {"length": length, "records": records}
                    result["__version"] = _canonical_digest(result)
                    if MODE != "shadow" and not _verify_this_one():
                        _warm_cache(self, result["records"])
                        # web_read stamps the JSON-RPC envelope with a version
                        # of its records; the routed answer carries the same
                        if result["records"]:
                            _stamp_envelope(result["records"])
                        return result
                except Exception as e:
                    _record_error(self, e)
                else:
                    original = _verified_baseline(
                        self,
                        "web_search_read",
                        result,
                        orig_web_search_read,
                        self,
                        domain,
                        specification,
                        offset=offset,
                        limit=limit,
                        order=order,
                        count_limit=count_limit,
                    )
                    _shadow(self, "web_search_read", result, original)
                    return original
            else:
                _gated(self, "web_search_read")
            return orig_web_search_read(
                self,
                domain,
                specification,
                offset=offset,
                limit=limit,
                order=order,
                count_limit=count_limit,
            )

        from odoo import api as _api
        from odoo.tools.cache_version import versioned as _versioned

        WebBase.web_search_read = _api.model(_api.readonly(_versioned(web_search_read)))

        orig_web_read = WebBase.web_read

        def web_read(self, specification):
            ids = list(self._ids)
            plan = None
            if not ids or set(specification) <= {"id"}:
                _refuse("empty recordset or id-only specification")
            elif not all(isinstance(i, int) and i > 0 for i in ids):
                _refuse("unsaved records")
            elif not _web_clean(self):
                _refuse("web read hooks overridden in python")
            else:
                plan = _web_spec_plan(self, specification)
            if plan and _gate(self, plan[0], method="read"):
                fields, many2ones, python_spec = plan
                raw, unredacted = _web_split_many2ones(self, specification, many2ones)
                try:
                    if not _flush_if_needed(
                        self.env, self, [("id", "in", ids)], "id", fields
                    ):
                        raise KernelRefused("flush failed; not routing")
                    recs = _dispatch(
                        self.with_context(active_test=False),
                        "search_read",
                        domain=[("id", "in", sorted(set(ids)))],
                        fields=fields,
                        offset=0,
                        limit=None,
                        order="id",
                        x2many_active_test=bool(
                            self.env.context.get("active_test", True)
                        ),
                        raw_many2one=raw,
                        unredacted_many2one=unredacted,
                    )
                    STATS["kernel"] += 1
                    ordered = _read_reorder(
                        self,
                        ids,
                        _revive_records(self, recs),
                        "_classic_read",
                        specification,
                    )
                    if ordered is None:
                        raise KernelRefused(
                            "web_read of records the kernel did not return"
                        )
                    result = _web_resolve_many2ones(
                        self, ordered, specification, raw, unredacted
                    )
                    if python_spec:
                        result = _web_merge(
                            specification, result, orig_web_read(self, python_spec)
                        )
                    if MODE != "shadow" and not _verify_this_one():
                        _warm_cache(self, result)
                        return result
                except Exception as e:
                    _record_error(self, e)
                else:
                    original = _verified_baseline(
                        self, "web_read", result, orig_web_read, self, specification
                    )
                    _shadow(self, "web_read", result, original)
                    return original
            else:
                _gated(self, "web_read")
            return orig_web_read(self, specification)

        from odoo.tools.cache_version import versioned_envelope as _versioned_envelope

        WebBase.web_read = _api.readonly(_versioned_envelope(web_read))
        _WEB_METHODS["web_read"] = WebBase.web_read

        from odoo.addons.web.models import web_read_group as _wrg_mod
        from odoo.addons.web.models import web_read_group_helpers as _wrg_helpers

        GroupBase = _wrg_mod.Base
        orig_formatted_read_group = GroupBase.formatted_read_group
        group_hooks = {
            name: getattr(holder, name)
            for holder, names in (
                (
                    GroupBase,
                    ("_web_read_group_format",),
                ),
                (
                    _wrg_helpers.Base,
                    (
                        "_web_read_group_get_groupby_formatter",
                        "_web_read_group_get_field_expand",
                        "_web_read_group_expand",
                        "_web_read_group_fill_temporal",
                    ),
                ),
            )
            for name in names
        }

        def _group_hooks_clean(model):
            cls = type(model)
            return getattr(
                cls, "formatted_read_group", None
            ) is GroupBase.formatted_read_group and all(
                getattr(cls, name, None) is method
                for name, method in group_hooks.items()
            )

        def _formatted_group_plan(model, groupby, aggregates, having):
            if having:
                return _refuse("having")
            if not groupby:
                return _refuse("no groupby")
            if not _group_hooks_clean(model):
                return _refuse("read_group formatting hooks overridden in python")
            fill_temporal = model.env.context.get("fill_temporal")
            if fill_temporal or isinstance(fill_temporal, dict):
                return _refuse("fill_temporal")
            for aggregate in aggregates:
                if (
                    aggregate != "__count"
                    and aggregate.rsplit(":", 1)[-1] not in AGGREGATE_FUNCS
                ):
                    return _refuse(f"aggregate {aggregate}")
            fields = []
            for spec in groupby:
                if ":" in spec or "." in spec or spec == "id":
                    return _refuse(f"groupby {spec}")
                field = model._fields.get(spec)
                grouped = (
                    model._fields.get(field.group_by_field)
                    if field is not None and getattr(field, "group_by_field", None)
                    else field
                )
                if grouped is None or grouped.type in (
                    "many2many",
                    "date",
                    "datetime",
                    "properties",
                ):
                    return _refuse(f"groupby {spec}")
                if field.type == "many2one" and not _kernel_labels(
                    model.env[field.comodel_name]
                ):
                    return _refuse(f"groupby {spec}: labels decided in python")
                fields.append(field)
            return fields

        def _format_routed_groups(
            model, groupby, gb_fields, aggregates, groups, labels, expand_field
        ):
            result = [{} for _group in groups]
            extra_domains = [[] for _group in groups]
            columns = list(zip(*groups, strict=True)) if groups else []
            for index, (spec, field) in enumerate(zip(groupby, gb_fields, strict=True)):
                values = columns[index] if columns else ()
                if field.type == "many2one":
                    unlabelled = [
                        value
                        for value in values
                        if value and (field.name, value.id) not in labels
                    ]
                    formatter = (
                        model._web_read_group_get_groupby_formatter(spec, unlabelled)
                        if unlabelled
                        else None
                    )
                    for value, group, domains in zip(
                        values, result, extra_domains, strict=True
                    ):
                        if not value:
                            group[spec] = False
                            domains.append([(spec, "=", False)])
                        elif (field.name, value.id) in labels:
                            group[spec] = (value.id, labels[field.name, value.id])
                            domains.append([(spec, "=", value.id)])
                        else:
                            group[spec], domain = formatter(value)
                            domains.append(domain)
                else:
                    for value, group, domains in zip(
                        values, result, extra_domains, strict=True
                    ):
                        group[spec] = value
                        domains.append([(spec, "=", value)])
                if expand_field is not None and expand_field.relational:
                    comodel = model.env[expand_field.comodel_name]
                    fold_name = comodel._fold_name
                    if fold_name in comodel._fields:
                        for value, group in zip(values, result, strict=True):
                            group["__fold"] = value.sudo()[fold_name]
            for group, domains in zip(result, extra_domains, strict=True):
                group["__extra_domain"] = _wrg_helpers.AND(domains)
            for offset_, spec in enumerate(aggregates, start=len(groupby)):
                for group, values in zip(result, groups, strict=True):
                    group[spec] = values[offset_]
            return result

        def formatted_read_group(
            self,
            domain,
            groupby=(),
            aggregates=(),
            having=(),
            offset=0,
            limit=None,
            order=None,
        ):
            groupby = tuple(groupby)
            aggregates = tuple(
                agg.replace(":recordset", ":array_agg") for agg in aggregates
            )
            if not order:
                order = ", ".join(groupby)
            gb_fields = _formatted_group_plan(self, groupby, aggregates, having)
            if gb_fields and _gate(
                self, list(groupby), order=order, domain=domain, method="_read_group"
            ):
                try:
                    if not _flush_if_needed(
                        self.env,
                        self,
                        domain,
                        order,
                        list(groupby)
                        + [a.rsplit(":", 1)[0] for a in aggregates if a != "__count"],
                    ):
                        raise KernelRefused("flush failed; not routing")
                    rows = _dispatch(
                        self,
                        "read_group",
                        domain=domain or [],
                        groupby=list(groupby),
                        aggregates=list(aggregates),
                        order=order,
                        offset=offset or 0,
                        limit=limit,
                        groupby_labels=True,
                        groupby_hidden_labels_empty=True,
                    )
                    STATS["kernel"] += 1
                    agg_fields = [
                        None
                        if a == "__count" or a.rsplit(":", 1)[-1] in COUNT_AGGREGATES
                        else self._fields.get(a.rsplit(":", 1)[0])
                        for a in aggregates
                    ]
                    labels = {}
                    groups = []
                    for row in rows:
                        item = []
                        for field, value in zip(gb_fields, row, strict=False):
                            if field.type != "many2one":
                                item.append(_revive_temporal(field, value))
                                continue
                            comodel = self.env[field.comodel_name]
                            if value:
                                labels[field.name, value[0]] = value[1]
                                item.append(comodel.browse(value[0]))
                            else:
                                item.append(comodel)
                        item.extend(
                            map(
                                _revive_temporal,
                                agg_fields,
                                row[len(groupby) :],
                                strict=False,
                            )
                        )
                        groups.append(tuple(item))
                    expand_field = self._web_read_group_get_field_expand(groupby)
                    if (
                        expand_field
                        and not offset
                        and (not limit or len(groups) < limit)
                    ):
                        expanded = self._web_read_group_expand(
                            domain, groups, groupby[0], aggregates, order
                        )
                        if not limit or len(expanded) <= limit:
                            groups = expanded
                    result = _format_routed_groups(
                        self,
                        groupby,
                        gb_fields,
                        aggregates,
                        groups,
                        labels,
                        expand_field,
                    )
                    if MODE != "shadow" and not _verify_this_one():
                        return result
                except Exception as e:
                    _record_error(self, e)
                else:
                    original = _verified_baseline(
                        self,
                        "formatted_read_group",
                        result,
                        orig_formatted_read_group,
                        self,
                        domain,
                        groupby,
                        aggregates,
                        having=having,
                        offset=offset,
                        limit=limit,
                        order=order,
                    )
                    _shadow(
                        self,
                        "formatted_read_group",
                        result,
                        original,
                        _formatted_groups_agree(self, aggregates),
                    )
                    return original
            else:
                _gated(self, "formatted_read_group")
            return orig_formatted_read_group(
                self,
                domain,
                groupby,
                aggregates,
                having=having,
                offset=offset,
                limit=limit,
                order=order,
            )

        GroupBase.formatted_read_group = _api.model(_api.readonly(formatted_read_group))

    orig_read = _BASE_METHODS["read"]

    def read(self, fields=None, load="_classic_read"):
        ids = list(self._ids)
        if not fields:
            routable = _refuse("no fields")
        elif not ids:
            routable = _refuse("empty recordset")
        elif not all(isinstance(i, int) and i > 0 for i in ids):
            routable = _refuse("unsaved records")
        elif not _read_fields_ok(self, fields):
            routable = False
        else:
            routable = _gate(self, list(fields), method="read")
        if routable:
            try:
                if not _flush_if_needed(
                    self.env, self, [("id", "in", ids)], "id", fields
                ):
                    raise KernelRefused("flush failed; not routing")
                if load == "_classic_read":
                    labelled = raw = _python_labelled(self, fields)
                else:
                    labelled, raw = (
                        [],
                        [
                            f
                            for f in fields
                            if getattr(self._fields.get(f), "type", None) == "many2one"
                        ],
                    )
                recs = _dispatch(
                    self.with_context(active_test=False),
                    "search_read",
                    domain=[("id", "in", sorted(set(ids)))],
                    fields=list(fields),
                    offset=0,
                    limit=None,
                    order="id",
                    x2many_active_test=bool(self.env.context.get("active_test", True)),
                    raw_many2one=raw,
                )
                STATS["kernel"] += 1
                revived = _label_in_python(self, _revive_records(self, recs), labelled)
                ordered = _read_reorder(self, ids, revived, load, fields)
                if ordered is None:
                    raise KernelRefused("read order not reproducible")
                if MODE != "shadow" and not _verify_this_one():
                    _warm_cache(self, ordered)
                    return ordered
            except Exception as e:
                _record_error(self, e)
            else:
                original = _verified_baseline(
                    self, "read", ordered, orig_read, self, fields, load=load
                )
                _shadow(self, "read", ordered, original)
                return original
        else:
            _gated(self, "read")
        return orig_read(self, fields, load=load)

    BaseModel.read = _restamp(read, orig_read)
    _BASE_METHODS["read"] = BaseModel.read

    orig_name_search = _BASE_METHODS["name_search"]

    def name_search(self, name="", domain=None, operator="ilike", limit=100):
        if _name_search_clean(self) and _gate(
            self, ["display_name"], domain=domain, method="name_search"
        ):
            try:
                if not _flush_if_needed(self.env, self, domain, None, ["display_name"]):
                    raise KernelRefused("flush failed; not routing")
                full = [("display_name", operator, name)]
                if domain:
                    from odoo.fields import Domain

                    full += list(Domain(domain))
                recs = _dispatch(
                    self,
                    "search_read",
                    domain=full,
                    fields=["display_name"],
                    offset=0,
                    limit=limit,
                    order=None,
                )
                STATS["kernel"] += 1
                pairs = [(r["id"], r["display_name"] or "") for r in recs]
                if MODE != "shadow" and not _verify_this_one():
                    return pairs
            except Exception as e:
                _record_error(self, e)
            else:
                original = _verified_baseline(
                    self,
                    "name_search",
                    pairs,
                    orig_name_search,
                    self,
                    name,
                    domain,
                    operator,
                    limit,
                )
                _shadow(self, "name_search", pairs, original)
                return original
        else:
            _gated(self, "name_search")
        return orig_name_search(self, name, domain, operator, limit)

    BaseModel.name_search = _restamp(name_search, orig_name_search)
    _BASE_METHODS["name_search"] = BaseModel.name_search
    BaseModel.search_read = _restamp(search_read, orig_search_read)
    BaseModel.search_count = _restamp(search_count, orig_search_count)
    BaseModel._read_group = _restamp(_read_group, orig_read_group)
    _BASE_METHODS["search_read"] = search_read
    _BASE_METHODS["search_count"] = search_count
    _BASE_METHODS["_read_group"] = _read_group
    _INSTALLED = {
        "orig_search_read": orig_search_read,
        "orig_search_count": orig_search_count,
        "orig_read_group": orig_read_group,
    }
    return _INSTALLED
