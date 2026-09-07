pub mod cursor;
pub mod export;
pub mod kernel;
pub mod logbridge;

#[pyo3::pymodule]
fn engine_py(
    py: pyo3::Python<'_>,
    m: &pyo3::Bound<'_, pyo3::types::PyModule>,
) -> pyo3::PyResult<()> {
    use pyo3::prelude::*;
    m.add_class::<cursor::RustDb>()?;
    m.add_class::<cursor::RustConn>()?;
    m.add_class::<cursor::RustCopy>()?;
    m.add_class::<kernel::RustKernel>()?;
    m.add_function(pyo3::wrap_pyfunction!(install_shims, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(export_registry, m)?)?;
    let _ = py;
    Ok(())
}

#[pyo3::pyfunction]
fn export_registry(
    py: pyo3::Python<'_>,
    registry: pyo3::Py<pyo3::PyAny>,
) -> pyo3::PyResult<String> {
    export::export_registry(py, &registry)
}

#[pyo3::pyfunction]
fn install_shims(
    py: pyo3::Python<'_>,
) -> pyo3::PyResult<(
    pyo3::Py<pyo3::types::PyModule>,
    pyo3::Py<pyo3::types::PyModule>,
)> {
    use pyo3::prelude::*;
    let modules = py.import("sys")?.getattr("modules")?;
    let mut out = Vec::new();
    for (name, src) in [
        ("rust_db_shim", include_str!("../python/rust_db_shim.py")),
        ("rust_orm_shim", include_str!("../python/rust_orm_shim.py")),
    ] {
        let module = pyo3::types::PyModule::from_code(
            py,
            &std::ffi::CString::new(src)?,
            &std::ffi::CString::new(format!("{name}.py"))?,
            &std::ffi::CString::new(name)?,
        )?;
        modules.set_item(name, &module)?;
        out.push(module.unbind());
    }
    let orm = out.pop().expect("two modules");
    let db = out.pop().expect("two modules");
    Ok((db, orm))
}
