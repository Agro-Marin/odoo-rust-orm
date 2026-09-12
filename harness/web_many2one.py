"""Many2one values of routed reads, at every user, against Python.

`read()` and `web_read` answer a many2one whose target the user cannot read
differently: `read()` redacts it to False, `web_read` keeps its id, and
`{"id": id}` when a name was asked for. The routed `web_search_read` once
built its answer from the kernel's label, which is `read()`'s, and 124 of
1,018 routed calls across the database's users disagreed with Python. No other
stage saw it: their corpora read as users who can see the targets.

Every user of the database reads every table-backed model's first stored
many2ones through `web_search_read` (plain and named), `search_read`, and
`read(load=None)`, routing on and then off, in a transaction rolled back after
each leg. A call counts only when the routed leg reached the kernel; a stage
that compared nothing fails.
"""

import collections
import sys

import rust_orm_shim

from odoo.exceptions import AccessError

MIN_COMPARED = 500
FIELDS_PER_MODEL = 4
ROWS = 40


def leg(mode, call):
    rust_orm_shim.set_mode(mode)
    routed = rust_orm_shim.STATS["kernel"]
    try:
        answer = call()
    except AccessError:
        answer = "AccessError"
    finally:
        env.cr.rollback()  # noqa: F821
    return answer, rust_orm_shim.STATS["kernel"] > routed


def shapes(model, names):
    named = {n: {"fields": {"display_name": {}}} for n in names}
    ids = model.sudo().search([], limit=ROWS).ids
    return {
        "web_search_read plain": lambda: model.web_search_read(
            [], {n: {} for n in names}, limit=ROWS
        )["records"],
        "web_search_read named": lambda: model.web_search_read([], named, limit=ROWS)[
            "records"
        ],
        "search_read": lambda: model.search_read([], names, limit=ROWS),
        "read load=None": lambda: model.browse(ids).read(names, load=None),
    }


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
    for uid in users:
        for name in models:
            model = env(user=uid)[name]  # noqa: F821
            names = [
                n for n, f in model._fields.items() if f.type == "many2one" and f.store
            ][:FIELDS_PER_MODEL]
            if not names:
                continue
            for shape, call in shapes(model, names).items():
                routed_answer, routed = leg("on", call)
                if not routed:
                    continue
                python_answer, _ = leg("off", call)
                compared[shape] += 1
                if routed_answer != python_answer:
                    mismatched[shape, name] += 1
                    example.setdefault(
                        (shape, name),
                        (uid, str(routed_answer)[:200], str(python_answer)[:200]),
                    )
finally:
    rust_orm_shim.set_mode(previous[0])
    rust_orm_shim.set_sample(previous[1])

total = sum(compared.values())
print("WEB M2O compared %d over %d users: %s" % (total, len(users), dict(compared)))
for key, n in mismatched.most_common(20):
    uid, routed_answer, python_answer = example[key]
    print(
        "WEB M2O MISMATCH %d %s at uid %d\n  routed %s\n  python %s"
        % (n, key, uid, routed_answer, python_answer)
    )
if mismatched:
    print("WEB M2O FAILED: %d mismatching calls" % sum(mismatched.values()))
    sys.exit(1)
if total < MIN_COMPARED:
    print("WEB M2O FAILED: compared %d, fewer than %d" % (total, MIN_COMPARED))
    sys.exit(1)
print("WEB M2O OK")
