import datetime
import json
import logging
import os
import random
import threading
import weakref

_logger = logging.getLogger("odoo.rust_kernel.routing")

KERNEL = None
DBNAME = None

KERNEL_FACTORY = None
_KERNEL_TRIED_PID = None
_KERNEL_PID = None
_KERNEL_LOCK = threading.Lock()

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
}


def _env_set(name):
    return frozenset(x.strip() for x in os.environ.get(name, "").split(",") if x.strip())


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
    return out


def reset_breaker(model_name=None):
    if model_name is None:
        STATS["errors_by_model"].clear()
        STATS["tripped"].clear()
    else:
        STATS["errors_by_model"].pop(model_name, None)
        if model_name in STATS["tripped"]:
            STATS["tripped"].remove(model_name)


def _bound_db(env):
    return DBNAME is None or env.registry.db_name == DBNAME


def set_kernel(kernel):
    """The one way a kernel is installed in this process.

    It records the pid alongside, because a kernel is bound to the tokio
    runtime it was built on and a forked child does not inherit that
    runtime's threads -- only the handle to them. A child that used the
    parent's kernel called `block_on` on a runtime nobody was driving and
    hung, instead of falling back to Python. Under `-d`, the master preloads
    the registry before forking, so the hook builds a kernel there and every
    worker inherits it: the common configuration, not the edge case.
    """
    global KERNEL, _KERNEL_PID
    KERNEL = kernel
    _KERNEL_PID = os.getpid() if kernel is not None else None


def _kernel_is_ours():
    return KERNEL is not None and _KERNEL_PID == os.getpid()


def _ensure_kernel(env):
    global KERNEL, _KERNEL_TRIED_PID, _PROCESS_PID
    if _PROCESS_PID != os.getpid() and PROCESS_HOOK is not None:
        _PROCESS_PID = os.getpid()
        try:
            PROCESS_HOOK()
        except Exception:
            _logger.exception("the rust engine process hook failed")
    if _kernel_is_ours():
        return True
    if KERNEL_FACTORY is None or _KERNEL_TRIED_PID == os.getpid():
        return False
    with _KERNEL_LOCK:
        if _kernel_is_ours():
            return True
        if KERNEL is not None:
            # Inherited across a fork: unusable here, and never to be
            # released either, since the parent still owns what it points at.
            _logger.info(
                "dropping the rust kernel inherited from pid %s; building this "
                "process's own", _KERNEL_PID,
            )
            set_kernel(None)
        if _KERNEL_TRIED_PID == os.getpid():
            return False
        _KERNEL_TRIED_PID = os.getpid()
        try:
            set_kernel(KERNEL_FACTORY(env.registry))
        except Exception:
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


def _policy_allows(name):
    if MODE == "off" or _in_baseline():
        return False
    if ONLY and name not in ONLY:
        return False
    if name in EXCEPT:
        return False
    if BREAKER and STATS["errors_by_model"].get(name, 0) >= BREAKER:
        if name not in STATS["tripped"]:
            STATS["tripped"].append(name)
            _logger.warning(
                "rust kernel routing disabled for %s after %d errors; "
                "reset_breaker(%r) to retry",
                name, BREAKER, name,
            )
        return False
    return True


def _verify_this_one():
    return SAMPLE > 0 and _RNG.random() < SAMPLE


def _shadow(model, method, kernel_result, python_result):
    if kernel_result == python_result:
        STATS["shadow_ok"] += 1
        return
    STATS["shadow_diff"] += 1
    _logger.error(
        "SHADOW DIVERGENCE %s.%s\n  python: %.400s\n  kernel: %.400s",
        model._name, method, python_result, kernel_result,
    )
SECURITY_MODELS = frozenset({
    "ir.rule", "ir.model.access", "res.groups", "res.users",
    "ir.default", "res.company", "res.lang", "ir.model", "ir.model.fields",
})
DIRTY_CRS = weakref.WeakSet()

_BASE_METHODS = {}
_GATE_CACHE = {}


def _cache_key(model, *parts):
    return (model.env.registry.db_name, model._name) + parts


def _clean_model(model):
    key = _cache_key(model)
    cached = _GATE_CACHE.get(key)
    if cached is not None:
        return cached
    cls = type(model.sudo())
    ok = all(
        getattr(cls, name) is _BASE_METHODS[name]
        for name in ("_search", "read", "search_read", "_read_group", "search_count")
    )
    _GATE_CACHE[key] = ok
    return ok


