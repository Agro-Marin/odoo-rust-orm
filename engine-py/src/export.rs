use anyhow::Result;
use pyo3::prelude::*;
use pyo3::types::PyList;

pub fn venv_site() -> String {
    odoo_kernel::config::venv_site().display().to_string()
}

pub fn odoo_root() -> String {
    odoo_kernel::config::odoo_root().display().to_string()
}

const EXPORT_ALL: &str = r#"
import json

def _s(v):
    return v if isinstance(v, (str, int, float, bool)) or v is None else None

def _cd_fallback(model, f):
    if not getattr(f, "company_dependent", False):
        return None
    try:
        v = f.get_company_dependent_fallback(model)
    except Exception:
        return None
    if hasattr(v, "_name"):
        return v.id or None
    return _s(v)

def _geo_fields(m):
    return tuple(n for n, f in m._fields.items() if f.type.startswith("geo_"))


TRANSPARENT_HOOKS = {
    ("odoo.addons.base.models.mixin_properties_base_definition",
     "MixinPropertiesBaseDefinition", "_field_to_sql"):
        ("properties_base_definition_id",),
    ("odoo.addons.base.models.res_device", "ResDeviceLog", "_order_field_to_sql"):
        ("is_current",),
    ("odoo.addons.mail.models.mixin_mail_activity", "MixinMailActivity",
     "_order_field_to_sql"):
        ("activity_date_deadline", "my_activity_date_deadline", "activity_state"),
    ("odoo.addons.mail.models.mixin_mail_activity", "MixinMailActivity",
     "_read_group_groupby"):
        ("activity_state",),
    ("odoo.addons.base.models.mixin_user_favorite", "MixinUserFavorite",
     "_order_field_to_sql"):
        ("is_user_favorite",),
    ("odoo.addons.crm.models.crm_lead", "CrmLead", "_field_to_sql"):
        ("company_currency",),
    ("odoo.addons.hr.models.hr_employee", "HrEmployee", "_field_to_sql"):
        ("version_id",),
    ("odoo.addons.document.models.document_document_search_panel",
     "DocumentsDocument", "_field_to_sql"):
        ("last_access_date_group",),
    ("odoo.addons.document.models.document_document_search_panel",
     "DocumentsDocument", "_order_field_to_sql"):
        ("last_access_date_group",),
    ("odoo.addons.analytic.models.mixin_analytic", "MixinAnalytic",
     "_read_group_groupby"):
        ("analytic_distribution",),
    ("odoo.addons.analytic.models.mixin_analytic", "MixinAnalytic",
     "_read_group_select"):
        ("analytic_distribution",),
    ("odoo.addons.analytic.models.analytic_account", "AccountAnalyticAccount",
     "_read_group_select"):
        ("balance", "debit", "credit"),
    ("odoo.addons.analytic.models.analytic_account", "AccountAnalyticAccount",
     "_read_group_postprocess_aggregate"):
        ("balance", "debit", "credit"),
    ("odoo.addons.project.models.project_task", "ProjectTask", "_read_group"):
        ("triage_id",),
    # geoengine patches every model: the two hooks act only on geo_* aggregate
    # functions, which the kernel refuses as unsupported, so a geometry field is
    # the whole surface they touch
    ("odoo.addons.geoengine.models.base", "Base", "_read_group_select"):
        _geo_fields,
    ("odoo.addons.geoengine.models.base", "Base", "_read_group_postprocess_aggregate"):
        _geo_fields,
    # res.groups sorts in Python only for an order on full_name; hr_appraisal
    # narrows hr.employee only when the domain names next_appraisal_date
    ("odoo.addons.base.models.res_groups", "ResGroups", "_search"):
        ("full_name",),
    ("odoo.addons.hr_appraisal.models.hr_employee", "HrEmployee", "_search"):
        ("next_appraisal_date",),
    ("odoo.addons.hr_appraisal.models.hr_employee", "HrEmployee", "fetch"):
        ("next_appraisal_date",),
}


