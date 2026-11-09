def _geo_fields(m):
    return tuple(n for n, f in m._fields.items() if f.type.startswith("geo_"))


TRANSPARENT_HOOKS = {
    (
        "odoo.addons.analytic.models.mixin_analytic",
        "MixinAnalytic",
        "_read_group_groupby",
    ): ("analytic_distribution",),
    (
        "odoo.addons.analytic.models.mixin_analytic",
        "MixinAnalytic",
        "_read_group_select",
    ): ("analytic_distribution",),
    ("odoo.addons.geoengine.models.base", "Base", "_read_group_select"): _geo_fields,
    (
        "odoo.addons.geoengine.models.base",
        "Base",
        "_read_group_postprocess_aggregate",
    ): _geo_fields,
    ("odoo.addons.base.models.res_groups", "ResGroups", "_search"): ("full_name",),
    ("odoo.addons.hr_appraisal.models.hr_employee", "HrEmployee", "_search"): (
        "next_appraisal_date",
    ),
    ("odoo.addons.hr_appraisal.models.hr_employee", "HrEmployee", "fetch"): (
        "next_appraisal_date",
    ),
}


def _passes_through(member, owner, name):
    from odoo.orm.delegation import DOOR_ORIGIN

    origin = getattr(member, DOOR_ORIGIN, None)
    if origin is None:
        return False
    # an operation door over no method of its own class calls the next class's,
    # which this walk judges where it is defined
    if origin is getattr(super(owner, owner), name, None):
        return True
    # the door keeps the method it found when the registry was set up: once the shim
    # patches BaseModel after that (a registry loaded before install(), as the registry
    # sweep does) the identity no longer holds, while a door that wraps nothing of its
    # own class (no inner) still calls the next class's at call time
    code = getattr(member, "__code__", None)
    if code is None or "inner" not in code.co_freevars:
        return False
    return member.__closure__[code.co_freevars.index("inner")].cell_contents is None


def override_keys(cls, base, name):
    if getattr(cls, name, None) is getattr(base, name, None):
        return []
    return [
        (k.__module__, k.__qualname__, name)
        for k in cls.__mro__
        if name in vars(k)
        and k not in base.__mro__
        and not _passes_through(vars(k)[name], k, name)
    ]


def transparent(cls, base, name):
    keys = override_keys(cls, base, name)
    if any(key not in TRANSPARENT_HOOKS for key in keys):
        return False, ()
    return True, tuple(key for key in keys)


def hooked_fields(model, cls, base, name):
    fields = set()
    for key in override_keys(cls, base, name):
        scope = TRANSPARENT_HOOKS.get(key)
        if scope is None:
            continue
        fields.update(scope(model) if callable(scope) else scope)
    return fields
