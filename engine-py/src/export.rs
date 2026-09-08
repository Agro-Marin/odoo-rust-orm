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
    """What Odoo COALESCEs a company-dependent column to."""
    if not getattr(f, "company_dependent", False):
        return None
    try:
        v = f.get_company_dependent_fallback(model)
    except Exception:
        return None
    if hasattr(v, "_name"):          # a recordset: the id is the stored form
        return v.id or None
    return _s(v)

# Keys are exported to be READ. `compute`, `translate`, `attachment`,
# `transient` and `inherits` were written and consumed by nothing:
# `Registry::from_export` never looked at them, the delegation map comes from
# `ir_model_inherit` (which the ir_model bootstrap can read too), and
# `translated` is derived from the column being jsonb -- agreed with the
# declared flag on all 15,674 fields of an agromarin+enterprise database, and
# the derivation is what the bootstrap has to use anyway.
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
            # Odoo's Field.get_comodel_domain APPLIES a list domain to x2many
            # reads and IGNORES a string one (it returns Domain.TRUE for a
            # str). Exporting only the string form therefore recorded exactly
            # the domains that do not matter, and dropped the 32 that do.
            domain = getattr(f, "domain", None)
            if callable(domain):
                domain = {"__callable__": True}
            elif isinstance(domain, str):
                domain = None          # Odoo ignores it, so must we
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
                # a many2one_reference stores the model name in a companion
                # char field; a one2many over such an inverse must filter on
                # it or it returns rows belonging to other models
                "model_field": _s(getattr(f, "model_field", None)),
                # a field with a search= method compiles its own domain in
                # Python; comparing the column instead silently diverges
                "custom_search": bool(getattr(f, "search", None)),
                # a field readable only by some groups: Odoo raises AccessError
                # rather than omitting it, and the column says nothing about it
                "groups": _s(getattr(f, "groups", None)),
                # a non-stored related field reaches SQL in Odoo only through
                # `_traverse_related_sql`, which insists on `env.su`, this
                # flag, or `inherited`; any other one is computed in Python
                # under the caller's access. The kernel refuses what Odoo
                # would refuse, and without these two facts it could not.
                "compute_sudo": bool(getattr(f, "compute_sudo", False)),
                "inherited": bool(getattr(f, "inherited", False)),
                # Odoo merges this into the comodel's environment when it
                # reads or traverses the field, and 10 of this database's 221
                # x2many fields use it to turn the archived filter OFF
                # (res.company.all_child_ids, mail.message.partner_ids, ...).
                # Dropping it would newly hide the archived rows those fields
                # exist to show.
                # A CALLABLE context is evaluated per record and cannot be
                # exported; `dict()` on one raises "'function' object is not
                # iterable", which is how a 253-module database found this and
                # a 35-module one did not. The sentinel is a key the kernel
                # does not model, so it refuses the field rather than reading
                # the context as absent.
                "context": (
                    dict(f.context)
                    if isinstance(getattr(f, "context", None), dict) and f.context
                    else ({"__callable__": True} if callable(getattr(f, "context", None)) else None)
                ),
                # `btree_not_null` means a partial index exists on
                # `col IS NOT NULL`, which is what makes the guard below worth
                # emitting; and the fallback is the value Odoo COALESCEs to,
                # which is the field's default when ir.default has nothing --
                # NOT null, which is what deriving it from ir.default alone
                # produced.
                "index": _s(getattr(f, "index", None)),
                "company_dependent_fallback": _cd_fallback(m, f),
                # The value the COLUMN holds for an unset field, and the
                # whole of Odoo's answer to a comparison against False. It is
                # declared on the field CLASS, not on its type, so it cannot
                # be derived from `type` without knowing the class hierarchy:
                # `id` is a `fields.Id` and not an `Integer`, so it has no 0,
                # and `Many2oneReference` has 0 where its relational siblings
                # have none. `null` here means "no falsy value" and is a
                # different answer from `false`, which is the boolean's.
                "falsy_value": _s(getattr(f, "falsy_value", None)),
                # Odoo evaluates a subquery through this field with the
                # comodel's ACL and record rules TURNED OFF
                # (`_optimize_any_with_rights` rewrites `any` to `any!`, and
                # `_search(bypass_access=True)` skips both). 90 fields of a
                # 73-module database declare it -- every mail-thread
                # `message_ids` and `activity_ids`, every `attachment_ids`,
                # `res.users.partner_id`, `account.move.line.move_id` -- and
                # a kernel that applies the rules anyway answers with fewer
                # rows than Python.
                "bypass_search_access": bool(getattr(f, "bypass_search_access", False)),
            }
        cls = type(m.sudo())
        base = odoo.orm.models.base.BaseModel
        # TWO facts, because two different questions get asked of a model.
        # A SUBQUERY over it -- and materialising an x2many's ids -- is
        # `comodel._search(...)`, and nothing else; whether the model overrides
        # `read` is irrelevant there. Serving the model itself, or rendering a
        # label in the caller's environment, goes through the read family.
        # Collapsing the two refused 464 subqueries over `res.users` on this
        # database, which overrides `read` (its self-readable-field whitelist)
        # and not `_search`.
        search_pure = getattr(cls, "_search", None) is getattr(base, "_search", None)
        read_path_pure = search_pure and not any(
            getattr(cls, n, None) is not getattr(base, n, None)
            for n in ("read", "search_read", "_read_group", "search_count")
        )
        # mirrors rust_orm_shim._display_clean, which is the only other place
        # this predicate is computed
        dn_default = getattr(cls, "_compute_display_name", None) is getattr(
            base, "_compute_display_name", None
        )
        if dn_default:
            dnf = m._fields.get("display_name")
            dn_default = getattr(dnf, "compute", None) in (None, "_compute_display_name")
        if dn_default and m._rec_name:
            rf = m._fields.get(m._rec_name)
            dn_default = rf is not None and bool(rf.store or rf.related)
        # The fields a `like` on a many2one compiles into. Odoo rewrites
        # `('partner_id','ilike',v)` to `('partner_id','any',('display_name',
        # 'ilike',v))` (`domain/optimizations.py::_optimize_relational_name_
        # search`), and `_search_display_name` turns THAT into an OR over
        # `_rec_names_search` -- so the whole thing is compilable exactly when
        # that method is BaseModel's and every name it searches is a stored,
        # non-relational column. None means "ask Python".
        ns = None
        if getattr(cls, "_search_display_name", None) is getattr(
            base, "_search_display_name", None
        ):
            rns = m._rec_names_search
            if callable(rns):
                rns = None  # evaluated in Python; not a list this can resolve
            fnames = list(rns or ([m._rec_name] if m._rec_name else []))
            fnames = [f for f in fnames if not m._is_rec_names_search_cyclic(f)]
            # `_is_rec_names_search_cyclic` only looks at RELATIONAL cycles, so a
            # model whose `_rec_name` is literally `display_name` passes it and
            # would ask the kernel to search display_name to answer
            # display_name. Odoo rewrites that to `display_name.no_error`;
            # there is nothing here to rewrite it to.
            fnames = [f for f in fnames if f != "display_name"]
            usable = bool(fnames) and all(
                (fl := m._fields.get(f)) is not None and fl.store and not fl.relational
                for f in fnames
            )
            ns = fnames if usable else None
        models[name] = {
            "name_search_fields": ns,
            "read_path_pure": bool(read_path_pure),
            "search_pure": bool(search_pure),
            "display_name_default": bool(dn_default),
            "table": _s(m._table),
            "order": _s(m._order),
            "rec_name": _s(m._rec_name),
            "parent_name": _s(m._parent_name),
            "fields": fields,
        }
      # Verify the export before handing it over.
      #
      # It can be silently incomplete: an export taken immediately after a module
      # install produced 640 models where the next four runs produced 950, with
      # hr.job carrying 32 of its 104 fields. The consumer refuses such a file,
      # but the better place to stop it is here -- do not WRITE an export that
      # does not describe the database it came from.
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
    return json.dumps({"models": models})
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
    prepare_python(py, config)?;
    let reg = py
        .import("odoo.modules.registry")?
        .getattr("Registry")?
        .call1((db,))?;
    Ok(reg.unbind())
}

pub fn export_registry(py: Python<'_>, reg: &Py<PyAny>) -> PyResult<String> {
    let ns = pyo3::types::PyDict::new(py);
    py.run(
        &std::ffi::CString::new(EXPORT_ALL).unwrap(),
        Some(&ns),
        Some(&ns),
    )?;
    let func = ns.get_item("export_registry")?.unwrap();
    func.call1((reg.bind(py),))?.extract()
}

pub fn boot_and_export(config: &str, db: &str) -> Result<String> {
    Python::initialize();
    Python::attach(|py| export_registry(py, &boot_registry(py, config, db)?))
        .map_err(|e| anyhow::anyhow!("python error: {e}"))
}