def export_registry(reg):
    import odoo.api
    import odoo.orm.models.base
    from odoo.fields import Domain
    models = {}
    with reg.cursor() as cr:
      env = odoo.api.Environment(cr, 1, {})
      for name in reg.models:
        m = env[name]
        if m._abstract:
            continue
        fields = {}
        for fname, f in m._fields.items():
            related = f.related
            if related and not isinstance(related, str):
                related = ".".join(related)
            domain = getattr(f, "domain", None)
            if callable(domain):
                domain = {"__callable__": True}
            elif isinstance(domain, str):
                domain = None
            elif domain:
                domain = list(Domain(domain)) if not isinstance(domain, list) else domain
            else:
                domain = None
            fields[fname] = {
                "type": f.type,
                "store": bool(f.store),
                "relation": _s(getattr(f, "comodel_name", None)),
                "related": _s(related),
                "company_dependent": bool(getattr(f, "company_dependent", False)),
                "relation_table": _s(getattr(f, "relation", None)) if f.type == "many2many" else None,
                "column1": _s(getattr(f, "column1", None)) if f.type == "many2many" else None,
                "column2": _s(getattr(f, "column2", None)) if f.type == "many2many" else None,
                "inverse_name": _s(getattr(f, "inverse_name", None)) if f.type == "one2many" else None,
                "domain": domain,
                "model_field": _s(getattr(f, "model_field", None)),
                "custom_search": bool(getattr(f, "search", None)),
                "groups": _s(getattr(f, "groups", None)),
                "context": (
                    dict(f.context)
                    if isinstance(getattr(f, "context", None), dict) and f.context
                    else ({"__callable__": True} if callable(getattr(f, "context", None)) else None)
                ),
                "index": _s(getattr(f, "index", None)),
                "company_dependent_fallback": _cd_fallback(m, f),
                "falsy_value": _s(getattr(f, "falsy_value", None)),
                "bypass_search_access": bool(getattr(f, "bypass_search_access", False)),
                "compute_sudo": bool(getattr(f, "compute_sudo", False)),
                "required": bool(getattr(f, "required", False)),
            }
        cls = type(m.sudo())
        base = odoo.orm.models.base.BaseModel
        hooked = set()

        def pure(*names):
            for n in names:
                if getattr(cls, n, None) is getattr(base, n, None):
                    continue
                owner = next((k for k in cls.__mro__ if n in vars(k)), None)
                key = (owner.__module__, owner.__qualname__, n) if owner else None
                if key in TRANSPARENT_HOOKS:
                    scope = TRANSPARENT_HOOKS[key]
                    hooked.update(scope(m) if callable(scope) else scope)
                    continue
                return False
            return True
        search_pure = pure("_search")
        impure_read_methods = [
            n
            for n in (
                "_search", "read", "search_read", "_read_group", "search_count",
                "search", "search_fetch", "fetch", "_fetch_query", "_field_to_sql",
            )
            if not pure(n)
        ]
        read_path_pure = not impure_read_methods
        order_pure = pure("_order_to_sql", "_order_field_to_sql")
        read_group_pure = pure(
            "_read_group_select", "_read_group_groupby", "_read_group_orderby",
            "_read_group_having", "_read_group_postprocess_aggregate",
            "_read_group_postprocess_groupby", "_read_group_empty_value",
        )
        display_name_access_pure = pure("_get_display_name_visible_ids")
        dn_default = getattr(cls, "_compute_display_name", None) is getattr(
            base, "_compute_display_name", None
        )
        if dn_default:
            dnf = m._fields.get("display_name")
            dn_default = getattr(dnf, "compute", None) in (None, "_compute_display_name")
        if dn_default and m._rec_name:
            rf = m._fields.get(m._rec_name)
            dn_default = rf is not None and bool(rf.store or rf.related)
        ns = None
        exact = list(getattr(cls, "_display_name_search_exact", ()) or ())
        if getattr(cls, "_search_display_name", None) is getattr(
            base, "_search_display_name", None
        ) or getattr(cls, "_display_name_search_default", False) or exact:
            rns = m._rec_names_search
            if callable(rns):
                rns = None
            fnames = list(rns or ([m._rec_name] if m._rec_name else []))
            fnames = [f for f in fnames if not m._is_rec_names_search_cyclic(f)]
            fnames = [f for f in fnames if f != "display_name"]
            # a stored scalar column, a non-stored related field (compiled the
            # way _search_related composes it) or a stored many2one (a name
            # search on its comodel); the kernel refuses at compile time what
            # the target itself cannot express
            def searchable(f):
                fl = m._fields.get(f)
                if fl is None:
                    return False
                if fl.store and not fl.relational:
                    return True
                if not fl.store and fl.related:
                    return True
                return bool(fl.store and fl.type == "many2one")

            usable = bool(fnames) and all(searchable(f) for f in fnames)
            ns = fnames if usable else None
        models[name] = {
            "name_search_fields": ns,
            "display_name_search_exact": exact,
            "read_path_pure": bool(read_path_pure),
            "impure_read_methods": impure_read_methods,
            "search_pure": bool(search_pure),
            "hooked_fields": sorted(hooked),
            "order_pure": bool(order_pure),
            "read_group_pure": bool(read_group_pure),
            "display_name_access_pure": bool(display_name_access_pure),
            "display_name_default": bool(dn_default),
            "table": _s(m._table),
            "order": _s(m._order),
            "rec_name": _s(m._rec_name),
            "parent_name": _s(m._parent_name),
            "parent_store": bool(getattr(m, "_parent_store", False)),
            "active_name": _s(getattr(m, "_active_name", None)),
            "display_name_column": (
                list(col) if isinstance(col := getattr(m, "_display_name_column", None), tuple)
                else ([col] if col else [])
            ),
            "display_name_guard": _s(getattr(m, "_display_name_column_guard", None)),
            "display_name_context_keys": list(getattr(m, "_display_name_context_keys", ()) or ()),
            "fields": fields,
        }
      cr.execute("""
          SELECT m.model FROM ir_model m
            JOIN information_schema.tables t
              ON t.table_schema = current_schema
             AND t.table_name = replace(m.model, '.', '_')
           WHERE t.table_type = 'BASE TABLE'
           ORDER BY m.model
      """)
      expected = [r[0] for r in cr.fetchall()]
      missing = [m for m in expected if m not in models]
      if missing:
          raise RuntimeError(
              "refusing to write an incomplete export: %d of %d table-backed "
              "models are missing (e.g. %s). The registry was not fully loaded; "
              "re-run." % (len(missing), len(expected), ", ".join(missing[:3]))
          )
    try:
        from odoo.libs.datetime.tz import TIMEZONE_ALIASES
        aliases = dict(TIMEZONE_ALIASES)
    except Exception:
        aliases = {}
    return json.dumps({"models": models, "timezone_aliases": aliases, "db": reg.db_name,
                       "registry_sequence": reg.registry_sequence})
