"""The `reach` web fast-path feature against Python's own resolution.

With the feature, the kernel compiles the record rules of a model whose rows
bind the principal through a `partner` reach (or `own` through a partner
anchor) instead of the shim resolving them in Python on each call. Those
rules walk the commercial partner's tree, which moves without any signal the
kernel's rule cache is keyed on, so every user reads ``res.partner`` and
``res.users`` while that tree changes under it: a contact created under the
commercial partner, one moved into it and one out of it, the commercial
partner itself moved under another, each flushed and still pending in the
ORM, and an acting partner named in the context. Every routed answer must be
Python's.

Three passes: the feature off (Python resolves, the control), the feature on
(the kernel compiles, and must never be handed ``res.partner``'s rules
resolved by Python for a principal it binds), and the feature on with the
hierarchy re-walk withheld, which must disagree somewhere, or the states
above do not exercise what the re-walk is for.
"""

import collections
import json
import sys

import rust_orm_shim

from odoo.exceptions import AccessError, UserError
from odoo.orm.constants import ACTING_PARTNER_KEY

MIN_COMPARED = 300
ROWS = 80


def calls(env, uid, ctx, probe_ids):
    partner = env(user=uid, context=ctx)["res.partner"]
    users = env(user=uid, context=ctx)["res.users"]
    return {
        "search_count": lambda: partner.search_count([]),
        "search_read ids": lambda: partner.search_read([], ["id"], order="id"),
        "search_read parent": lambda: partner.search_read(
            [],
            ["display_name", "parent_id", "commercial_partner_id"],
            order="id",
            limit=ROWS,
        ),
        "web_search_read": lambda: partner.web_search_read(
            [],
            {"display_name": {}, "parent_id": {"fields": {"display_name": {}}}},
            order="id",
            limit=ROWS,
        ),
        "name_search": lambda: partner.name_search("", limit=ROWS),
        "_read_group parent": lambda: [
            (tuple(row[:-1]), row[-1])
            for row in partner._read_group([], ["parent_id"], ["__count"])
        ],
        "web_read probes": lambda: partner.browse(probe_ids()).web_read(
            {"display_name": {}}
        ),
        "res.users search_read": lambda: users.search_read([], ["login"], order="id"),
    }


def scenarios():
    # each one returns the context it adds; `pending` leaves its write in the
    # ORM, unflushed, for the routed leg's own flush to reach the database
    def as_is(_fx, _pending):
        return {}

    def created_under(fx, _pending):
        new = fx.sudo["res.partner"].create({"name": "reach new contact"})
        new.write({"parent_id": fx.commercial})
        fx.probes.append(new.id)
        return {}

    def moved_in(fx, _pending):
        fx.sudo["res.partner"].browse(fx.outside_child).write(
            {"parent_id": fx.commercial}
        )
        return {}

    def moved_out(fx, _pending):
        fx.sudo["res.partner"].browse(fx.kin).write({"parent_id": False})
        return {}

    def commercial_moved(fx, _pending):
        fx.sudo["res.partner"].browse(fx.commercial).write({"parent_id": fx.outside})
        return {}

    def acting(fx, _pending):
        return {ACTING_PARTNER_KEY: (fx.outside_child, fx.outside)}

    def acting_moved_in(fx, _pending):
        fx.sudo["res.partner"].browse(fx.kin).write({"parent_id": fx.outside})
        return {ACTING_PARTNER_KEY: (fx.outside_child, fx.outside)}

    return {
        "as is": (as_is, False),
        "contact created under, flushed": (created_under, False),
        "contact created under, pending": (created_under, True),
        "contact moved in, flushed": (moved_in, False),
        "contact moved in, pending": (moved_in, True),
        "contact moved out, flushed": (moved_out, False),
        "contact moved out, pending": (moved_out, True),
        "commercial partner moved, flushed": (commercial_moved, False),
        "commercial partner moved, pending": (commercial_moved, True),
        "acting partner": (acting, False),
        "acting partner, contact moved in, pending": (acting_moved_in, True),
    }


class Fixture:
    """Partners each leg creates again under the same ids."""

    def __init__(self, env, uid, sequence_start):
        env.cr.execute("SELECT setval('res_partner_id_seq', %s)", [sequence_start])
        self.sudo = env(su=True)
        partners = self.sudo["res.partner"].with_context(active_test=False)
        self.commercial = (
            self.sudo["res.users"].browse(uid).partner_id.commercial_partner_id.id
        )
        outside = partners.create({"name": "reach outside", "is_company": True})
        self.outside = outside.id
        self.outside_child = partners.create(
            {"name": "reach outside contact", "parent_id": outside.id}
        ).id
        self.kin = partners.create(
            {"name": "reach kin", "parent_id": self.commercial}
        ).id
        self.probes = [self.commercial, self.outside, self.outside_child, self.kin]
        env.flush_all()


def one_leg(env, mode, uid, ctx, scenario, pending, shape, sequence_start):
    rust_orm_shim.set_mode(mode)
    routed_before = rust_orm_shim.STATS["kernel"]
    # a write the leg rolls back may have invalidated an ormcache group (the
    # commercial partner moving clears the access domains); what was cached
    # after it is released as a rolled-back request releases it
    snapshot = env.registry.changes_snapshot()
    try:
        fx = Fixture(env, uid, sequence_start)
        extra = scenario(fx, pending)
        if not pending:
            env.flush_all()
        call = calls(env, uid, {**ctx, **extra}, lambda: fx.probes)[shape]
        try:
            answer = call()
        except (AccessError, UserError) as exc:
            answer = type(exc).__name__
    finally:
        env.cr.rollback()
        env.invalidate_all(flush=False)
        env.registry.reset_changes_to(env.cr, snapshot)
    return answer, rust_orm_shim.STATS["kernel"] > routed_before


