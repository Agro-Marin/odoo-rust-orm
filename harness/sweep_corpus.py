import json
import os

OUT = os.environ.get("RUSTORM_SWEEP_OUT", "/tmp/rustorm_sweep_corpus.json")
SEED = os.environ.get("RUSTORM_SWEEP_SEED", "1") not in ("0", "", "no")
PER_MODEL = int(os.environ.get("RUSTORM_SWEEP_PER_MODEL", "11"))
TAG = "RUSTORM SWEEP"


def seed(env):
    su = env(user=1, su=True)
    Partner = su["res.partner"]
    Category = su["res.partner.tag"]

    active = Category.with_context(active_test=False).search(
        [("name", "=", TAG + " active")]
    )
    if not active:
        active = Category.create({"name": TAG + " active"})
    archived = Category.with_context(active_test=False).search(
        [("name", "=", TAG + " archived")]
    )
    if not archived:
        archived = Category.create({"name": TAG + " archived"})
    if archived.active:
        archived.active = False
    tagged = Partner.with_context(active_test=False).search([("name", "=", TAG + " tagged")])
    if not tagged:
        tagged = Partner.create({"name": TAG + " tagged"})
    tagged.tag_ids = [(6, 0, (active + archived).ids)]

    group = su.ref("base.group_portal", raise_if_not_found=False)
    if group is None:
        return None
    user = su["res.users"].with_context(active_test=False).search(
        [("login", "=", "rustorm_sweep_probe")]
    )
    if not user:
        user = su["res.users"].create(
            {
                "login": "rustorm_sweep_probe",
                "name": TAG + " probe",
                "group_ids": [(6, 0, [group.id])],
            }
        )
    if not user.active:
        user.active = True
    return user.id


def probes(model, uid):
    fields = model._fields
    out = []

    def add(**kw):
        kw["model"] = model._name
        if uid is not None:
            kw["uid"] = uid
        out.append(kw)

    def stored(f):
        return f.store and f.name != "id"

    scalars = sorted(
        (f.name for f in fields.values() if stored(f) and f.type in ("char", "boolean", "integer", "date", "datetime", "selection")),
    )[:2]
    m2o = sorted(f.name for f in fields.values() if stored(f) and f.type == "many2one")[:2]
    x2m = sorted(
        f.name
        for f in fields.values()
        if f.store and f.type in ("one2many", "many2many") and f.comodel_name in model.env.registry
    )[:1]

    read = scalars + m2o + x2m
    if read:
        add(method="search_read", fields=read, domain=[], limit=5)
    for name in m2o:
        add(method="read_group", groupby=[name], aggregates=["__count"], domain=[])
    if m2o:
        add(method="search_read", fields=["id"], domain=[[m2o[0], "ilike", "a"]], limit=5)
        add(method="search_read", fields=["id"], domain=[[m2o[0], "not ilike", "a"]], limit=5)
        add(method="search_read", fields=["id"], domain=[[m2o[0], "ilike", ""]], limit=5)
    if model._rec_name:
        add(method="search_read", fields=["id"], domain=[["display_name", "ilike", "a"]], limit=5)
        add(method="search_read", fields=["id"], domain=[["display_name", "not ilike", "a"]], limit=5)
    extra = []

    def add_extra(**kw):
        kw["model"] = model._name
        if uid is not None:
            kw["uid"] = uid
        extra.append(kw)

    parent_field = getattr(model, "_parent_name", None)
    if parent_field and parent_field in fields:
        try:
            roots = model.search([(parent_field, "=", False)], limit=1).ids
            children = model.search([(parent_field, "!=", False)], limit=1).ids
            seeds = [i for i in (roots + children) if i]
        except Exception:  # noqa: BLE001  an unreadable model is not this file's problem
            seeds = []
        for seed in seeds[:2]:
            for op in ("child_of", "parent_of"):
                add_extra(
                    method="search_read",
                    fields=["id"],
                    domain=[["id", op, seed]],
                    limit=20,
                )
        if len(seeds) > 1:
            add_extra(
                method="search_read",
                fields=["id"],
                domain=[["id", "child_of", seeds]],
                limit=20,
            )
            add_extra(method="search_count", domain=[["id", "parent_of", seeds]])

    numeric = sorted(
        f.name
        for f in fields.values()
        if stored(f) and f.type in ("integer", "float", "monetary")
    )[:1]
    groupable = sorted(
        f.name
        for f in fields.values()
        if stored(f) and f.type in ("char", "boolean", "integer", "selection")
    )[:1]
    if numeric and groupable:
        add_extra(
            method="read_group",
            groupby=[groupable[0]],
            aggregates=["%s:sum" % numeric[0], "%s:avg" % numeric[0],
                        "%s:min" % numeric[0], "%s:max" % numeric[0]],
            domain=[],
        )
    if m2o and groupable:
        add_extra(
            method="read_group",
            groupby=[groupable[0]],
            aggregates=["%s:count_distinct" % m2o[0]],
            domain=[],
        )

    if x2m:
        add(method="search_read", fields=["id"], domain=[[x2m[0], "!=", False]], limit=5)
        add(
            method="search_read",
            fields=["id"],
            domain=[[x2m[0] + ".id", ">", 0]],
            limit=5,
        )
    add(method="search_count", domain=[], limit=3)
    return out[:PER_MODEL] + extra


def main(env):
    portal = seed(env) if SEED else None
    if SEED:
        env.cr.commit()
    base = env(user=2, su=False)
    cases = []
    identities = [None] + ([portal] if portal else [])
    for name in sorted(base.registry):
        model = base.get(name)
        if model is None or model._abstract or model._transient or not model._auto:
            continue
        for uid in identities:
            for case in probes(model, uid):
                case["id"] = "s%04d" % len(cases)
                cases.append(case)
    with open(OUT, "w") as f:
        json.dump(cases, f, indent=1)
    print(
        "wrote %d cases to %s (%d models, identities: %s)"
        % (len(cases), OUT, len(base.registry), identities)
    )


main(env)  # noqa: F821  (env comes from the odoo shell namespace)
