#!/usr/bin/env python3
import importlib.util
import json
import os
import sys

FAILURES = []


def check(name, got, want):
    if got != want:
        FAILURES.append("%s\n    got  %r\n    want %r" % (name, got, want))


def main():
    try:
        import engine_py
    except ImportError as exc:
        print("SHIMS SKIP: engine_py is not importable (%s)" % exc)
        return 0

    db_shim, orm_shim = engine_py.install_shims()

    dbname = db_shim._dbname
    check("dict", dbname({"dbname": "mydb"}), "mydb")
    check("dict database=", dbname({"database": "mydb"}), "mydb")
    check("unquoted", dbname("host=localhost dbname=mydb user=odoo"), "mydb")
    check(
        "quoted (the defect)",
        dbname("host='/var/run/postgresql' dbname='mydb' user='odoo'"),
        "mydb",
    )
    check("value with a space", dbname("dbname='has space' host=x"), "has space")
    check("escaped quote in an earlier value",
          dbname(r"password='a b\'c' dbname='mydb'"), "mydb")
    check("escaped backslash", dbname(r"password='a\\b' dbname='mydb'"), "mydb")
    check("absent", dbname("host=x user=y"), None)
    check("empty", dbname(""), None)
    check("not a spec", dbname(None), None)
    check("spaces around =", dbname("dbname = mydb"), "mydb")
    check("dangling key", dbname("dbname=mydb host"), "mydb")

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
            FAILURES.append("set_mode accepted an unknown mode")
    finally:
        orm_shim.MODE, orm_shim.KERNEL, orm_shim.DBNAME = saved
        orm_shim.STATS["errors_by_model"].clear()

    saved_sample = orm_shim.SAMPLE
    try:
        orm_shim.set_sample(0)
        check("sample 0 never fires",
              any(orm_shim._verify_this_one() for _ in range(2000)), False)
        orm_shim.set_sample(1)
        check("sample 1 always fires",
              all(orm_shim._verify_this_one() for _ in range(200)), True)
        orm_shim.set_sample(0.5)
        fired = sum(orm_shim._verify_this_one() for _ in range(4000))
        check("sample 0.5 is a coin", 1600 < fired < 2400, True)
        for bad in (-0.1, 1.5):
            try:
                orm_shim.set_sample(bad)
            except ValueError:
                pass
            else:
                FAILURES.append("set_sample accepted %r" % bad)
    finally:
        orm_shim.SAMPLE = saved_sample

    addon = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "addons", "rust_engine", "__init__.py",
    )
    if os.path.exists(addon):
        spec = importlib.util.spec_from_file_location("rust_engine_probe", addon)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
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
    else:
        print("  (rust_engine addon not found beside the harness; switch untested)")

    recs = [
        {"id": 1, "partner_id": [7, "Seven"], "user_id": [2, "Admin"], "name": "a"},
        {"id": 2, "partner_id": False, "user_id": [3, "Bob"], "name": "b"},
    ]
    out = orm_shim._web_records(recs, named={"partner_id"}, plain={"user_id"})
    check("m2o named", out[0]["partner_id"], {"id": 7, "display_name": "Seven"})
    check("m2o plain", out[0]["user_id"], 2)
    check("m2o empty stays False", out[1]["partner_id"], False)
    check("untouched field", out[1]["name"], "b")

    calls = []

    def count(cap):
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

    class F:
        def __init__(self, type_):
            self.type = type_

    class M:
        _fields = {
            "name": F("char"), "partner_id": F("many2one"), "user_id": F("many2one"),
            "tag_ids": F("many2many"), "ref_id": F("reference"), "props": F("properties"),
        }

    plan = orm_shim._web_spec_plan
    check(
        "plain spec",
        plan(M(), {"name": {}, "partner_id": {"fields": {"display_name": {}}},
                   "user_id": {}, "tag_ids": {}}),
        (["name", "partner_id", "user_id", "tag_ids"], {"partner_id"}, {"user_id"}),
    )
    check("unknown field refuses", plan(M(), {"nope": {}}), None)
    check("m2o with extra sub-field refuses",
          plan(M(), {"partner_id": {"fields": {"display_name": {}, "email": {}}}}), None)
    check("m2o with context refuses",
          plan(M(), {"partner_id": {"fields": {"display_name": {}}, "context": {"x": 1}}}), None)
    check("x2many with fields refuses", plan(M(), {"tag_ids": {"fields": {"name": {}}}}), None)
    check("x2many with order refuses", plan(M(), {"tag_ids": {"order": "name"}}), None)
    check("x2many with limit refuses", plan(M(), {"tag_ids": {"limit": 5}}), None)
    check("reference with spec refuses", plan(M(), {"ref_id": {"fields": {}}}), None)
    check("bare reference maps", plan(M(), {"ref_id": {}}), (["ref_id"], set(), set()))
    check("properties with spec refuses", plan(M(), {"props": {"fields": {}}}), None)

    odoo_root = os.environ.get("RUSTORM_ODOO_ROOT") or os.path.join(
        os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "odoo"
    )
    sys.path.insert(0, odoo_root)
    try:
        import odoo.addons
        import odoo.orm.models.base as base_mod

        odoo.addons.__path__.append(os.path.join(odoo_root, "addons"))
        from odoo.addons.web.models import web_read as web_read_mod
    except ImportError as exc:
        print("  (odoo not importable from %s; stamps untested: %s)" % (odoo_root, exc))
    else:
        orm_shim.install()
        BaseModel = base_mod.BaseModel
        for name in ("search_read", "search_count"):
            check("%s keeps api.model" % name, getattr(BaseModel, name)._api_model, True)
            check("%s keeps api.readonly" % name, getattr(BaseModel, name)._readonly, True)
        check("create keeps api.model", BaseModel.create._api_model, True)
        check("name_search keeps api.model", BaseModel.name_search._api_model, True)
        check("name_search keeps api.readonly", BaseModel.name_search._readonly, True)
        from odoo.fields import Domain

        class _Env:
            uid, su, context = 2, False, {}

        class _Model:
            _name = "res.partner"
            env = _Env()

        req = json.loads(orm_shim._request(_Model(), "search_count", domain=Domain([("id", "=", 1)]) & Domain([("active", "=", True)])))
        check("a Domain object serialises as a list", isinstance(req["domain"], list) and len(req["domain"]) >= 2, True)
        wsr = web_read_mod.Base.web_search_read
        check("web_search_read keeps api.model", getattr(wsr, "_api_model", False), True)
        check("web_search_read keeps api.readonly", getattr(wsr, "_readonly", False), True)

    od = orm_shim._order_is_default
    check("no order is default", od("", ["stage_id"]), True)
    check("groupby order is default", od("stage_id, date:month", ["stage_id", "date:month"]), True)
    check("explicit asc is default", od("stage_id asc", ["stage_id"]), True)
    check("desc is not default", od("stage_id desc", ["stage_id"]), False)
    check("other field is not default", od("name", ["stage_id"]), False)
    check("reordered groupby is not default", od("b, a", ["a", "b"]), False)
    check("aggregate order is not default", od("__count desc", ["stage_id"]), False)
    check("trailing term is not default", od("a, b", ["a"]), False)

    class _F:
        def __init__(self, type_, store=True, related=None):
            self.type, self.store, self.related = type_, store, related

    class _M:
        _fields = {"name": _F("char"), "icon": _F("char", store=False), "img": _F("binary"),
                   "cur": _F("many2one", store=False, related="company_id.currency_id"), "display_name": _F("char", store=False)}

    rfo = orm_shim._read_fields_ok
    check("stored fields read", rfo(_M(), ["name"]), True)
    check("related non-stored reads", rfo(_M(), ["name", "cur"]), True)
    check("display_name reads", rfo(_M(), ["display_name"]), True)
    check("a compute does not", rfo(_M(), ["name", "icon"]), False)
    check("a binary does not", rfo(_M(), ["img"]), False)
    check("an unknown field does not", rfo(_M(), ["nope"]), False)
    rr = orm_shim._read_reorder
    recs = [{"id": 2, "name": "b", "partner_id": (7, "Seven")}, {"id": 1, "name": "a", "partner_id": False}]
    check("read keeps the requested order", [r["id"] for r in rr([1, 2], recs, "_classic_read")], [1, 2])
    check("read repeats a repeated id", [r["id"] for r in rr([2, 2, 1], recs, "_classic_read")], [2, 2, 1])
    check("classic read keeps m2o tuples", rr([2], recs, "_classic_read")[0]["partner_id"], (7, "Seven"))
    check("load=None bares the m2o id", rr([2], recs, None)[0]["partner_id"], 7)
    check("a missing id is not answered", rr([1, 3], recs, None), None)
    check("an unrequested row is ignored", [r["id"] for r in rr([1], recs, None)], [1])
    check("policy allows outside a baseline", orm_shim._in_baseline(), False)
    check("policy refuses inside a baseline", orm_shim._baseline(lambda: orm_shim._in_baseline()), True)
    check("baseline depth unwinds", orm_shim._in_baseline(), False)

    parse = orm_shim._parse_dt
    check("no microseconds", str(parse("2026-08-31 12:34:56")), "2026-08-31 12:34:56")
    check("microseconds", str(parse("2026-08-31 12:34:56.123456")),
          "2026-08-31 12:34:56.123456")
    check("trailing zeros kept", str(parse("2026-08-31 12:34:56.100000")),
          "2026-08-31 12:34:56.100000")

    diff_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "diff.py")
    spec = importlib.util.spec_from_file_location("diff_probe", diff_path)
    diff = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(diff)

    def one(exp, act):
        p, f, r, v, d = diff.score({"c": exp}, {"c": act})
        return ("pass" if p else "") + ("fail" if f else "") + \
               ("refused" if r else "") + ("vacuous" if v else "") + \
               ("+denied" if d else "")

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
    check("both failed otherwise is vacuous", one(missing, missing), "vacuous")
    p, _f, _r, v, _d = diff.score({"c": missing}, {"c": missing})
    check("vacuous is not a pass", (len(p), len(v)), (0, 1))
    p, f, _r, _v, _d = diff.score({"c": ok}, {})
    check("missing actual fails", (len(p), len(f)), (0, 1))

    for failure in FAILURES:
        print("  FAIL %s" % failure)
    print("SHIMS %s" % ("OK" if not FAILURES else "FAILED (%d)" % len(FAILURES)))
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