def contexts(env):
    users = env["res.users"].with_context(active_test=False).search([])
    companies = env["res.company"].search([]).ids
    out = []
    for user in users:
        if user.id == 1:
            continue
        out += [
            (user.id, user.share, {}),
            (user.id, user.share, {"active_test": False}),
        ]
        mine = [c for c in companies if c in user.company_ids.ids]
        if len(mine) > 1:
            out += [
                (user.id, user.share, {"allowed_company_ids": allowed})
                for allowed in ([mine[0]], [mine[-1]], mine, mine[::-1])
            ]
    return out


def run_pass(env, features, withhold_rewalk=False):
    rust_orm_shim.set_web_fastpath(features)
    resolved_partner = collections.Counter()
    original_resolved, original_request = (
        rust_orm_shim._resolved_rules,
        rust_orm_shim._request,
    )

    def resolved_rules(model, kw, encode):
        resolved = original_resolved(model, kw, encode)
        if "res.partner" in resolved and not model.env.su:
            resolved_partner[model.env.uid] += 1
        return resolved

    def request(model, method, **kw):
        sent = original_request(model, method, **kw)
        if withhold_rewalk:
            decoded = json.loads(sent)
            decoded["fresh_hierarchies"] = False
            sent = json.dumps(decoded)
        return sent

    rust_orm_shim._resolved_rules = resolved_rules
    rust_orm_shim._request = request
    compared, routed_share = collections.Counter(), collections.Counter()
    mismatched, example = collections.Counter(), {}
    env.cr.execute("SELECT last_value + 1000 FROM res_partner_id_seq")
    sequence_start = env.cr.fetchone()[0]
    try:
        for uid, share, ctx in contexts(env):
            for name, (scenario, pending) in scenarios().items():
                for shape in calls(env, uid, ctx, list):
                    routed_answer, routed = one_leg(
                        env, "on", uid, ctx, scenario, pending, shape, sequence_start
                    )
                    if not routed:
                        continue
                    python_answer, _ = one_leg(
                        env, "off", uid, ctx, scenario, pending, shape, sequence_start
                    )
                    compared[name] += 1
                    if share:
                        routed_share[name] += 1
                    if routed_answer != python_answer:
                        key = (name, shape, uid, repr(ctx))
                        mismatched[key] += 1
                        example.setdefault(
                            key, (str(routed_answer)[:240], str(python_answer)[:240])
                        )
    finally:
        rust_orm_shim._resolved_rules = original_resolved
        rust_orm_shim._request = original_request
    return compared, routed_share, mismatched, example, resolved_partner


previous = rust_orm_shim.MODE, rust_orm_shim.SAMPLE, rust_orm_shim.WEB_FASTPATH
rust_orm_shim.set_sample(0.0)
failed = []
try:
    base = set(rust_orm_shim.WEB_FASTPATH) - {"reach"}
    passes = [
        ("off", sorted(base), False),
        ("on", sorted(base | {"reach"}), False),
        ("rewalk withheld", sorted(base | {"reach"}), True),
    ]
    results = {}
    for label, features, withhold in passes:
        results[label] = run_pass(env, features, withhold)  # noqa: F821
        compared, routed_share, mismatched, example, resolved = results[label]
        print(
            "PARTNER REACH %s [%s]: compared %d (share users %d), mismatching %d, "
            "res.partner rules resolved in python for uids %s"
            % (
                label,
                ",".join(features) or "-",
                sum(compared.values()),
                sum(routed_share.values()),
                sum(mismatched.values()),
                dict(resolved) or "{}",
            )
        )
        for key, n in mismatched.most_common(8):
            routed_answer, python_answer = example[key]
            print(
                "  %s %d %s\n    routed %s\n    python %s"
                % (
                    "stale" if withhold else "MISMATCH",
                    n,
                    key,
                    routed_answer,
                    python_answer,
                )
            )
    off, on, stale = results["off"], results["on"], results["rewalk withheld"]
    if sum(off[2].values()):
        failed.append("the feature off disagrees with Python")
    if sum(on[2].values()):
        failed.append("the feature on disagrees with Python")
    if on[4]:
        failed.append("the feature on still had Python resolve res.partner's rules")
    if not off[4]:
        failed.append("the feature off never had Python resolve res.partner's rules")
    if sum(on[0].values()) < MIN_COMPARED:
        failed.append("the feature on compared fewer than %d" % MIN_COMPARED)
    unexercised = [name for name in scenarios() if not on[1][name]]
    if unexercised:
        failed.append("no share user routed in: %s" % ", ".join(unexercised))
    if not sum(stale[2].values()):
        failed.append(
            "withholding the re-walk changed no answer: the states do not test it"
        )
finally:
    rust_orm_shim.set_mode(previous[0])
    rust_orm_shim.set_sample(previous[1])
    rust_orm_shim.set_web_fastpath(previous[2])

if failed:
    print("PARTNER REACH FAILED: " + "; ".join(failed))
    sys.exit(1)
print("PARTNER REACH OK")
