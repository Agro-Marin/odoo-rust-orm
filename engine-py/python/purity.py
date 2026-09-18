# Overrides of the read path that touch a named set of fields and nothing
# else. The gate keys on method identity and cannot see what an override
# touches, so without this table one hook on one field -- mail's activity
# groupby on 13 models of a plain database -- refuses every grouped read on the
# model. The export and the shim's gate read ONE table: the export drops the
# scoped fields from the kernel's registry, so naming them refuses, and the
# gate admits the model for everything else. A key is (module, qualname,
# method); a scope is the field names, or a callable of the model returning them.


def _geo_fields(m):
    return tuple(n for n, f in m._fields.items() if f.type.startswith("geo_"))


TRANSPARENT_HOOKS = {
    (
        "odoo.addons.base.models.mixin_properties_base_definition",
        "MixinPropertiesBaseDefinition",
        "_field_to_sql",
    ): ("properties_base_definition_id",),
    ("odoo.addons.base.models.res_device", "ResDeviceLog", "_order_field_to_sql"): (
        "is_current",
    ),
    (
        "odoo.addons.mail.models.mixin_mail_activity",
        "MixinMailActivity",
        "_order_field_to_sql",
    ): ("activity_date_deadline", "my_activity_date_deadline", "activity_state"),
    (
        "odoo.addons.mail.models.mixin_mail_activity",
        "MixinMailActivity",
        "_read_group_groupby",
    ): ("activity_state",),
    (
        "odoo.addons.base.models.mixin_user_favorite",
        "MixinUserFavorite",
        "_order_field_to_sql",
    ): ("is_user_favorite",),
    (
        "odoo.addons.knowledge.models.knowledge_article",
        "KnowledgeArticle",
        "_order_field_to_sql",
    ): ("is_user_favorite",),
    ("odoo.addons.crm.models.crm_lead", "CrmLead", "_field_to_sql"): (
        "company_currency",
    ),
    (
        "odoo.addons.account.models.account_analytic_line_reports",
        "AccountAnalyticLine",
        "_field_to_sql",
    ): ("analytic_coverage",),
    ("odoo.addons.account.models.account_move", "AccountMove", "_field_to_sql"): (
        "display_state",
        "move_sent_values",
    ),
    ("odoo.addons.hr.models.hr_employee", "HrEmployee", "_field_to_sql"): (
        "version_id",
    ),
    (
        "odoo.addons.document.models.document_document_search_panel",
        "DocumentsDocument",
        "_field_to_sql",
    ): ("last_access_date_group",),
    (
        "odoo.addons.document.models.document_document_search_panel",
        "DocumentsDocument",
        "_order_field_to_sql",
    ): ("last_access_date_group",),
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
    (
        "odoo.addons.analytic.models.analytic_account",
        "AccountAnalyticAccount",
        "_read_group_select",
    ): ("balance", "debit", "credit"),
    (
        "odoo.addons.analytic.models.analytic_account",
        "AccountAnalyticAccount",
        "_read_group_postprocess_aggregate",
    ): ("balance", "debit", "credit"),
    # geoengine patches every model: the two hooks act only on geo_* aggregate
    # functions, which the kernel refuses as unsupported, so a geometry field is
    # the whole surface they touch
    ("odoo.addons.geoengine.models.base", "Base", "_read_group_select"): _geo_fields,
    (
        "odoo.addons.geoengine.models.base",
        "Base",
        "_read_group_postprocess_aggregate",
    ): _geo_fields,
    # res.groups sorts in Python only for an order on full_name; hr_appraisal
    # narrows hr.employee only when the domain names next_appraisal_date
    ("odoo.addons.base.models.res_groups", "ResGroups", "_search"): ("full_name",),
    ("odoo.addons.hr_appraisal.models.hr_employee", "HrEmployee", "_search"): (
        "next_appraisal_date",
    ),
    ("odoo.addons.hr_appraisal.models.hr_employee", "HrEmployee", "fetch"): (
        "next_appraisal_date",
    ),
}


def override_keys(cls, base, name):
    """The (module, qualname, method) of every class in `cls`'s MRO outside
    `base`'s that defines `name`; empty when nothing overrides it."""
    if getattr(cls, name, None) is getattr(base, name, None):
        return []
    return [
        (k.__module__, k.__qualname__, name)
        for k in cls.__mro__
        if name in vars(k) and k not in base.__mro__
    ]


def transparent(cls, base, name):
    """(pure, hooked): whether every override of `name` on `cls` is in the
    table, and the union of the fields those overrides touch."""
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
