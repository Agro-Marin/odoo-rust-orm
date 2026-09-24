import collections
import datetime
import json
import sys
import time

import rust_orm_shim
from rust_engine_errors import KernelRefused

from odoo import Command
from odoo.exceptions import AccessError, UserError

# the principals whose groups the memberships misstate: a grant limited to
# some companies, a dated grant, a privilege. Each is routed and compared
# with Python in every company context it can be in; the kernel either
# answers what Python answers or refuses, and each must be answered at least
# once, or the stage proves nothing
MIN_ROUTED = 5
ROWS = 80
TAG = "rustorm_grant"
PRIVILEGE = "rustorm_harness.grant_privilege"


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


def seed(env):
    su = env(su=True)
    Company = su["res.company"]
    company_a = su.ref("base.main_company")
    company_b = Company.search([("name", "=", f"{TAG} B")]) or Company.create(
        {"name": f"{TAG} B"}
    )
    Partner = su["res.partner"]
    for name, company in [
        ("in A", company_a),
        ("in B", company_b),
        ("shared", Company),
        ("hidden in A", company_a),
        ("hidden in B", company_b),
        ("hidden shared", Company),
    ]:
        full = f"{TAG} {name}"
        if not Partner.search_count([("name", "=", full)]):
            Partner.create({"name": full, "company_id": company.id or False})

    Groups = su["res.groups"]

    def group(name):
        return Groups.search([("name", "=", f"{TAG} {name}")]) or Groups.create(
            {"name": f"{TAG} {name}"}
        )

    reader, guard = group("reader"), group("guard")
    module, _, xmlid_name = PRIVILEGE.partition(".")
    privilege = su.ref(PRIVILEGE, raise_if_not_found=False)
    if privilege is None:
        privilege = Groups.create({"name": f"{TAG} privilege", "is_privilege": True})
        su["ir.model.data"].create(
            {
                "module": module,
                "name": xmlid_name,
                "model": "res.groups",
                "res_id": privilege.id,
                "noupdate": True,
            }
        )
    Access = su["ir.access"].with_context(active_test=False)
    partner_model = su["ir.model"]._get_id("res.partner")
    for name, grp, kind, domain in [
        ("reader", reader, "permission", f"[('name', 'ilike', '{TAG}')]"),
        ("guard", guard, "guard", f"[('name', 'not ilike', '{TAG} hidden')]"),
        ("privilege reader", privilege, "permission", f"[('name', 'ilike', '{TAG}')]"),
        ("privilege guard", privilege, "guard", f"[('name', 'not ilike', '{TAG} hidden')]"),
    ]:
        if not Access.search_count([("name", "=", f"{TAG} {name}")]):
            Access.create(
                {
                    "name": f"{TAG} {name}",
                    "model_id": partner_model,
                    "group_id": grp.id,
                    "kind": kind,
                    "guard_scope": "members" if kind == "guard" else "everyone",
                    "operation": "r",
                    "domain": domain,
                }
            )

    Users = su["res.users"].with_context(no_reset_password=True, active_test=False)

    def user(login, type_xmlid):
        return Users.search([("login", "=", login)]) or Users.create(
            {
                "name": login,
                "login": login,
                "company_id": company_a.id,
                "company_ids": [Command.set([company_a.id, company_b.id])],
                "group_ids": [Command.set([su.ref(type_xmlid).id])],
            }
        )

    portal_scoped = user(f"{TAG}_portal_scoped", "base.group_portal")
    guard_scoped = user(f"{TAG}_guard_scoped", "base.group_user")
    dated = user(f"{TAG}_dated", "base.group_portal")
    window_closed = user(f"{TAG}_window_closed", "base.group_portal")
    privileged = user(f"{TAG}_privileged_portal", "base.group_portal")

    Grant = su["res.users.grant"].with_context(active_test=False)
    now = env.cr.now()
    opened = []
    for grantee, grp, vals in [
        (portal_scoped, reader, {"company_ids": [Command.set(company_a.ids)]}),
        (guard_scoped, guard, {"company_ids": [Command.set(company_a.ids)]}),
        (dated, guard, {"date_to": now + datetime.timedelta(days=30)}),
        (dated, reader, {"date_from": now + datetime.timedelta(days=30)}),
        # still active in the table once its end has passed: the boundary
        # cron settles it, and no cron runs here
        (window_closed, reader, {"date_to": now + datetime.timedelta(seconds=2)}),
    ]:
        if not Grant.search_count(
            [
                ("user_id", "=", grantee.id),
                ("group_id", "=", grp.id),
                ("state", "in", ("scheduled", "active")),
            ]
        ):
            Grant.create(
                {"user_id": grantee.id, "group_id": grp.id, "cause": "manual", **vals}
            )
            opened.append(grantee.id)
    return (
        (company_a.id, company_b.id),
        {
            "portal scoped": portal_scoped.id,
            "guard scoped": guard_scoped.id,
            "dated": dated.id,
            "window closed": window_closed.id,
            "privileged": privileged.id,
        },
        window_closed.id in opened,
    )


