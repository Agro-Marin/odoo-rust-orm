import json
import os
import pathlib
import sys

_HERE = (
    str(pathlib.Path(__file__).resolve().parent) if "__file__" in globals() else None
)
sys.path.insert(
    0,
    os.environ.get("RUSTORM_HARNESS")
    or _HERE
    or os.path.join(
        os.environ.get("RUSTORM_WORKSPACE") or pathlib.Path("~/Odoo").expanduser(),
        "odoo-rust-orm",
        "harness",
    ),
)
import pathlib

from _env import base_env, out_path

OUT = out_path("sweep_corpus.json", "RUSTORM_SWEEP_OUT")
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
    tagged = Partner.with_context(active_test=False).search(
        [("name", "=", TAG + " tagged")]
    )
    if not tagged:
        tagged = Partner.create({"name": TAG + " tagged"})
    tagged.tag_ids = [(6, 0, (active + archived).ids)]

    group = su.ref("base.group_portal", raise_if_not_found=False)
    if group is None:
        return {}
    user = (
        su["res.users"]
        .with_context(active_test=False)
        .search([("login", "=", "rustorm_sweep_probe")])
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

    Rule = su["ir.rule"]
    IM = su["ir.model"]
    for name, model, dom in (
        (
            TAG + " states via any country",
            "res.country.state",
            "[('country_id', 'any', [('code', '!=', 'ZZ')])]",
        ),
        (
            TAG + " countries US/MX only",
            "res.country",
            "[('code', 'in', ['US', 'MX'])]",
        ),
    ):
        if not Rule.search([("name", "=", name)]):
            Rule.create(
                {
                    "name": name,
                    "model_id": IM._get_id(model),
                    "domain_force": dom,
                    "groups": [(6, 0, [group.id])],
                    "perm_read": True,
                    "perm_write": False,
                    "perm_create": False,
                    "perm_unlink": False,
                }
            )
    Template = su["mail.template"]
    if Template._table and "subject" in Template._fields:
        blank = Template.with_context(active_test=False).search(
            [("subject", "=", TAG + " blank name")]
        )
        if not blank:
            blank = Template.create(
                {
                    "subject": TAG + " blank name",
                    "name": "tmp",
                    "model_id": IM._get_id("res.partner"),
                }
            )
        env.cr.execute(
            "UPDATE mail_template SET name = NULL WHERE id = %s", (blank.id,)
        )
    Tag = su["res.partner.tag"] if "res.partner.tag" in su.registry else None
    if Tag is not None:
        names = [TAG + " tz a", TAG + " tz b", TAG + " tz c"]
        tags = Tag.search([("name", "in", names)])
        if len(tags) < 3:
            tags = Tag.create([{"name": n} for n in names])
        for tag, stamp in zip(
            tags.sorted("id"),
            (
                "2026-03-01 03:00:00",
                "2026-03-01 09:00:00",
                "2026-02-28 23:30:00",
            ),
            strict=False,
        ):
            env.cr.execute(
                "UPDATE %s SET create_date = %%s WHERE id = %%s" % Tag._table,
                (stamp, tag.id),
            )
    # a two-level hierarchy on a model whose display name is its name column,
    # for child_of / parent_of seeded by a NAME (corpus h0* cases)
    Cat = su["ir.module.category"]
    root = Cat.search([("name", "=", "Rustorm Root Cat")]) or Cat.create(
        {"name": "Rustorm Root Cat"}
    )
    if not Cat.search([("name", "=", "Rustorm Kid Cat")]):
        Cat.create({"name": "Rustorm Kid Cat", "parent_id": root.id})
    seed_lang_and_fields(su)
    debug = seed_debug_user(su)
    identities = {"portal": user.id}
    if debug:
        identities["debug"] = debug
    grouped = seed_grouped_user(su, IM, Rule)
    if grouped:
        identities["grouped"] = grouped
    archived_co = seed_archived_company_user(env, su)
    if archived_co:
        identities["archived_company"] = archived_co
    return identities


def seed_lang_and_fields(su) -> None:
    # en_US stays active beside whatever --load-language installed: the web
    # tours and TestIrModelFieldsTranslation expect English labels, and a
    # kernel that must accept an inactive en_US is exercised by lang cases
    # naming a language the database lacks
    en = (
        su["res.lang"]
        .with_context(active_test=False)
        .search([("code", "=", "en_US")], limit=1)
    )
    if en and not en.active:
        en.active = True
    # French is installed for the translated-column cases, but the database's
    # default language stays en_US, so a fixture built plain (no
    # --load-language) still compares French terms and the web tours read
    # English labels
    fr = (
        su["res.lang"]
        .with_context(active_test=False)
        .search([("code", "=", "fr_FR")], limit=1)
    )
    if fr and not fr.active:
        su["res.lang"]._activate_lang("fr_FR")
        installed = su["ir.module.module"].search([("state", "=", "installed")])
        installed._update_translations("fr_FR")
    Fields = su["ir.model.fields"]
    country_model = su["ir.model"].search([("model", "=", "res.country")], limit=1)
    specs = (
        ("x_rustorm_secret", "char", "base.group_system", {}),
        ("x_rustorm_debug_only", "char", "base.group_no_one", {}),
        (
            "x_rustorm_secret_currency_id",
            "many2one",
            "base.group_system",
            {"relation": "res.currency"},
        ),
    )
    for name, ttype, group, extra in specs:
        if Fields.search([("model", "=", "res.country"), ("name", "=", name)], limit=1):
            continue
        g = su.ref(group)
        Fields.create(
            dict(
                {
                    "name": name,
                    "ttype": ttype,
                    "model_id": country_model.id,
                    "field_description": name,
                    "store": True,
                    "groups": [(6, 0, [g.id])],
                },
                **extra,
            )
        )
    be, us, usd = su.ref("base.be"), su.ref("base.us"), su.ref("base.USD")
    su.cr.execute(
        "UPDATE res_country SET x_rustorm_secret = 'sesame', x_rustorm_debug_only = 'dbg', "
        "x_rustorm_secret_currency_id = %s WHERE id = %s",
        (usd.id, be.id),
    )
    su.cr.execute(
        "UPDATE res_country SET x_rustorm_secret = 'other', x_rustorm_debug_only = 'dbg2' WHERE id = %s",
        (us.id,),
    )


def seed_debug_user(su):
    internal = su.ref("base.group_user", raise_if_not_found=False)
    no_one = su.ref("base.group_no_one", raise_if_not_found=False)
    if internal is None or no_one is None:
        return None
    Users = su["res.users"].with_context(active_test=False)
    user = Users.search([("login", "=", "rustorm_sweep_debug")])
    if not user:
        user = Users.create(
            {
                "login": "rustorm_sweep_debug",
                "name": TAG + " debug",
                "group_ids": [(6, 0, [internal.id, no_one.id])],
            }
        )
    if not user.active:
        user.active = True
    return user.id


def seed_grouped_user(su, IM, Rule):
    Group = su["res.groups"]
    internal = su.ref("base.group_user", raise_if_not_found=False)
    if internal is None:
        return None
    groups = []
    for suffix in ("alpha", "beta"):
        name = TAG + " group " + suffix
        group = Group.search([("name", "=", name)], limit=1)
        if not group:
            group = Group.create({"name": name})
        groups.append(group)
    # several top-level terms per rule (an implicit AND, and an explicit OR
    # of three), a grant rule and a restrict rule on the same model, so a
    # compiled rule set has more than one leaf per group to get wrong
    rules = (
        (
            TAG + " partners alpha grant",
            "res.partner",
            groups[0],
            "grant",
            "[('is_company', '=', False), ('name', '!=', False), ('id', '>', 0)]",
        ),
        (
            TAG + " partners beta grant",
            "res.partner",
            groups[1],
            "grant",
            "['|', '|', ('is_company', '=', True), ('name', 'ilike', 'RUSTORM'), ('parent_id', '!=', False)]",
        ),
        (
            TAG + " partners beta restrict",
            "res.partner",
            groups[1],
            "restrict",
            "[('active', '=', True), ('name', 'not ilike', 'RUSTORM SWEEP restricted'), ('type', '!=', 'other')]",
        ),
        (
            TAG + " countries alpha grant",
            "res.country",
            groups[0],
            "grant",
            "[('code', '!=', False), ('code', 'not in', ['ZZ', 'ZY']), ('name', '!=', 'nowhere')]",
        ),
        (
            TAG + " countries alpha restrict",
            "res.country",
            groups[0],
            "restrict",
            "[('code', 'not in', ['US']), ('id', '>', 0)]",
        ),
    )
    for name, model, group, composition, dom in rules:
        if Rule.search([("name", "=", name)]):
            continue
        vals = {
            "name": name,
            "model_id": IM._get_id(model),
            "domain_force": dom,
            "groups": [(6, 0, [group.id])],
            "perm_read": True,
            "perm_write": False,
            "perm_create": False,
            "perm_unlink": False,
        }
        if "composition" in Rule._fields:
            vals["composition"] = composition
        Rule.create(vals)
    Users = su["res.users"].with_context(active_test=False)
    user = Users.search([("login", "=", "rustorm_sweep_grouped")])
    if not user:
        user = Users.create(
            {
                "login": "rustorm_sweep_grouped",
                "name": TAG + " grouped",
                "group_ids": [(6, 0, [internal.id] + [g.id for g in groups])],
            }
        )
    if not user.active:
        user.active = True
    # a zone for the day-boundary cases: env.tz falls back to the user's
    if user.partner_id.tz != "America/Mexico_City":
        user.partner_id.tz = "America/Mexico_City"
    return user.id


def seed_archived_company_user(env, su):
    Company = su["res.company"].with_context(active_test=False)
    Users = su["res.users"].with_context(active_test=False)
    company = Company.search([("name", "=", TAG + " archived co")], limit=1)
    if not company:
        company = Company.create({"name": TAG + " archived co"})
    user = Users.search([("login", "=", "rustorm_sweep_archived_co")])
    if not user:
        user = Users.create(
            {
                "login": "rustorm_sweep_archived_co",
                "name": TAG + " archived-company user",
                "company_id": company.id,
                "company_ids": [(6, 0, [company.id])],
            }
        )
    if company.active:
        try:
            with env.cr.savepoint():
                company.active = False
                company.flush_recordset()
        except Exception as exc:
            print(
                "archived-company identity skipped: Odoo refuses to archive %r: %s: %s"
                % (company.name, type(exc).__name__, str(exc).replace("\n", " ")[:200])
            )
            return None
    return user.id


def probes(model, uid):
    fields = model._fields
    out = []

    def add(**kw) -> None:
        kw["model"] = model._name
        if uid is not None:
            kw["uid"] = uid
        if (
            kw.get("method") == "search_read"
            and kw.get("limit")
            and not kw.get("order")
        ):
            terms = [t.strip().split()[0] for t in model._order.split(",") if t.strip()]
            kw["order"] = model._order if "id" in terms else model._order + ", id"
        out.append(kw)

    def stored(f):
        return f.store and f.name != "id"

    scalars = sorted(
        (
            f.name
            for f in fields.values()
            if stored(f)
            and f.type
            in ("char", "boolean", "integer", "date", "datetime", "selection")
        ),
    )[:2]
    m2o = sorted(f.name for f in fields.values() if stored(f) and f.type == "many2one")[
        :2
    ]
    x2m = sorted(
        f.name
        for f in fields.values()
        if f.store
        and f.type in ("one2many", "many2many")
        and f.comodel_name in model.env.registry
    )[:1]

    read = scalars + m2o + x2m
    if read:
        add(method="search_read", fields=read, domain=[], limit=5)
    for name in m2o:
        add(method="read_group", groupby=[name], aggregates=["__count"], domain=[])
    if m2o:
        add(
            method="search_read",
            fields=["id"],
            domain=[[m2o[0], "ilike", "a"]],
            limit=5,
        )
        add(
            method="search_read",
            fields=["id"],
            domain=[[m2o[0], "not ilike", "a"]],
            limit=5,
        )
        add(
            method="search_read", fields=["id"], domain=[[m2o[0], "ilike", ""]], limit=5
        )
    if model._rec_name:
        add(
            method="search_read",
            fields=["id"],
            domain=[["display_name", "ilike", "a"]],
            limit=5,
        )
        add(
            method="search_read",
            fields=["id"],
            domain=[["display_name", "not ilike", "a"]],
            limit=5,
        )
        add(method="search_count", domain=[["display_name", "!=", False]])
        add(method="search_count", domain=[["display_name", "in", ["a", False]]])
    chars = sorted(f.name for f in fields.values() if stored(f) and f.type == "char")[
        :1
    ]
    for c in chars:
        add(method="search_count", domain=[[c, "like", ""]])
        add(method="search_count", domain=[[c, "not like", ""]])
        add(method="search_count", domain=[[c, "=like", ""]])
        add(method="search_count", domain=[[c, "like", "%"]])
    nums = sorted(
        f.name
        for f in fields.values()
        if stored(f) and f.type in ("integer", "float", "monetary")
    )[:1]
    for n in nums:
        add(method="search_count", domain=[[n, "=?", 0]])
        add(method="search_count", domain=[[n, ">", False]])
        add(method="search_count", domain=[[n, "=", "1"]])
    for m in m2o[:1]:
        add(method="search_count", domain=[[m, "=", 0]])
        add(method="search_count", domain=[[m, "not ilike", ""]])
    for x in x2m:
        add(method="search_count", domain=[[x, "in", [0]]])
        add(method="search_count", domain=[[x, "=", False]])
    dts = sorted(f.name for f in fields.values() if stored(f) and f.type == "datetime")[
        :1
    ]
    for d in dts:
        add(
            method="read_group",
            groupby=[d + ":month"],
            aggregates=["__count"],
            domain=[],
            tz="America/Mexico_City",
        )
        add(
            method="read_group",
            groupby=[d + ":week"],
            aggregates=["__count"],
            domain=[],
            lang="en_US",
            tz="America/Mexico_City",
        )
        add(
            method="read_group",
            groupby=[d + ":day_of_week"],
            aggregates=["__count"],
            domain=[],
            lang="en_US",
        )
    extra = []

    def add_extra(**kw) -> None:
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
        except Exception:
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
            aggregates=[
                "%s:sum" % numeric[0],
                "%s:avg" % numeric[0],
                "%s:min" % numeric[0],
                "%s:max" % numeric[0],
            ],
            domain=[],
        )
    if m2o and groupable:
        add_extra(
            method="read_group",
            groupby=[groupable[0]],
            aggregates=["%s:count_distinct" % m2o[0]],
            domain=[],
        )

    # Comparing against an unset value is decided by the field's
    # `falsy_value`, which Odoo declares on the field CLASS and not on its
    # field type: `id` is a `fields.Id` and has none where every other integer has
    # 0, and `many2one_reference` has 0 where its relational siblings have
    # none. Nothing else in this corpus compares against False with an
    # ordering operator, so the whole family went unmeasured -- and a
    # divergence in it returns wrong rows rather than refusing. Admin only,
    # and one field per branch, because this doubles otherwise.
    if uid is None:
        with_falsy = sorted(
            f.name
            for f in fields.values()
            if stored(f) and f.type in ("char", "text", "integer", "float", "monetary")
        )[:1]
        without_falsy = sorted(
            f.name
            for f in fields.values()
            if stored(f) and f.type in ("date", "datetime", "selection", "many2one")
        )[:1]
        for name in with_falsy:
            for op in (">", ">="):
                add_extra(method="search_count", domain=[[name, op, False]])
        for name in without_falsy:
            add_extra(method="search_count", domain=[[name, ">", False]])
        for f in sorted(fields.values(), key=lambda f: f.name):
            if stored(f) and f.type == "many2one_reference":
                for op in ("=", "!=", ">="):
                    add_extra(method="search_count", domain=[[f.name, op, False]])
        add_extra(method="search_count", domain=[["id", ">", False]])

    # A traversal THROUGH a field that declares `bypass_search_access`, which
    # Odoo evaluates with the comodel's ACL and record rules turned off. The
    # kernel applied them anyway until 2026-09-08 and answered with fewer
    # rows; nothing in this corpus traversed such a field, so nothing saw it.
    # Both identities, because the difference only exists for a non-superuser.
    bypassing = sorted(
        f.name
        for f in fields.values()
        if getattr(f, "bypass_search_access", False)
        and f.type in ("many2one", "one2many", "many2many")
        and f.comodel_name in model.env.registry
    )[:1]
    for name in bypassing:
        add_extra(method="search_count", domain=[["%s.id" % name, ">", 0]])

    # `binary` was the one field type in the registry that NO lane touched --
    # 127 fields, exercised by nothing. Auditing it found no defect (a filter
    # agrees with Python, and READING one is refused so the shim falls back),
    # which is the outcome to hope for and not the reason to have looked.
    # Only the filter is probed: the read refuses by design and would add a
    # classified non-comparison rather than coverage.
    binary = sorted(
        f.name
        for f in fields.values()
        if stored(f) and f.type == "binary" and not getattr(f, "attachment", False)
    )[:1]
    for name in binary:
        add_extra(method="search_count", domain=[[name, "=", False]])
        add_extra(method="search_count", domain=[[name, "!=", False]])

    if x2m:
        add_extra(
            method="search_read", fields=["id"], domain=[[x2m[0], "!=", False]], limit=5
        )
        add_extra(
            method="search_read",
            fields=["id"],
            domain=[[x2m[0] + ".id", ">", 0]],
            limit=5,
        )
    add_extra(method="search_count", domain=[], limit=3)
    return out[:PER_MODEL] + extra


def main(env) -> None:
    seeded = seed(env) if SEED else {}
    if SEED:
        env.cr.commit()
    base = base_env(env)
    cases = []
    identities = [None] + [uid for _, uid in sorted(seeded.items())]
    for name in sorted(base.registry):
        model = base.get(name)
        if model is None or model._abstract or model._transient or not model._auto:
            continue
        for uid in identities:
            for case in probes(model, uid):
                case["id"] = "s%04d" % len(cases)
                cases.append(case)
    with pathlib.Path(OUT).open("w", encoding="utf-8") as f:
        json.dump(cases, f, indent=1)
    print(
        "wrote %d cases to %s (%d models, identities: %s)"
        % (len(cases), OUT, len(base.registry), dict(seeded, admin=None))
    )


main(env)  # noqa: F821
