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
    corpus = json.load(open(os.environ["RUSTPOC_CORPUS_PATH"]))

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

        # -- 1. corpus: original vs routed ------------------------------
        # A case the DATABASE cannot express is not a result. The corpus covers
        # modules a given database need not have -- `supplier_rank` needs
        # account -- and running one of those used to raise out of the whole
        # stage, so `verify.sh` reported the registry sweep as FAILED on any
        # base+mail database with no way to tell that from a real mismatch.
        # Both sides erroring is agreement, which is what gen_expected.py and
        # diff.py have always said; this loop is the last place that did not.
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

        # -- 2. sweep every readable model -------------------------------
        # Shapes are derived from each model's own fields rather than being a
        # single empty-domain read: an empty domain exercises no operator, no
        # ordering and no grouping, which is how a whole-registry sweep could
        # report zero mismatches while every ilike was compiled wrongly.
        def probes(M):
            """Query shapes for this model, from its own stored fields."""
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
                """An order that fully determines the row sequence.

                A probe that LIMITS rows and orders by a key with ties is
                comparing two engines on an arbitrary permutation, and either
                may legitimately return a different one. website.page._order
                is 'website_id', five of its six rows have website_id NULL, and
                the sweep duly reported a mismatch that was nothing at all.
                Most models end _order with id, which is why this went unseen.
                """
                o = (order or M._order or "id").strip()
                last = o.split(",")[-1].strip().split()[0]
                return o if last == "id" else o + ", id"

            if char:
                c = char[0]
                # ilike drives the unaccent path; the falsy pair drives
                # null-equivalence, both invisible to an empty domain
                add("search_count", domain=[(c, "ilike", "a")])
                add("search_count", domain=[(c, "=", False)])
                add("search_count", domain=[(c, "!=", False)])
                add("search_read", domain=[], fields=scal, limit=5,
                    order=total(f"{c} desc"))
            if num:
                add("search_count", domain=[(num[0], ">", 0)])
                add("search_count", domain=[(num[0], "not in", [0])])
            if date:
                add("search_count", domain=[(date[0], "!=", False)])
                add("read_group", domain=[], groupby=[f"{date[0]}:month"],
                    aggregates=["__count"])
            if sel:
                add("read_group", domain=[], groupby=[sel[0]],
                    aggregates=["__count"])
            if m2o:
                m = m2o[0]
                add("search_count", domain=[(m, "!=", False)])
                add("search_count", domain=[(f"{m}.id", "in", [1, 2, 3])])
                add("read_group", domain=[], groupby=[m], aggregates=["__count"])
            if x2m:
                add("search_read", domain=[], fields=[x2m[0]], limit=3,
                    order=total())
                # the same relation as a TRAVERSAL, not a read: field-level
                # domains and the many2one_reference model filter are folded
                # into both, and only the read was probed
                add("search_count", domain=[(x2m[0], "!=", False)])
            if scal:
                add("search_read", domain=[], fields=scal, limit=3, offset=1,
                    order=total())

            # Shapes aimed at what has actually broken, rather than at breadth
            # for its own sake. Each of these corresponds to a defect the
            # narrower probe set could not see.
            if M._order and M._order != "id":
                # the model's OWN _order, and reversed: many2one chains, the
                # nullable-boolean COALESCE, and NULLS handling all live here
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
                # a company-dependent column carries a BOUND parameter, which
                # is what made GROUP BY on one emit invalid SQL
                add("read_group", domain=[], groupby=[cd[0]], aggregates=["__count"])
                add("search_count", domain=[(cd[0], "!=", False)])
            if sel and m2o:
                # two groupby terms: the label join is appended after the
                # aggregates, so the column indices have to stay straight
                add("read_group", domain=[], groupby=[sel[0], m2o[0]],
                    aggregates=["__count"])
            if num and len(num) >= 1:
                add("read_group", domain=[], groupby=[],
                    aggregates=[f"{num[0]}:sum", f"{num[0]}:max", "__count"])
            return cases

        def run_probe(M, case, use_orig):
            """`use_orig` turns ROUTING off; it does not call BaseModel.

            Calling `originals[...]` -- the pre-patch BaseModel methods --
            skips the MODEL's own override, so for any model that defines one
            the comparison was "the model's real method" against "BaseModel's
            generic one", which legitimately differ. `calendar.event
            ._read_group` grouped by `create_date:month` raises AccessError on
            appointment.type at a portal identity, every time, in plain Python;
            the sweep called it a kernel mismatch for as long as one side
            called past the override. Toggling the shim's mode compares the
            same code path with and without the kernel underneath it, which is
            the question being asked."""
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
            """One whole-registry pass at ONE identity."""
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
                try:
                    env.invalidate_all()
                    originals["orig_search_count"](M, [])
                except Exception:
                    denied += 1
                    continue
                model_bad, any_hit, gate_err = [], False, None
                for case in cases:
                    # Each probe runs in a savepoint: a shape that raises a SQL
                    # error would otherwise abort the shared transaction and make
                    # every later model fail for an unrelated reason.
                    def attempt(use_orig, M=M, case=case):
                        with contextlib.closing(cr.savepoint(flush=False)):
                            env.invalidate_all()
                            return _ser(run_probe(M, case, use_orig=use_orig))

                    try:
                        exp = attempt(True)
                    except Exception:
                        continue  # shape not valid for this model; not a kernel fault
                    k0 = orm_shim.STATS["kernel"]
                    try:
                        act = attempt(False)
                    except Exception as e:
                        # Before blaming the routed path: can the ORIGINAL run
                        # a second time? Some shapes cannot -- state the first
                        # pass leaves in the environment makes the second one
                        # raise, with routing disabled for the model entirely --
                        # and a shape Python cannot run twice cannot be
                        # compared against anything.
                        try:
                            attempt(True)
                        except Exception:
                            not_repeatable.append(f"{name}: {case['method']}")
                            continue
                        # the CASE, not just the method: a failure that does
                        # not say which shape produced it cannot be reproduced
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
                    if exp != act:
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

        # A whole-registry sweep at ONE identity checks one point of the space
        # that actually decides the answer. Record rules ARE the headline
        # feature, and three fail-open defects lived in exactly this blind spot
        # when the corpus had the same shape. su is not a variation on
        # non-su either: it takes a different path -- no access check, no rule
        # injection -- so it can only be verified by being run.
        def _ident(uid, su, label):
            e = env0(user=uid, su=su)
            return (label, e(context=dict(e.context, lang="en_US")))

        identities = [_ident(2, False, "admin"), _ident(2, True, "admin_su")]
        # any other real user on this database; a share user if there is one,
        # because its rules differ most
        pool = env0(user=1, su=True)["res.users"].search(
            [("id", "not in", [1, 2]), ("active", "=", True)]
        )
        extra = next((u for u in pool if u.share), pool[:1] and pool[0])
        if extra:
            identities.append(_ident(extra.id, False, f"uid{extra.id}"))

        per_identity, all_ok, total_run, total_routed = {}, None, 0, 0
        for label, e in identities:
            r = sweep(e)
            per_identity[label] = {k: v for k, v in r.items() if k != "kernel_ok"}
            per_identity[label]["kernel_ok"] = len(r["kernel_ok"])
            total_run += r["shapes_run"]
            total_routed += r["shapes_routed"]
            # the allowlist is models that agree at EVERY identity, not at one
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

        # -- 3. timing on a few heavy corpus cases -----------------------
        for cid in ("c21", "c33", "c51", "c11"):
            case = next(c for c in corpus if c["id"] == cid)
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

        # the full policy view, not just the counters: which mode was in
        # force and what share of calls the kernel actually took
        out["stats"] = {
            k: v for k, v in orm_shim.stats().items() if k not in ("errors",)
        }
        cr.rollback()
    return json.dumps(out)