def _display_clean(model):
    key = _cache_key(model, "dn")
    cached = _GATE_CACHE.get(key)
    if cached is not None:
        return cached
    cls = type(model.sudo())
    ok = getattr(cls, "_compute_display_name") is _BASE_METHODS["_compute_display_name"]
    if ok:
        f = model._fields.get("display_name")
        compute = getattr(f, "compute", None) if f is not None else None
        ok = compute in (None, "_compute_display_name")
    if ok and model._rec_name:
        f = model._fields.get(model._rec_name)
        ok = f is not None and (bool(f.store) or bool(f.related))
    _GATE_CACHE[key] = ok
    return ok


def _gate(model, fields=None, order=None, domain=None):
    if not _policy_allows(model._name) or not _bound_db(model.env):
        return False
    if not _ensure_kernel(model.env):
        return False
    try:
        if model.env.cr in DIRTY_CRS:
            return False
    except TypeError:
        return False
    if not _clean_model(model):
        return False
    for fname in fields or ():
        if fname == "display_name" and not _display_clean(model):
            return False
        f = model._fields.get(fname)
        if f is None:
            return False
        if f.type == "many2one" and not _display_clean(model.env[f.comodel_name]):
            return False
    return True


def _needs_flush(env):
    try:
        tx = env.transaction
        return tx._cache_store.is_any_dirty() or bool(tx._compute_engine.pending)
    except Exception:
        return True


def _flush_if_needed(env):
    if not _needs_flush(env):
        return True
    try:
        env.flush_all()
        return True
    except Exception as e:  # noqa: BLE001
        STATS["fallback_flush"] = STATS.get("fallback_flush", 0) + 1
        _logger.info("not routing: the flush the kernel needs raised %s", e)
        return False


def _rust_conn(env):
    return env.cr._cnx._rust


def _request(model, method, **kw):
    env = model.env
    req = {
        "model": model._name,
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
        # Read by `_read_group` only, where a datetime granularity under a
        # non-UTC zone is refused; the other methods accept and ignore it.
        "tz": env.context.get("tz") or None,
    }
    if kw.get("domain") is not None and not isinstance(kw["domain"], list):
        from odoo.fields import Domain

        kw["domain"] = list(Domain(kw["domain"]))
    req.update(kw)
    return json.dumps(req)


def _dispatch(model, method, **kw):
    with model.env.cr.savepoint(flush=False):
        raw = KERNEL.dispatch(_rust_conn(model.env), _request(model, method, **kw))
    return json.loads(raw)


def _parse_dt(text):
    fmt = "%Y-%m-%d %H:%M:%S.%f" if "." in text else "%Y-%m-%d %H:%M:%S"
    return datetime.datetime.strptime(text, fmt)


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
                rec[name] = datetime.datetime.strptime(val, "%Y-%m-%d").date()
            elif f.type == "many2one" and isinstance(val, list):
                rec[name] = (val[0], val[1])
    return records


