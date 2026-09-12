"""Routed reads at every user of the database, against Python.

The corpora the other stages compare read as a handful of identities the
fixture seeds, and they read targets those identities can see. The first thing
this stage caught was the difference between `read()` and `web_read` on a
many2one whose target the user cannot read: `read()` redacts it to False,
`web_read` keeps its id, and `{"id": id}` when a name was asked for. The
routed `web_search_read` used the kernel's label, which is `read()`'s answer,
and 124 of 1,018 routed calls across the database's users disagreed.

Every user of the database, archived ones and other companies' included, reads
every table-backed model through each routed method: many2ones and x2manys
through `web_search_read`, `search_read` and `read(load=None)`, then
`search_count`, `display_name`, `name_search`, `_read_group` and
`web_read_group` on a many2one. Each call runs routing on and then off, in a
transaction rolled back after each leg, and counts only when the routed leg
reached the kernel. A stage that compared too little fails.
"""

import collections
import sys

import rust_orm_shim

from odoo.exceptions import AccessError, UserError

MIN_COMPARED = 2000
MANY2ONES = 4
X2MANYS = 3
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
            "_read_group": lambda: [
                (tuple(row[:-1]), row[-1])
                for row in model._read_group([], [m2o[0]], ["__count"])
            ],
            "web_read_group": lambda: model.web_read_group([], [m2o[0]], ["__count"]),
        }
    if x2m:
        out |= {
            "web_search_read x2many": lambda: model.web_search_read(
                [], {n: {} for n in x2m}, limit=ROWS
            )["records"],
            "search_read x2many": lambda: model.search_read([], x2m, limit=ROWS),
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
    for uid in users:
        for name in models:
            for shape, call in shapes(env(user=uid)[name]).items():  # noqa: F821
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
print("EVERY USER compared %d over %d users: %s" % (total, len(users), dict(compared)))
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
