use std::time::Instant;

use anyhow::Result;
use engine_py::cursor::RustDb;
use pyo3::prelude::*;

const DRIVER: &str = r#"
import contextlib, json, datetime, os, time

from wire import ser as _ser

def run(reg, orm_shim, originals):
    import odoo.api
    out = {"corpus": {}, "sweep": {}, "timing": {}}
    corpus = json.load(open(os.environ["RUSTORM_CORPUS_PATH"]))

    with reg.cursor() as cr:
        env0 = odoo.api.Environment(cr, 2, {})
        env = env0(user=2, su=False)
        env = env(context=dict(env.context, lang="en_US"))

        def run_case(case, use_orig):
            M = env[case["model"]]
            method = case["method"]
            domain = case.get("domain") or []
            if method == "search_read":
                f = originals["orig_search_read"] if use_orig else type(M).search_read
                return f(M, domain=domain, fields=case["fields"],
                         offset=case.get("offset") or 0,
                         limit=case.get("limit"), order=case.get("order"))
            if method == "search_count":
                f = originals["orig_search_count"] if use_orig else type(M).search_count
                return f(M, domain)
            if method == "read_group":
                gb = case["groupby"]
                gb = [gb] if isinstance(gb, str) else gb
                aggs = case.get("aggregates") or ["__count"]
                f = originals["orig_read_group"] if use_orig else type(M)._read_group
                return f(M, domain, groupby=gb, aggregates=aggs)
            raise ValueError(method)

        match, mismatch, cases_kernel, vacuous = 0, [], 0, 0
        def attempt(case, use_orig):
            with contextlib.closing(cr.savepoint(flush=False)):
                env.invalidate_all()
                return _ser(run_case(case, use_orig=use_orig))
        for case in corpus:
            k0 = orm_shim.STATS["kernel"]
            try:
                a, a_err = attempt(case, use_orig=False), None
            except Exception as e:
                a, a_err = None, str(e)[:120]
            routed_hit = orm_shim.STATS["kernel"] > k0
            try:
                b, b_err = attempt(case, use_orig=True), None
            except Exception as e:
                b, b_err = None, str(e)[:120]
            if a_err and b_err:
                vacuous += 1
            elif a_err or b_err:
                mismatch.append("%s: %s" % (case["id"], a_err or ("python raised " + b_err)))
            elif a == b:
                match += 1
            else:
                mismatch.append(case["id"])
            cases_kernel += bool(routed_hit)
        out["corpus"] = {"match": match, "total": len(corpus), "vacuous": vacuous,
                         "kernel_handled": cases_kernel, "mismatch": mismatch}

        def probes(M):
            f = M._fields
            def pick(*types, n=1, skip=()):
                out = [name for name, fld in sorted(f.items())
                       if fld.type in types and fld.store
                       and getattr(fld, "column_type", None)
                       and name not in skip and name != "id"]
                return out[:n]
            char = pick("char")
            num = pick("integer", "float", "monetary")
            date = pick("date", "datetime")
            m2o = pick("many2one")
            sel = pick("selection")
            x2m = [name for name, fld in sorted(f.items())
                   if fld.type in ("one2many", "many2many")][:1]
            scal = (char + num + date + m2o + sel)[:4]
            cases = []
            def add(method, **kw):
                cases.append(dict(method=method, **kw))

            def total(order=None):
                o = (order or M._order or "id").strip()
                last = o.split(",")[-1].strip().split()[0]
                return o if last == "id" else o + ", id"

            boolean = pick("boolean")
            if char:
                c = char[0]
                add("search_count", domain=[(c, "ilike", "a")])
                add("search_count", domain=[(c, "=", False)])
                add("search_count", domain=[(c, "!=", False)])
                add("search_read", domain=[], fields=scal, limit=5,
                    order=total(f"{c} desc"))
                # the pre-SQL optimisations: empty and wildcard-only patterns,
                # a scalar for `in`, a list for `=`
                add("search_count", domain=[(c, "like", "")])
                add("search_count", domain=[(c, "not like", "")])
                add("search_count", domain=[(c, "=like", "")])
                add("search_count", domain=[(c, "like", "%")])
                add("search_count", domain=[(c, "in", "a")])
                add("search_count", domain=[(c, "=", ["a", "b"])])
            if num:
                add("search_count", domain=[(num[0], ">", 0)])
                add("search_count", domain=[(num[0], "not in", [0])])
                add("search_count", domain=[(num[0], "=", "1")])
                add("search_count", domain=[(num[0], "in", ["1", "x"])])
                add("search_count", domain=[(num[0], "=?", 0)])
                add("search_count", domain=[(num[0], ">", False)])
            if boolean:
                add("search_count", domain=[(boolean[0], "=", 1)])
                add("search_count", domain=[(boolean[0], "in", ["true"])])
            if date:
                add("search_count", domain=[(date[0], "!=", False)])
                add("search_count", domain=[(date[0], ">", False)])
                add("read_group", domain=[], groupby=[f"{date[0]}:month"],
                    aggregates=["__count"])
                add("read_group", domain=[], groupby=[f"{date[0]}:week"],
                    aggregates=["__count"])
                add("read_group", domain=[], groupby=[f"{date[0]}:day_of_week"],
                    aggregates=["__count"])
                add("read_group", domain=[], groupby=[f"{date[0]}:year_number"],
                    aggregates=["__count"])
            if M._rec_name:
                add("search_count", domain=[("display_name", "!=", False)])
                add("search_count", domain=[("display_name", "in", ["a", False])])
            if sel:
                add("read_group", domain=[], groupby=[sel[0]],
                    aggregates=["__count"])
            if m2o:
                m = m2o[0]
                add("search_count", domain=[(m, "!=", False)])
                add("search_count", domain=[(f"{m}.id", "in", [1, 2, 3])])
                add("read_group", domain=[], groupby=[m], aggregates=["__count"])
                add("search_count", domain=[(m, "=", 0)])
                add("search_count", domain=[(m, "not ilike", "")])
                add("search_count", domain=[(m, "=ilike", "")])
                add("search_count", domain=[(m, ">", False)])
            if x2m:
                add("search_read", domain=[], fields=[x2m[0]], limit=3,
                    order=total())
                add("search_count", domain=[(x2m[0], "!=", False)])
                add("search_count", domain=[(x2m[0], "=", False)])
                add("search_count", domain=[(x2m[0], "in", [0])])
                add("search_count", domain=[(x2m[0], "in", [1, 2])])
            if scal:
                add("search_read", domain=[], fields=scal, limit=3, offset=1,
                    order=total())

            if M._order and M._order != "id":
                add("search_read", domain=[], fields=(scal or ["id"])[:2], limit=5,
                    order=total())
                rev = ", ".join(
                    p.split()[0] + (" asc" if p.strip().endswith("desc") else " desc")
                    for p in M._order.split(",") if p.strip()
                )
                add("search_read", domain=[], fields=(scal or ["id"])[:2],
                    limit=5, order=total(rev))
            cd = [n for n, fld in sorted(f.items())
                  if getattr(fld, "company_dependent", False) and fld.store][:1]
            if cd:
                add("read_group", domain=[], groupby=[cd[0]], aggregates=["__count"])
                add("search_count", domain=[(cd[0], "!=", False)])
            if sel and m2o:
                add("read_group", domain=[], groupby=[sel[0], m2o[0]],
                    aggregates=["__count"])
            if num and len(num) >= 1:
                add("read_group", domain=[], groupby=[],
                    aggregates=[f"{num[0]}:sum", f"{num[0]}:max", "__count"])
            return cases

        def canon(M, case, value):
            # an x2many's ids follow the comodel's _order, which is rarely
            # total: two corecords equal under it come back in either order,
            # from Python as much as from the kernel, so such lists compare
            # as sets; a comodel ordered through id keeps its order
            if case["method"] != "search_read" or not isinstance(value, list):
                return value
            loose = []
            for fname in case.get("fields") or []:
                f = M._fields.get(fname)
                if f is None or f.type not in ("one2many", "many2many"):
                    continue
                co = M.env.get(f.comodel_name)
                terms = [t.strip().split()[0] for t in (co._order if co is not None else "id").split(",") if t.strip()]
                if "id" not in terms:
                    loose.append(fname)
            if not loose:
                return value
            out = []
            for row in value:
                if isinstance(row, dict):
                    row = dict(row)
                    for fname in loose:
                        if isinstance(row.get(fname), list):
                            row[fname] = sorted(row[fname], key=lambda v: (str(type(v)), str(v)))
                out.append(row)
            return out

        def run_probe(M, case, use_orig):
            method = case["method"]
            domain = case.get("domain") or []
            saved = orm_shim.MODE
            if use_orig:
                orm_shim.MODE = "off"
            try:
                if method == "search_read":
                    return type(M).search_read(
                        M, domain=domain, fields=case["fields"],
                        offset=case.get("offset") or 0, limit=case.get("limit"),
                        order=case.get("order"))
                if method == "search_count":
                    return type(M).search_count(M, domain)
                return type(M)._read_group(M, domain, groupby=case["groupby"],
                                           aggregates=case["aggregates"])
            finally:
                orm_shim.MODE = saved

        def sweep(env):
            ok_models, mism_models, gated, errored, denied = [], [], [], [], 0
            not_repeatable = []
            shapes_run = shapes_routed = 0
            for name in sorted(reg.models):
                M = env.get(name)
                if M is None or M._abstract or not M._auto:
                    continue
                cases = probes(M)
                if not cases:
                    continue
                # inside a savepoint: a model whose registry outruns the
                # database (a peer's uncommitted field, a skipped migration)
                # raises a SQL error that would otherwise abort the transaction
                # for every model after it
                try:
                    with contextlib.closing(cr.savepoint(flush=False)):
                        env.invalidate_all()
                        originals["orig_search_count"](M, [])
                except Exception:
                    denied += 1
                    continue
                model_bad, any_hit, gate_err = [], False, None
                for case in cases:
                    def attempt(use_orig, M=M, case=case):
                        with contextlib.closing(cr.savepoint(flush=False)):
                            env.invalidate_all()
                            return _ser(run_probe(M, case, use_orig=use_orig))

                    try:
                        exp = attempt(True)
                    except Exception:
                        continue
                    k0 = orm_shim.STATS["kernel"]
                    try:
                        act = attempt(False)
                    except Exception as e:
                        try:
                            attempt(True)
                        except Exception:
                            not_repeatable.append(f"{name}: {case['method']}")
                            continue
                        model_bad.append(
                            f"{case['method']} {json.dumps({k: v for k, v in case.items() if k != 'method'})[:120]}"
                            f": raised {str(e)[:80]}"
                        )
                        continue
                    shapes_run += 1
                    if orm_shim.STATS["kernel"] > k0:
                        any_hit = True
                        shapes_routed += 1
                    elif gate_err is None:
                        gate_err = orm_shim.STATS["errors"].get(name)
                    if canon(M, case, exp) != canon(M, case, act):
                        model_bad.append(
                            f"{case['method']} {case.get('domain') or case.get('groupby')}: "
                            f"{str(exp)[:90]} vs {str(act)[:90]}"
                        )
                if model_bad:
                    mism_models.append([name, model_bad[0]])
                elif any_hit:
                    ok_models.append(name)
                else:
                    gated.append(name)
                    if gate_err:
                        errored.append((name, gate_err))
            return {"kernel_ok": ok_models, "mismatch": mism_models,
                    "gated_or_error": len(gated), "denied": denied,
                    "not_repeatable": not_repeatable,
                    "shapes_run": shapes_run, "shapes_routed": shapes_routed}

        def _ident(uid, su, label):
            e = env0(user=uid, su=su)
            # a timezone the web client always sends; a datetime groupby that
            # ignored it would group in UTC and differ
            return (label, e(context=dict(e.context, lang="en_US", tz="America/Mexico_City")))

        identities = [_ident(2, False, "admin"), _ident(2, True, "admin_su")]
        pool = env0(user=1, su=True)["res.users"].search(
            [("id", "not in", [1, 2]), ("active", "=", True)]
        )
        # one share user and one internal non-admin user: their group sets
        # differ, and a rule that only bites at one of them is invisible at
        # the other. RUSTORM_SWEEP_UIDS names uids outright instead.
        wanted = os.environ.get("RUSTORM_SWEEP_UIDS")
        if wanted:
            extras = [pool.browse(int(x)) for x in wanted.split(",") if x.strip()]
        else:
            share = next((u for u in pool if u.share), None)
            internal = next((u for u in pool if not u.share), None)
            extras = [u for u in (share, internal) if u]
        for extra in extras:
            identities.append(_ident(extra.id, False, f"uid{extra.id}"))

        per_identity, all_ok, total_run, total_routed = {}, None, 0, 0
        for label, e in identities:
            r = sweep(e)
            per_identity[label] = {k: v for k, v in r.items() if k != "kernel_ok"}
            per_identity[label]["kernel_ok"] = len(r["kernel_ok"])
            total_run += r["shapes_run"]
            total_routed += r["shapes_routed"]
            names = set(r["kernel_ok"])
            all_ok = names if all_ok is None else (all_ok & names)

        out["sweep"] = {
            "identities": per_identity,
            "kernel_ok": len(all_ok or ()),
            "mismatch": [m for r in per_identity.values() for m in r["mismatch"]],
            "gated_or_error": per_identity[identities[0][0]]["gated_or_error"],
            "denied": per_identity[identities[0][0]]["denied"],
            "shapes_run": total_run, "shapes_routed": total_routed,
            "gate_errors": {k: v for k, v in list(orm_shim.STATS["errors"].items())[:8]},
        }
        out["allowlist"] = sorted(all_ok or ())

        for cid in ("c21", "c33", "c51", "c11"):
            case = next((c for c in corpus if c["id"] == cid), None)
            if case is None:
                continue
            times = {"orig": [], "routed": []}
            for kind in ("orig", "routed"):
                for _ in range(15):
                    env.invalidate_all()
                    t0 = time.perf_counter()
                    run_case(case, use_orig=(kind == "orig"))
                    times[kind].append((time.perf_counter() - t0) * 1000)
            out["timing"][cid] = {
                k: sorted(v)[len(v) // 2] for k, v in times.items()
            }

        out["stats"] = {
            k: v for k, v in orm_shim.stats().items() if k not in ("errors",)
        }
        cr.rollback()
    return json.dumps(out)
"#;

fn main() -> Result<()> {
    engine_py::logbridge::install_stderr();
    // SAFETY: main has spawned no thread yet, so no other thread can be reading
    // the environment concurrently.
    unsafe {
        std::env::set_var("ODOO_DISABLE_COPY", "1");

        if std::env::var_os("RUSTORM_CORPUS_PATH").is_none() {
            std::env::set_var(
                "RUSTORM_CORPUS_PATH",
                odoo_kernel::config::harness_dir().join("corpus.json"),
            );
        }
    }

    let rt = std::sync::Arc::new(tokio::runtime::Runtime::new()?);
    Python::initialize();

    let result: String = Python::attach(|py| -> PyResult<String> {
        engine_py::export::prepare_python(
            py,
            &odoo_kernel::config::odoo_conf().display().to_string(),
        )?;
        let (db_shim, orm_shim) = engine_py::install_shims_py(py)?;
        let rust_db = Py::new(py, RustDb::new(odoo_kernel::config::dsn(), rt.clone()))?;
        db_shim.setattr("RUST_DB", &rust_db)?;
        db_shim.setattr("CONNINFO", odoo_kernel::config::dsn())?;
        db_shim.call_method0("install")?;

        let t = Instant::now();
        let reg = py
            .import("odoo.modules.registry")?
            .getattr("Registry")?
            .call1((&odoo_kernel::config::db(),))?;
        println!(
            "[boot] registry on rust connections: {:.2}s",
            t.elapsed().as_secs_f64()
        );

        let export_json = engine_py::export::export_registry(py, &reg.clone().unbind())?;
        let kernel = engine_py::kernel::RustKernel::build(py, &rust_db.borrow(py), &export_json)?;
        println!(
            "[kernel] {} models from live-registry export",
            kernel.model_count_pub()
        );
        let kernel_py = Py::new(py, kernel)?;

        orm_shim.setattr("KERNEL", kernel_py)?;
        let originals = orm_shim.call_method0("install")?;
        println!("[shim] search_read / search_count / _read_group routed to kernel");

        let ns = pyo3::types::PyDict::new(py);
        py.run(
            &std::ffi::CString::new(DRIVER).unwrap(),
            Some(&ns),
            Some(&ns),
        )?;
        ns.get_item("run")?
            .unwrap()
            .call1((reg, orm_shim, originals))?
            .extract()
    })
    .map_err(|e| anyhow::anyhow!("python error: {e}"))?;

    let out: serde_json::Value = serde_json::from_str(&result)?;

    let allowlist = odoo_kernel::config::harness_dir().join("phase2_allowlist.json");
    std::fs::write(&allowlist, serde_json::to_string_pretty(&out["allowlist"])?)?;

    println!("\n== corpus (original vs routed, same transaction) ==");
    println!(
        "  {} / {} match, kernel handled {} cases, mismatches: {}",
        out["corpus"]["match"],
        out["corpus"]["total"],
        out["corpus"]["kernel_handled"],
        out["corpus"]["mismatch"]
    );
    println!("== whole-registry sweep ==");
    println!(
        "  kernel-verified at EVERY identity: {}  (allowlist written), mismatches: {}, gated/fallback: {}, no-access skipped: {}",
        out["sweep"]["kernel_ok"],
        out["sweep"]["mismatch"],
        out["sweep"]["gated_or_error"],
        out["sweep"]["denied"]
    );
    println!(
        "  query shapes compared: {} ({} routed to the kernel)",
        out["sweep"]["shapes_run"], out["sweep"]["shapes_routed"]
    );
    if let Some(ids) = out["sweep"]["identities"].as_object() {
        for (label, r) in ids {
            println!(
                "    {label:10} shapes {} ({} routed), models ok {}, mismatches {}",
                r["shapes_run"],
                r["shapes_routed"],
                r["kernel_ok"],
                r["mismatch"].as_array().map(|a| a.len()).unwrap_or(0)
            );
        }
    }
    if let Some(ms) = out["sweep"]["mismatch"].as_array() {
        for m in ms.iter().take(10) {
            println!("    MISMATCH {}", m);
        }
    }
    if let Some(errs) = out["sweep"]["gate_errors"].as_object() {
        for (m, e) in errs.iter().take(6) {
            println!("    fallback[{m}]: {e}");
        }
    }
    println!("== timing (median ms, in-process) ==");
    for (cid, t) in out["timing"].as_object().unwrap() {
        let o = t["orig"].as_f64().unwrap_or(0.0);
        let r = t["routed"].as_f64().unwrap_or(0.0);
        println!(
            "  {cid}: python {o:.2}ms -> routed {r:.2}ms ({:.1}x)",
            o / r.max(0.001)
        );
    }
    println!("== shim stats == {}", out["stats"]);

    let empty = |v: &serde_json::Value| v.as_array().is_some_and(|a| a.is_empty());
    let ok = empty(&out["corpus"]["mismatch"]) && empty(&out["sweep"]["mismatch"]);
    println!("\nPhase 2 verifier: {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