def _record_error(model, e):
    unexpected = not isinstance(e, RuntimeError)
    msg = "%s: %s" % (type(e).__name__, e) if unexpected else str(e)
    STATS["fallback_error"] += 1
    STATS["errors_by_model"][model._name] = STATS["errors_by_model"].get(model._name, 0) + 1
    first = model._name not in STATS["errors"]
    STATS["errors"].setdefault(model._name, msg[:160])
    if unexpected:
        _logger.log(
            logging.WARNING if first else logging.DEBUG,
            "the rust routing path raised for %s; answering from python. "
            "This is a bug in the shim or the kernel, not a refusal: %s",
            model._name, msg[:200], exc_info=first,
        )
    else:
        _logger.log(
            logging.INFO if first else logging.DEBUG,
            "falling back to Python for %s: %s", model._name, msg[:200],
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


def _web_spec_plan(model, specification):
    fields, named, plain = [], set(), set()
    for name, spec in specification.items():
        f = model._fields.get(name)
        if f is None:
            return None
        spec = spec or {}
        if not isinstance(spec, dict):
            return None
        if f.type == "many2one":
            if "context" in spec:
                return None
            sub = spec.get("fields")
            if sub is None:
                plain.add(name)
            elif isinstance(sub, dict) and set(sub) == {"display_name"}:
                named.add(name)
            else:
                return None
        elif f.type in ("one2many", "many2many"):
            if spec:
                return None
        elif f.type in ("reference", "many2one_reference", "properties"):
            if spec:
                return None
        fields.append(name)
    return fields, named, plain


def _web_records(records, named, plain):
    for rec in records:
        for name in named:
            val = rec.get(name)
            if isinstance(val, list):
                rec[name] = {"id": val[0], "display_name": val[1]}
        for name in plain:
            val = rec.get(name)
            if isinstance(val, list):
                rec[name] = val[0]
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



def _order_is_default(order, groupby):
    if not order:
        return True
    terms = [t.strip() for t in order.split(",") if t.strip()]
    if len(terms) != len(groupby):
        return False
    for term, spec in zip(terms, groupby):
        parts = term.split()
        if not parts or parts[0] != spec:
            return False
        if len(parts) > 1 and (len(parts) > 2 or parts[1].lower() != "asc"):
            return False
    return True




READ_SKIP_TYPES = frozenset({"binary", "properties", "properties_definition", "reference", "many2one_reference"})


def _read_fields_ok(model, fields):
    for name in fields:
        f = model._fields.get(name)
        if f is None or f.type in READ_SKIP_TYPES:
            return False
        if name == "display_name":
            continue
        if not (f.store or f.related):
            return False
    return True


def _read_reorder(ids, records, load):
    by_id = {rec["id"]: rec for rec in records}
    if any(i not in by_id for i in ids):
        return None
    out = []
    for i in ids:
        rec = dict(by_id[i])
        if load is None:
            for name, val in rec.items():
                if isinstance(val, tuple):
                    rec[name] = val[0]
        out.append(rec)
    return out



def _name_search_clean(model):
    key = _cache_key(model, "ns")
    cached = _GATE_CACHE.get(key)
    if cached is not None:
        return cached
    cls = type(model.sudo())
    ok = (
        getattr(cls, "name_search") is _BASE_METHODS["name_search"]
        and getattr(cls, "_search_display_name") is _BASE_METHODS["_search_display_name"]
        and _display_clean(model)
    )
    _GATE_CACHE[key] = ok
    return ok


def _restamp(new, orig):
    for name in STAMPS:
        if getattr(orig, name, False):
            setattr(new, name, getattr(orig, name))
    return new


def install():
    global DBNAME
    if DBNAME is None:
        try:
            import rust_db_shim

            DBNAME = rust_db_shim._dbname(rust_db_shim.CONNINFO)
        except Exception:  # noqa: BLE001  (an optional convenience, never a gate)
            pass
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
    ):
        _BASE_METHODS[name] = getattr(BaseModel, name)

    orig_search_read = BaseModel.search_read
    orig_search_count = BaseModel.search_count
    orig_read_group = BaseModel._read_group

    def search_read(self, domain=None, fields=None, offset=0, limit=None, order=None, **kw):
        if not kw and fields and _gate(self, fields, order=order, domain=domain):
            try:
                if not _flush_if_needed(self.env):
                    raise RuntimeError("flush failed; not routing")
                recs = _dispatch(
                    self,
                    "search_read",
                    domain=domain or [],
                    fields=list(fields),
                    offset=offset or 0,
                    limit=limit,
                    order=order,
                )
                STATS["kernel"] += 1
                revived = _revive_records(self, recs)
                if MODE != "shadow" and not _verify_this_one():
                    return revived
                original = _baseline(orig_search_read, self, domain=domain, fields=fields,
                                            offset=offset, limit=limit, order=order, **kw)
                _shadow(self, "search_read", revived, original)
                return original
            except Exception as e:  # noqa: BLE001  see _record_error
                _record_error(self, e)
        else:
            STATS["fallback_gate"] += 1
        return orig_search_read(self, domain=domain, fields=fields, offset=offset,
                                limit=limit, order=order, **kw)

    def search_count(self, domain, limit=None):
        if _gate(self, domain=domain):
            try:
                if not _flush_if_needed(self.env):
                    raise RuntimeError("flush failed; not routing")
                n = _dispatch(self, "search_count", domain=domain or [], limit=limit)
                STATS["kernel"] += 1
                if MODE != "shadow" and not _verify_this_one():
                    return n
                original = _baseline(orig_search_count, self, domain, limit=limit)
                _shadow(self, "search_count", n, original)
                return original
            except Exception as e:  # noqa: BLE001  see _record_error
                _record_error(self, e)
        else:
            STATS["fallback_gate"] += 1
        return orig_search_count(self, domain, limit=limit)

    def _read_group(self, domain, groupby=(), aggregates=(), having=(), offset=0,
                    limit=None, order=None):
        supported = (
            not having and not offset and limit is None
            and _order_is_default(order, list(groupby))
            and groupby and _gate(self, [g.split(":")[0] for g in groupby], domain=domain)
            and all(a == "__count" or ":" in a for a in aggregates)
        )
        if supported:
            try:
                if not _flush_if_needed(self.env):
                    raise RuntimeError("flush failed; not routing")
                rows = _dispatch(
                    self,
                    "read_group",
                    domain=domain or [],
                    groupby=list(groupby),
                    aggregates=list(aggregates),
                    order=order or None,
                    groupby_labels=False,
                )
                STATS["kernel"] += 1
                out = []
                gb_fields = [self._fields[g.split(":")[0]] for g in groupby]
                gb_gran = [":" in g for g in groupby]
                for row in rows:
                    item = []
                    for i, f in enumerate(gb_fields):
                        v = row[i]
                        if f.type == "many2one":
                            item.append(
                                self.env[f.comodel_name].browse(v) if v else
                                self.env[f.comodel_name]
                            )
                        elif v is False:
                            item.append(False)
                        elif gb_gran[i] and f.type == "datetime":
                            item.append(_parse_dt(v))
                        elif gb_gran[i] and f.type == "date":
                            item.append(datetime.datetime.strptime(v, "%Y-%m-%d").date())
                        elif f.type == "datetime" and isinstance(v, str):
                            item.append(_parse_dt(v))
                        elif f.type == "date" and isinstance(v, str):
                            item.append(datetime.datetime.strptime(v, "%Y-%m-%d").date())
                        else:
                            item.append(v)
                    item.extend(row[len(gb_fields):])
                    out.append(tuple(item))
                if MODE != "shadow" and not _verify_this_one():
                    return out
                original = _baseline(orig_read_group, self, domain, groupby=groupby,
                                           aggregates=aggregates, having=having,
                                           offset=offset, limit=limit, order=order)
                _shadow(self, "_read_group", out, original)
                return original
            except Exception as e:  # noqa: BLE001  see _record_error
                _record_error(self, e)
        else:
            STATS["fallback_gate"] += 1
        return orig_read_group(self, domain, groupby=groupby, aggregates=aggregates,
                               having=having, offset=offset, limit=limit, order=order)

    orig_create = BaseModel.create
    orig_write = BaseModel.write
    orig_unlink = BaseModel.unlink

    def _taint(self):
        if self._name in SECURITY_MODELS:
            try:
                DIRTY_CRS.add(self.env.cr)
            except TypeError:
                pass

    def create(self, vals_list):
        _taint(self)
        return orig_create(self, vals_list)

    def write(self, vals):
        _taint(self)
        return orig_write(self, vals)

    def unlink(self):
        _taint(self)
        return orig_unlink(self)

    BaseModel.create = _restamp(create, orig_create)
    BaseModel.write = _restamp(write, orig_write)
    BaseModel.unlink = _restamp(unlink, orig_unlink)

    import odoo.db.cursor as _cursor_mod

    orig_rollback = _cursor_mod.Cursor.rollback

    def rollback(self):
        DIRTY_CRS.discard(self)
        return orig_rollback(self)

    _cursor_mod.Cursor.rollback = rollback


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

        def _stamp_envelope_version(records):
            try:
                from odoo.http import request
            except ModuleNotFoundError:
                return
            if request:
                request._response_version = _canonical_digest(records)

        def web_search_read(self, domain, specification, offset=0, limit=None,
                            order=None, count_limit=None):
            plan = None
            if _web_clean(self):
                plan = _web_spec_plan(self, specification)
            if plan and plan[0] and _gate(self, plan[0], order=order, domain=domain):
                fields, named, plain = plan
                try:
                    if not _flush_if_needed(self.env):
                        raise RuntimeError("flush failed; not routing")
                    recs = _dispatch(
                        self, "search_read", domain=domain or [], fields=fields,
                        offset=offset or 0, limit=limit, order=order,
                    )
                    STATS["kernel"] += 1

                    def count(cap):
                        n = _dispatch(self, "search_count", domain=domain or [], limit=cap)
                        STATS["kernel"] += 1
                        return n

                    length = _web_length(
                        len(recs), offset or 0, limit, count_limit,
                        bool(self.env.context.get("force_search_count")), count,
                    )
                    result = {
                        "length": length,
                        "records": _revive_records(self, _web_records(recs, named, plain)),
                    }
                    result["__version"] = _canonical_digest(result)
                    # Python does not stamp the response ENVELOPE from
                    # `web_search_read`; it gets there as a side effect of
                    # the inner `records.web_read(specification)`, which
                    # carries `@versioned_envelope` and digests the record
                    # list alone. A routed answer never makes that call, so
                    # the top-level "version" key simply vanished from every
                    # `web_search_read` the kernel served -- the records were
                    # byte-identical and one envelope key was missing, which
                    # is why the shadow comparison (it diffs the METHOD's
                    # result) reported no divergence.
                    _stamp_envelope_version(result["records"])
                    if MODE != "shadow" and not _verify_this_one():
                        return result
                    original = _baseline(
                        orig_web_search_read, self, domain, specification, offset=offset, limit=limit,
                        order=order, count_limit=count_limit,
                    )
                    _shadow(self, "web_search_read", result, original)
                    return original
                except Exception as e:  # noqa: BLE001  see _record_error
                    _record_error(self, e)
            else:
                STATS["fallback_gate"] += 1
            return orig_web_search_read(
                self, domain, specification, offset=offset, limit=limit,
                order=order, count_limit=count_limit,
            )

        from odoo import api as _api
        from odoo.tools.cache_version import versioned as _versioned

        WebBase.web_search_read = _api.model(_api.readonly(_versioned(web_search_read)))

    orig_read = _BASE_METHODS["read"]

    def read(self, fields=None, load="_classic_read"):
        ids = list(self._ids)
        routable = (
            bool(fields) and ids
            and all(isinstance(i, int) and i > 0 for i in ids)
            and load in (None, "_classic_read")
            and _read_fields_ok(self, fields)
            and _gate(self, list(fields))
        )
        if routable:
            try:
                if not _flush_if_needed(self.env):
                    raise RuntimeError("flush failed; not routing")
                recs = _dispatch(
                    self.with_context(active_test=False), "search_read",
                    domain=[("id", "in", sorted(set(ids)))], fields=list(fields),
                    offset=0, limit=None, order="id",
                )
                STATS["kernel"] += 1
                ordered = _read_reorder(ids, _revive_records(self, recs), load)
                if ordered is None:
                    STATS["fallback_gate"] += 1
                    return _baseline(orig_read, self, fields, load=load)
                if MODE != "shadow" and not _verify_this_one():
                    return ordered
                original = _baseline(orig_read, self, fields, load=load)
                _shadow(self, "read", ordered, original)
                return original
            except Exception as e:  # noqa: BLE001  see _record_error
                _record_error(self, e)
        else:
            STATS["fallback_gate"] += 1
        return orig_read(self, fields, load=load)

    BaseModel.read = _restamp(read, orig_read)
    _BASE_METHODS["read"] = BaseModel.read

    orig_name_search = _BASE_METHODS["name_search"]

    def name_search(self, name="", domain=None, operator="ilike", limit=100):
        if _name_search_clean(self) and _gate(self, ["display_name"], domain=domain):
            try:
                if not _flush_if_needed(self.env):
                    raise RuntimeError("flush failed; not routing")
                full = [("display_name", operator, name)]
                if domain:
                    from odoo.fields import Domain

                    full = full + list(Domain(domain))
                recs = _dispatch(
                    self, "search_read", domain=full, fields=["display_name"],
                    offset=0, limit=limit, order=None,
                )
                STATS["kernel"] += 1
                pairs = [(r["id"], r["display_name"] or "") for r in recs]
                if MODE != "shadow" and not _verify_this_one():
                    return pairs
                original = _baseline(orig_name_search, self, name, domain, operator, limit)
                _shadow(self, "name_search", pairs, original)
                return original
            except Exception as e:  # noqa: BLE001  see _record_error
                _record_error(self, e)
        else:
            STATS["fallback_gate"] += 1
        return orig_name_search(self, name, domain, operator, limit)

    BaseModel.name_search = _restamp(name_search, orig_name_search)
    _BASE_METHODS["name_search"] = BaseModel.name_search
    BaseModel.search_read = _restamp(search_read, orig_search_read)
    BaseModel.search_count = _restamp(search_count, orig_search_count)
    BaseModel._read_group = _restamp(_read_group, orig_read_group)
    _BASE_METHODS["search_read"] = search_read
    _BASE_METHODS["search_count"] = search_count
    _BASE_METHODS["_read_group"] = _read_group
    return {"orig_search_read": orig_search_read,
            "orig_search_count": orig_search_count,
            "orig_read_group": orig_read_group}
