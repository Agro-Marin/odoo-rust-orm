pub mod cursor;
pub mod errors;
pub mod export;
pub mod kernel;
pub mod logbridge;

use pyo3::prelude::*;
use pyo3::types::PyModule;

#[pyo3::pymodule]
fn engine_py(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    logbridge::install();
    let exceptions = errors::register(py)?;
    for name in [
        "KernelRefused",
        "KernelAccessDenied",
        "KernelRegistryStale",
        "KernelDatabaseError",
        "KernelInternalError",
    ] {
        m.add(name, exceptions.getattr(name)?)?;
    }
    m.add_class::<cursor::RustDb>()?;
    m.add_class::<cursor::RustConn>()?;
    m.add_class::<cursor::RustCopy>()?;
    m.add_class::<kernel::RustKernel>()?;
    m.add_function(pyo3::wrap_pyfunction!(install_shims, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(install_backend, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(export_registry, m)?)?;
    let _ = py;
    Ok(())
}

#[pyo3::pyfunction]
fn export_registry(py: Python<'_>, registry: Py<PyAny>) -> PyResult<String> {
    export::export_registry(py, &registry)
}

const SHIM_SOURCES: [(&str, &str); 3] = [
    ("wire", include_str!("../python/wire.py")),
    ("rust_db_shim", include_str!("../python/rust_db_shim.py")),
    ("rust_orm_shim", include_str!("../python/rust_orm_shim.py")),
];

/// The persistence port is registered on its own, not with the shims.
/// It replaces no method: it implements `StorageBackend`, which the fork
/// declares and pins, so a caller can install the port without the method
/// routing or the routing without the port.
const BACKEND_SOURCE: (&str, &str) = ("rust_backend", include_str!("../python/rust_backend.py"));

/// Register one embedded module under `name`, or hand back the one already
/// registered: importing it twice would give the process two copies of the
/// state these modules hold.
fn register<'py>(py: Python<'py>, name: &str, src: &str) -> PyResult<Bound<'py, PyModule>> {
    let modules = py.import("sys")?.getattr("modules")?;
    if let Some(existing) = modules
        .get_item(name)
        .ok()
        .and_then(|m| m.cast_into::<PyModule>().ok())
    {
        return Ok(existing);
    }
    let module = PyModule::from_code(
        py,
        &std::ffi::CString::new(src)?,
        &std::ffi::CString::new(format!("{name}.py"))?,
        &std::ffi::CString::new(name)?,
    )?;
    modules.set_item(name, &module)?;
    Ok(module)
}

pub fn install_backend_py<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyModule>> {
    errors::register(py)?;
    let (wire_name, wire_src) = SHIM_SOURCES[0];
    register(py, wire_name, wire_src)?;
    let (name, src) = BACKEND_SOURCE;
    register(py, name, src)
}

pub fn install_shims_py<'py>(
    py: Python<'py>,
) -> PyResult<(Bound<'py, PyModule>, Bound<'py, PyModule>)> {
    errors::register(py)?;
    let mut out: Vec<Bound<'py, PyModule>> = Vec::new();
    for (name, src) in SHIM_SOURCES {
        let module = register(py, name, src)?;
        if name != "wire" {
            out.push(module);
        }
    }
    let orm = out.pop().expect("two shim modules");
    let db = out.pop().expect("two shim modules");
    Ok((db, orm))
}

#[pyo3::pyfunction]
fn install_shims(py: Python<'_>) -> PyResult<(Py<PyModule>, Py<PyModule>)> {
    let (db, orm) = install_shims_py(py)?;
    Ok((db.unbind(), orm.unbind()))
}

#[pyo3::pyfunction]
fn install_backend(py: Python<'_>) -> PyResult<Py<PyModule>> {
    Ok(install_backend_py(py)?.unbind())
}