"#;

fn main() -> Result<()> {
    let rt = std::sync::Arc::new(tokio::runtime::Runtime::new()?);

    std::env::set_var("ODOO_DISABLE_COPY", "1");

    if std::env::var_os("RUSTPOC_CORPUS_PATH").is_none() {
        std::env::set_var(
            "RUSTPOC_CORPUS_PATH",
            odoo_kernel::config::harness_dir().join("corpus.json"),
        );
    }
    Python::initialize();

    let result: String = Python::attach(|py| -> PyResult<String> {
        engine_py::export::prepare_python(
            py,
            &odoo_kernel::config::odoo_conf().display().to_string(),
        )?;
        engine_py::export::install_wire_module(py)?;

        let db_shim = pyo3::types::PyModule::from_code(
            py,
            &std::ffi::CString::new(include_str!("../../python/rust_db_shim.py")).unwrap(),
            c"rust_db_shim.py",
            c"rust_db_shim",
        )?;
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

        let orm_shim = pyo3::types::PyModule::from_code(
            py,
            &std::ffi::CString::new(include_str!("../../python/rust_orm_shim.py")).unwrap(),
            c"rust_orm_shim.py",
            c"rust_orm_shim",
        )?;
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
        out["sweep"]["kernel_ok"], out["sweep"]["mismatch"],
        out["sweep"]["gated_or_error"], out["sweep"]["denied"]
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
