import collections
import sys

import rust_orm_shim

from odoo.exceptions import AccessError, UserError

MIN_COMPARED = 5000
MANY2ONES = 4
X2MANYS = 3
COMPUTED = 3
ROWS = 40


def leg(mode, call):
    rust_orm_shim.set_mode(mode)
    routed = rust_orm_shim.STATS["kernel"]
    try:
        answer = call()
    except (AccessError, UserError) as exc:
        answer = type(exc).__name__
    finally:
        env.cr.rollback()  # noqa: F821
    return answer, rust_orm_shim.STATS["kernel"] > routed


def shapes(model):
    fields = model._fields
    m2o = [n for n, f in fields.items() if f.type == "many2one" and f.store]
    x2m = [
        n
        for n, f in fields.items()
        if f.type in ("one2many", "many2many") and (f.store or f.type == "one2many")
    ]
    m2o, x2m = m2o[:MANY2ONES], x2m[:X2MANYS]
    out = {
        "search_count": lambda: model.search_count([]),
        "display_name": lambda: model.search_read([], ["display_name"], limit=ROWS),
        "web_search_read display_name": lambda: model.web_search_read(
            [], {"display_name": {}}, limit=ROWS
        )["records"],
        "name_search": lambda: model.name_search("", limit=ROWS),
    }
    if m2o:
        ids = model.sudo().search([], limit=ROWS).ids
        named = {n: {"fields": {"display_name": {}}} for n in m2o}
        out |= {
            "web_search_read many2one": lambda: model.web_search_read(
                [], {n: {} for n in m2o}, limit=ROWS
            )["records"],
            "web_search_read named": lambda: model.web_search_read(
                [], named, limit=ROWS
            )["records"],
            "search_read many2one": lambda: model.search_read([], m2o, limit=ROWS),
            "read load=None": lambda: model.browse(ids).read(m2o, load=None),
            "web_read many2one": lambda: model.browse(ids).web_read(
                {n: {} for n in m2o}
            ),
            "web_read named": lambda: model.browse(ids).web_read(named),
            "_read_group": lambda: [
                (tuple(row[:-1]), row[-1])
                for row in model._read_group([], [m2o[0]], ["__count"])
            ],
            "web_read_group": lambda: model.web_read_group([], [m2o[0]], ["__count"]),
        }
    computed = [
        n
        for n, f in fields.items()
        if f.compute
        and not f.store
        and not f.related
        and f.type not in ("one2many", "many2many", "binary", "properties", "json")
    ][:COMPUTED]
    if computed:
        out["web_search_read computed"] = lambda: model.web_search_read(
            [], {n: {} for n in [*m2o[:1], *computed]}, limit=ROWS
        )["records"]
    expandable = [
        n
        for n, f in fields.items()
        if f.store and f.group_expand and f.type in ("many2one", "selection")
    ]
    if expandable:
        out["web_read_group group_expand"] = lambda: model.with_context(
            read_group_expand=True
        ).web_read_group([], [expandable[0]], ["__count"])
    if x2m:
        out |= {
            "web_search_read x2many": lambda: model.web_search_read(
                [], {n: {} for n in x2m}, limit=ROWS
            )["records"],
            "search_read x2many": lambda: model.search_read([], x2m, limit=ROWS),
            "web_read x2many": lambda: model.browse(
                model.sudo().search([], limit=ROWS).ids
            ).web_read({n: {} for n in x2m}),
        }
    return out


previous = rust_orm_shim.MODE, rust_orm_shim.SAMPLE
rust_orm_shim.set_sample(0.0)
compared = collections.Counter()
mismatched = collections.Counter()
example = {}
try:
    users = env["res.users"].with_context(active_test=False).search([]).ids  # noqa: F821
    models = [
        name
        for name in env.registry  # noqa: F821
        if env[name]._auto and not env[name]._abstract and not env[name]._transient  # noqa: F821
    ]
    companies = env["res.company"].search([]).ids  # noqa: F821
    langs = [code for code, _name in env["res.lang"].get_installed()]  # noqa: F821
    contexts = [
        (uid, ctx)
        for uid in users
        for ctx in (
            {},
            {"active_test": False},
            *({"lang": lang} for lang in langs if lang != "en_US"),
        )
    ]
    for user in env["res.users"].browse(users):  # noqa: F821
        mine = [c for c in companies if c in user.company_ids.ids]
        if len(mine) > 1:
            contexts += [
                (user.id, {"allowed_company_ids": allowed})
                for allowed in ([mine[0]], [mine[-1]], mine, mine[::-1])
            ]
    for uid, ctx in contexts:
        for name in models:
            model = env(user=uid, context=ctx)[name]  # noqa: F821
            for shape, call in shapes(model).items():
                routed_answer, routed = leg("on", call)
                if not routed:
                    continue
                python_answer, _ = leg("off", call)
                compared[shape] += 1
                if routed_answer != python_answer:
                    mismatched[shape, name, repr(ctx)] += 1
                    example.setdefault(
                        (shape, name, repr(ctx)),
                        (uid, str(routed_answer)[:200], str(python_answer)[:200]),
                    )
finally:
    rust_orm_shim.set_mode(previous[0])
    rust_orm_shim.set_sample(previous[1])

total = sum(compared.values())
print(
    "EVERY USER compared %d over %d users in %d contexts: %s"
    % (total, len(users), len(contexts), dict(compared))
)
for key, n in mismatched.most_common(20):
    uid, routed_answer, python_answer = example[key]
    print(
        "EVERY USER MISMATCH %d %s at uid %d\n  routed %s\n  python %s"
        % (n, key, uid, routed_answer, python_answer)
    )
if mismatched:
    print("EVERY USER FAILED: %d mismatching calls" % sum(mismatched.values()))
    sys.exit(1)
if total < MIN_COMPARED:
    print("EVERY USER FAILED: compared %d, fewer than %d" % (total, MIN_COMPARED))
    sys.exit(1)
print("EVERY USER OK")
