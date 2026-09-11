use anyhow::Result;
use engine_py::cursor::RustDb;
use pyo3::prelude::*;

const DRIVER: &str = r#"
import io, json, os, unittest, importlib

MODULES = [
    "odoo.addons.base.tests.test_expression",
    "odoo.addons.base.tests.test_search",
]

ONLY = [t for t in os.environ.get("PHASE2_ONLY", "").split(",") if t]
FULL_TB = os.environ.get("PHASE2_TRACEBACK") == "1"

def _select(suite):
    out = unittest.TestSuite()
    for t in suite:
        if isinstance(t, unittest.TestSuite):
            out.addTest(_select(t))
        elif not ONLY or t._testMethodName in ONLY:
            out.addTest(t)
    return out

def run_suite(orm_shim, kernel, label):
    import odoo.tools as tools
    tools.config["db_name"] = os.environ.get("RUSTORM_DB", "rustorm_probe")
    orm_shim.KERNEL = kernel
    k0 = orm_shim.STATS["kernel"]
    out = {}
    for modname in MODULES:
        mod = importlib.import_module(modname)
        suite = _select(unittest.TestLoader().loadTestsFromModule(mod))
        runner = unittest.TextTestRunner(verbosity=0, stream=io.StringIO())
        r = runner.run(suite)
        out[modname.rsplit(".", 1)[-1]] = {
            "run": r.testsRun,
            "failures": len(r.failures),
            "errors": len(r.errors),
            "skipped": len(r.skipped),
            "problems": sorted(
                str(t[0]).split(" ")[0] for t in (r.failures + r.errors)
            )[:12],
            "detail": [
                [
                    str(t[0]).split(" ")[0],
                    str(t[1]).strip() if FULL_TB
                    else str(t[1]).strip().splitlines()[-1][:180],
                ]
                for t in (r.failures + r.errors)[:4]
            ],
        }
    out["kernel_calls"] = orm_shim.STATS["kernel"] - k0
    return out

def run(orm_shim, kernel, mode):
    return json.dumps({mode: run_suite(orm_shim, kernel if mode == "routed" else None, mode)})
"#;

fn run_mode(mode: &str) -> Result<serde_json::Value> {
    let out = std::process::Command::new(std::env::current_exe()?)
        .env("PHASE2_MODE", mode)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "{mode}: child exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = err.lines().rev().take(8).collect();
            anyhow::anyhow!(
                "{mode}: no JSON payload in child output (exit {:?}). Child stderr:\n  {}",
                out.status.code(),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n  ")
            )
        })?;
    Ok(serde_json::from_str::<serde_json::Value>(line)?[mode].clone())
}

fn problems(m: &serde_json::Value) -> std::collections::BTreeSet<String> {
    m["problems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect()
}

fn counts(m: &serde_json::Value) -> Vec<i64> {
    ["run", "failures", "errors", "skipped"]
        .iter()
        .map(|k| m[k].as_i64().unwrap_or(0))
        .collect()
}

fn compare() -> Result<()> {
    let baseline = run_mode("baseline")?;
    let routed = run_mode("routed")?;
    let mut ok = true;
    println!("\n== upstream suites, routing off vs on (separate processes) ==");
    for (name, b) in baseline.as_object().into_iter().flatten() {
        if name == "kernel_calls" {
            continue;
        }
        let r = &routed[name];
        println!(
            "  {name:16} baseline {}/{} fail  ->  routed {}/{} fail",
            b["failures"], b["run"], r["failures"], r["run"]
        );
        let (bp, rp) = (problems(b), problems(r));
        for t in rp.difference(&bp) {
            ok = false;
            println!("    APPEARED under routing: {t}");
        }
        for t in bp.difference(&rp) {
            ok = false;
            println!("    GONE under routing: {t}");
        }
        if counts(b) != counts(r) {
            ok = false;
            println!("    count delta: {:?} vs {:?}", counts(b), counts(r));
        }
        if !bp.is_empty() {
            ok = false;
            println!("    baseline failures also block the gate: {bp:?}");
        }
    }
    let executed: i64 = baseline
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.as_str() != "kernel_calls")
        .map(|(_, suite)| {
            suite["run"].as_i64().unwrap_or(0) - suite["skipped"].as_i64().unwrap_or(0)
        })
        .sum();
    if executed == 0 {
        ok = false;
        println!("    no unskipped upstream tests ran");
    }
    let calls = routed["kernel_calls"].as_i64().unwrap_or(0);
    println!("  kernel calls under routing: {calls}");
    if calls < 20 {
        println!(
            "  NOTE: these suites call search()/read() far more than the\n\
             \x20       search_read / search_count / _read_group the shim routes,\n\
             \x20       so a zero delta says the kernel did not BREAK Odoo -- not\n\
             \x20       that it was exercised. phase2_verify is the coverage check."
        );
    }
    println!(
        "\nPhase 2 upstream gate: {}",
        if ok { "PASS" } else { "FAIL" }
    );
    std::process::exit(if ok { 0 } else { 1 });
}

fn main() -> Result<()> {
    if std::env::var_os("PHASE2_MODE").is_none() {
        return compare();
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

        let reg = py
            .import("odoo.modules.registry")?
            .getattr("Registry")?
            .call1((&odoo_kernel::config::db(),))?;
        let export_json = engine_py::export::export_registry(py, &reg.clone().unbind())?;
        let kernel = engine_py::kernel::RustKernel::build(py, &rust_db.borrow(py), &export_json)?;
        let kernel_py = Py::new(py, kernel)?;

        orm_shim.setattr("KERNEL", &kernel_py)?;
        orm_shim.call_method0("install")?;
        println!("[hybrid] registry + kernel + routing ready; running upstream tests");

        let mode = std::env::var("PHASE2_MODE").unwrap_or_else(|_| "routed".into());
        let ns = pyo3::types::PyDict::new(py);
        py.run(
            &std::ffi::CString::new(DRIVER).unwrap(),
            Some(&ns),
            Some(&ns),
        )?;
        ns.get_item("run")?
            .unwrap()
            .call1((orm_shim, kernel_py, mode))?
            .extract()
    })
    .map_err(|e| anyhow::anyhow!("python error: {e}"))?;

    println!("{result}");
    Ok(())
}