def shapes(model):
    ids = model.sudo().search([], limit=ROWS).ids
    fields = [f for f in ("name", "company_id") if f in model._fields] or ["display_name"]
    out = {
        "search_count": lambda: model.search_count([]),
        "search_read": lambda: model.search_read([], fields, limit=ROWS, order="id"),
        "web_search_read": lambda: model.web_search_read(
            [], {"display_name": {}}, limit=ROWS, order="id"
        )["records"],
        "name_search": lambda: model.name_search("", limit=ROWS),
        "read": lambda: model.browse(ids).read(fields),
    }
    if "company_id" in model._fields:
        out["_read_group"] = lambda: sorted(
            (row[0].id, row[1]) for row in model._read_group([], ["company_id"], ["__count"])
        )
    return out


def refused_without_python_state(uid):
    # the kernel alone reads the memberships, which misstate this principal
    request = json.dumps(
        {"model": "res.partner", "method": "search_count", "uid": uid, "domain": []}
    )
    try:
        rust_orm_shim.KERNEL.dispatch(rust_orm_shim._rust_conn(env), request)  # noqa: F821
    except KernelRefused:
        return True
    except AccessError:
        return False
    finally:
        env.cr.rollback()  # noqa: F821
    return False


previous = rust_orm_shim.MODE, rust_orm_shim.SAMPLE
rust_orm_shim.set_sample(0.0)
compared = collections.Counter()
routed_by = collections.Counter()
mismatched = collections.Counter()
example = {}
failures = []
try:
    # seeded on a cursor of its own: one that wrote a security model is never
    # routed, and the comparisons must be
    with env.registry.cursor() as seed_cr:  # noqa: F821
        (company_a, company_b), users, window_opened = seed(env(cr=seed_cr))  # noqa: F821
    # what a request's transaction does once it commits: without it no other
    # cursor, the kernel's included, learns that the security tables moved
    env.registry.signal_changes()  # noqa: F821
    if window_opened:
        time.sleep(3)
    env.cr.rollback()  # noqa: F821
    env.invalidate_all()  # noqa: F821
    rust_orm_shim._ensure_kernel(env)  # noqa: F821
    if rust_orm_shim.KERNEL is None:
        print("GRANT SCOPES FAILED: no kernel is built for this database")
        sys.exit(1)
    contexts = [
        {},
        {"allowed_company_ids": [company_a]},
        {"allowed_company_ids": [company_b]},
        {"allowed_company_ids": [company_a, company_b]},
        {"allowed_company_ids": [company_b, company_a]},
    ]
    cases = [
        (label, uid, ctx, "res.partner", ())
        for label, uid in users.items()
        if label != "privileged"
        for ctx in contexts
    ] + [
        ("privileged", users["privileged"], ctx, "res.partner", privilege)
        for ctx in contexts
        for privilege in ((), (PRIVILEGE,))
    ]
    for label, uid, ctx, name, privilege in cases:
        model = env(user=uid, context=ctx)[name]  # noqa: F821
        if privilege:
            model = model.with_privilege(*privilege)
        for shape, call in shapes(model).items():
            routed_answer, routed = leg("on", call)
            if not routed:
                continue
            python_answer, _ = leg("off", call)
            compared[shape] += 1
            routed_by[label] += 1
            if routed_answer != python_answer:
                key = (label, name, shape, repr(ctx), privilege)
                mismatched[key] += 1
                example.setdefault(key, (str(routed_answer)[:240], str(python_answer)[:240]))
    for label in ("portal scoped", "guard scoped", "dated", "window closed"):
        if not refused_without_python_state(users[label]):
            failures.append(
                "the kernel answered %s without Python's group state" % label
            )
    for label in users:
        if routed_by[label] < MIN_ROUTED:
            failures.append(
                "%s: routed %d calls, fewer than %d" % (label, routed_by[label], MIN_ROUTED)
            )
finally:
    rust_orm_shim.set_mode(previous[0])
    rust_orm_shim.set_sample(previous[1])

print(
    "GRANT SCOPES compared %d: %s; routed per principal %s"
    % (sum(compared.values()), dict(compared), dict(routed_by))
)
by_principal = collections.Counter()
for key, n in mismatched.items():
    by_principal[key[0]] += n
if by_principal:
    print("GRANT SCOPES MISMATCHES per principal %s" % dict(by_principal))
for key, n in mismatched.most_common(20):
    routed_answer, python_answer = example[key]
    print(
        "GRANT SCOPES MISMATCH %d %s\n  routed %s\n  python %s"
        % (n, key, routed_answer, python_answer)
    )
for failure in failures:
    print("GRANT SCOPES FAILED: %s" % failure)
if mismatched:
    print("GRANT SCOPES FAILED: %d mismatching calls" % sum(mismatched.values()))
if mismatched or failures:
    sys.exit(1)
print("GRANT SCOPES OK")