"#;

pub fn prepare_python(py: Python<'_>, config: &str) -> PyResult<()> {
    let sys = py.import("sys")?;
    let path = sys
        .getattr("path")?
        .cast_into::<PyList>()
        .map_err(|e| pyo3::exceptions::PyTypeError::new_err(e.to_string()))?;
    path.insert(0, venv_site())?;
    path.insert(0, odoo_root())?;
    py.import("odoo")?;
    py.import("odoo.tools")?.getattr("config")?.call_method1(
        "parse_config",
        (vec!["-c".to_string(), config.to_string()],),
    )?;
    Ok(())
}

pub fn install_wire_module(py: Python<'_>) -> PyResult<()> {
    let src = include_str!("../python/wire.py");
    let m = pyo3::types::PyModule::from_code(
        py,
        &std::ffi::CString::new(src).unwrap(),
        c"wire.py",
        c"wire",
    )?;
    py.import("sys")?.getattr("modules")?.set_item("wire", m)?;
    Ok(())
}

pub fn boot_registry(py: Python<'_>, config: &str, db: &str) -> PyResult<Py<PyAny>> {
    let t0 = std::time::Instant::now();
    tracing::info!(
        target: "odoo_kernel::export",
        %db, %config, "booting an embedded Odoo registry"
    );
    prepare_python(py, config)?;
    let reg = py
        .import("odoo.modules.registry")?
        .getattr("Registry")?
        .call1((db,))?;
    tracing::info!(
        target: "odoo_kernel::export",
        %db, ms = t0.elapsed().as_secs_f64() * 1000.0, "registry booted"
    );
    Ok(reg.unbind())
}

pub fn export_registry(py: Python<'_>, reg: &Py<PyAny>) -> PyResult<String> {
    let t0 = std::time::Instant::now();
    let ns = pyo3::types::PyDict::new(py);
    py.run(
        &std::ffi::CString::new(EXPORT_ALL).unwrap(),
        Some(&ns),
        Some(&ns),
    )?;
    let func = ns.get_item("export_registry")?.unwrap();
    let json: String = func.call1((reg.bind(py),))?.extract()?;
    // This walk reads every model class in the live registry to decide which
    // read paths Python overrides; it runs once per worker at registry load,
    // and its duration is on the first request that pays for it.
    tracing::info!(
        target: "odoo_kernel::export",
        bytes = json.len(),
        ms = t0.elapsed().as_secs_f64() * 1000.0,
        "exported the live Python registry"
    );
    Ok(json)
}

pub fn boot_and_export(config: &str, db: &str) -> Result<String> {
    Python::initialize();
    Python::attach(|py| export_registry(py, &boot_registry(py, config, db)?))
        .map_err(|e| anyhow::anyhow!("python error: {e}"))
}
