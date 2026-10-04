use anyhow::Result;
use engine_py::cursor::RustDb;
use pyo3::prelude::*;

const DRIVER: &str = r#"
import importlib, json, os
from upstream_suite import run as run_upstream
from odoo.tests.loader import get_module_test_cases
from odoo.tests.suite import OdooSuite
from odoo.tests.tag_selector import TagsSelector

MODULES = [
    "odoo.addons.base.tests.test_expression",
    "odoo.addons.base.tests.test_search",
]

ONLY = [t for t in os.environ.get("PHASE2_ONLY", "").split(",") if t]
FULL_TB = os.environ.get("PHASE2_TRACEBACK") == "1"

def run_suite(orm_shim, kernel, label):
    import odoo.tools as tools
    tools.config["db_name"] = os.environ.get("RUSTORM_DB", "rustorm_probe")
    orm_shim.KERNEL = kernel
    k0 = orm_shim.STATS["kernel"]
    out = {}
    for modname in MODULES:
        mod = importlib.import_module(modname)
        tags = TagsSelector("standard")
        suite = OdooSuite(sorted(
            (test for test in get_module_test_cases(mod) if tags.select_test(test)),
            key=lambda test: getattr(test, "test_sequence", 0),
        ))
        report = run_upstream(suite, only=ONLY, full_traceback=FULL_TB)
        report.pop("outcomes")
        report.pop("all_details")
        out[modname.rsplit(".", 1)[-1]] = report
    out["kernel_calls"] = orm_shim.STATS["kernel"] - k0
    return out

def run(orm_shim, kernel, mode):
    return json.dumps({mode: run_suite(orm_shim, kernel if mode == "routed" else None, mode)})
"#;

fn run_mode(mode: &str) -> Result<serde_json::Value> {
    let out = std::process::Command::new(std::env::current_exe()?)
        .env("PHASE2_MODE", mode)
        .output()?;
    if let Some(dir) = std::env::var_os("RUSTORM_VERIFY_OUT") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("upstream-{mode}.stdout.log")), &out.stdout)?;
        std::fs::write(dir.join(format!("upstream-{mode}.stderr.log")), &out.stderr)?;
    }
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
    [
        "run",
        "failures",
        "errors",
        "skipped",
        "infrastructure_skipped",
    ]
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
            "  {name:16} baseline {} tests, {} failures, {} errors -> routed {} tests, {} failures, {} errors",
            b["run"], b["failures"], b["errors"], r["run"], r["failures"], r["errors"]
        );
        if [b, r].iter().any(|m| {
            m["infrastructure_skipped"].as_i64().unwrap_or(0) > 0
                || m["aborted"].as_str().is_some_and(|s| !s.is_empty())
        }) {
            ok = false;
            println!("    suite aborted or skipped infrastructure");
        }
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
        for (leg, suite) in [("baseline", b), ("routed", r)] {
            for d in suite["detail"].as_array().into_iter().flatten() {
                println!(
                    "    {leg} {}: {}",
                    d[0].as_str().unwrap_or("?"),
                    d[1].as_str().unwrap_or("")
                );
            }
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
    engine_py::logbridge::install_stderr();
    if std::env::var_os("PHASE2_MODE").is_none() {
        return compare();
    }
    Python::initialize();

    let result: String = Python::attach(|py| -> PyResult<String> {
        engine_py::export::prepare_python(
            py,
            &odoo_kernel::config::odoo_conf().display().to_string(),
        )?;
        let (db_shim, orm_shim) = engine_py::install_shims_py(py)?;
        let rust_db = Py::new(py, RustDb::new(odoo_kernel::config::dsn()))?;
        db_shim.setattr("RUST_DB", &rust_db)?;
        db_shim.setattr("CONNINFO", odoo_kernel::config::dsn())?;
        db_shim.call_method0("install")?;
        db_shim.call_method1("set_active", (true,))?;

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
        py.import("sys")?.getattr("path")?.call_method1(
            "insert",
            (0, odoo_kernel::config::harness_dir().display().to_string()),
        )?;
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
