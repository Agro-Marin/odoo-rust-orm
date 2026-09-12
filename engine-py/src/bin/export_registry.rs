use std::time::Instant;

fn main() -> anyhow::Result<()> {
    engine_py::logbridge::install_stderr();
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "harness/registry_export.json".to_string());
    let t = Instant::now();
    let json = engine_py::export::boot_and_export(
        &odoo_kernel::config::odoo_conf().display().to_string(),
        &odoo_kernel::config::db(),
    )?;
    std::fs::write(&out, &json)?;
    let parsed: serde_json::Value = serde_json::from_str(&json)?;
    println!(
        "exported {} models ({} KB) to {out} in {:.2}s",
        parsed["models"].as_object().map(|m| m.len()).unwrap_or(0),
        json.len() / 1024,
        t.elapsed().as_secs_f64()
    );
    Ok(())
}
