pub mod cursor;
pub mod errors;
pub mod export;
pub mod kernel;
pub mod logbridge;

use pyo3::prelude::*;
use pyo3::exceptions::PyModuleNotFoundError;
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
    m.add("__source_crc__", env!("ENGINE_PY_SOURCE_CRC"))?;
    m.add("__profile__", env!("ENGINE_PY_PROFILE"))?;
    let _ = py;
    Ok(())
}

#[pyo3::pyfunction]
fn export_registry(py: Python<'_>, registry: Py<PyAny>) -> PyResult<String> {
    export::export_registry(py, &registry)
}

fn register<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyModule>> {
    match py.import(name) {
        Err(err) if is_missing(py, &err, name) => {
            let dir = odoo_kernel::config::engine_python_dir();
            let path = py.import("sys")?.getattr("path")?;
            let entry = dir.display().to_string();
            if !path.contains(&entry)? {
                path.call_method1("append", (&entry,))?;
            }
            py.import(name).map_err(|err| {
                if is_missing(py, &err, name) {
                    PyModuleNotFoundError::new_err(format!(
                        "{name} is engine_py's Python half, loaded from a checkout's \
                         engine-py/python rather than compiled into the extension; it is \
                         neither on sys.path nor in {entry} (RUSTORM_ENGINE_PYTHON)"
                    ))
                } else {
                    err
                }
            })
        }
        imported => imported,
    }
}

fn is_missing(py: Python<'_>, err: &PyErr, name: &str) -> bool {
    err.is_instance_of::<PyModuleNotFoundError>(py)
        && err
            .value(py)
            .getattr("name")
            .and_then(|n| n.extract::<String>())
            .is_ok_and(|n| n == name)
}

pub fn register_purity(py: Python<'_>) -> PyResult<()> {
    register(py, "purity").map(|_| ())
}

pub fn install_backend_py<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyModule>> {
    errors::register(py)?;
    register(py, "rust_backend")
}

pub fn install_shims_py<'py>(
    py: Python<'py>,
) -> PyResult<(Bound<'py, PyModule>, Bound<'py, PyModule>)> {
    errors::register(py)?;
    let db = register(py, "rust_db_shim")?;
    let orm = register(py, "rust_orm_shim")?;
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
