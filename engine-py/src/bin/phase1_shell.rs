use std::time::Instant;

use anyhow::Result;
use engine_py::cursor::RustDb;
use pyo3::prelude::*;

const DRIVER: &str = r#"
import json, datetime, time

from wire import ser as _ser

def run(reg, shim):
    import odoo.api
    out = {}
    with reg.cursor() as cr:
        env0 = odoo.api.Environment(cr, 2, {})
        env = env0(user=2, su=False)
        env = env(context=dict(env.context, lang="en_US"))

        # read parity: harness case c21
        t0 = time.perf_counter()
        recs = env["res.partner"].search_read(
            [], ["name", "email", "is_company", "type", "create_date", "country_id"],
            limit=10,
        )
        out["c21_ms"] = (time.perf_counter() - t0) * 1000.0
        out["c21"] = _ser(recs)

        # write path: create (jsonb name, RETURNING id), read back, rollback
        su = env0
        before = su["res.partner.industry"].search_count([["name", "ilike", "phaseone"]])
        rec = su["res.partner.industry"].create({"name": "PhaseOne Industry"})
        su.flush_all()
        mid = su["res.partner.industry"].search_count([["name", "ilike", "phaseone"]])
        found = su["res.partner.industry"].browse(rec.id).name
        cr.rollback()
        su.invalidate_all()
        after = su["res.partner.industry"].search_count([["name", "ilike", "phaseone"]])
        out["write"] = {"before": before, "mid": mid, "name": found, "after": after}

        out["borrowed_connections"] = shim.INSTALLED["count"]
        out["pool"] = json.dumps(shim.pool_stats())
        out["sql_count"] = cr.sql_log_count
    return json.dumps(out)
"#;

fn main() -> Result<()> {
    // SAFETY: main has spawned no thread yet, so no other thread can be reading
    // the environment concurrently.
    unsafe {
        std::env::set_var("ODOO_DISABLE_COPY", "1");
    }

    let rt = std::sync::Arc::new(tokio::runtime::Runtime::new()?);

    Python::initialize();

    let result: String = Python::attach(|py| -> PyResult<String> {
        engine_py::export::prepare_python(
            py,
            &odoo_kernel::config::odoo_conf().display().to_string(),
        )?;
        engine_py::export::install_wire_module(py)?;

        let shim_src = include_str!("../../python/rust_db_shim.py");
        let shim = pyo3::types::PyModule::from_code(
            py,
            &std::ffi::CString::new(shim_src).unwrap(),
            c"rust_db_shim.py",
            c"rust_db_shim",
        )?;
        let rust_db = Py::new(py, RustDb::new(odoo_kernel::config::dsn(), rt.clone()))?;
        shim.setattr("RUST_DB", rust_db)?;
        shim.setattr("CONNINFO", odoo_kernel::config::dsn())?;
        shim.call_method0("install")?;
        println!("[shim] psycopg pool patched: all connections are rust-backed");

        let t = Instant::now();
        let reg = py
            .import("odoo.modules.registry")?
            .getattr("Registry")?
            .call1((&odoo_kernel::config::db(),))?;
        println!(
            "[boot] registry loaded through rust connections in {:.2}s",
            t.elapsed().as_secs_f64()
        );

        let ns = pyo3::types::PyDict::new(py);
        py.run(
            &std::ffi::CString::new(DRIVER).unwrap(),
            Some(&ns),
            Some(&ns),
        )?;
        let func = ns.get_item("run")?.unwrap();
        func.call1((reg, shim))?.extract()
    })
    .map_err(|e| anyhow::anyhow!("python error: {e}"))?;

    let out: serde_json::Value = serde_json::from_str(&result)?;

    let expected_path = std::env::var("RUSTORM_EXPECTED").unwrap_or_else(|_| {
        odoo_kernel::config::harness_dir()
            .join("expected.json")
            .display()
            .to_string()
    });

    let expected: serde_json::Value = match std::fs::read_to_string(&expected_path) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(e) => {
            println!("[orm]  c21 parity: SKIPPED (no baseline at {expected_path}: {e})");
            serde_json::Value::Null
        }
    };
    let baseline_db = expected["db"].as_str();
    let cases = expected
        .get("cases")
        .and_then(|c| c.as_array())
        .or_else(|| expected.as_array());

    let comparable = baseline_db.is_some_and(|d| d == odoo_kernel::config::db());
    let exp_c21 = cases
        .and_then(|c| c.iter().find(|c| c["id"] == "c21"))
        .map(|c| &c["result"])
        .filter(|_| comparable);

    println!(
        "\n[orm]  search_read c21 through odoo ORM on rust connections: {:.2}ms",
        out["c21_ms"].as_f64().unwrap_or(0.0)
    );
    match exp_c21 {
        None => println!(
            "[orm]  c21 parity: SKIPPED (baseline {} is from db {:?}, running {:?}; \
             regenerate with gen_expected.py)",
            expected_path,
            baseline_db.unwrap_or("<unstamped>"),
            odoo_kernel::config::db()
        ),
        Some(exp) if &out["c21"] == exp => {
            println!("[orm]  c21 parity vs shadow baseline: MATCH")
        }
        Some(exp) => {
            println!("[orm]  c21 parity: MISMATCH");
            println!("  expected: {}", serde_json::to_string(exp)?);
            println!("  actual:   {}", serde_json::to_string(&out["c21"])?);
        }
    }
    println!(
        "[write] create/flush/rollback: before={} mid={} name={:?} after={}",
        out["write"]["before"], out["write"]["mid"], out["write"]["name"], out["write"]["after"]
    );
    println!(
        "[stats] {} rust connections borrowed ({}), {} SQL statements on the final cursor",
        out["borrowed_connections"], out["pool"], out["sql_count"]
    );

    let ok = exp_c21.is_none_or(|exp| &out["c21"] == exp)
        && out["write"]["mid"].as_i64() == Some(out["write"]["before"].as_i64().unwrap_or(0) + 1)
        && out["write"]["after"] == out["write"]["before"];
    println!("\nPhase 1 exit test: {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
