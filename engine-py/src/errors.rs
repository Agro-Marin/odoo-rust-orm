use odoo_kernel::error::ErrorKind;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

pyo3::create_exception!(engine_py, KernelRefused, PyRuntimeError);
pyo3::create_exception!(engine_py, KernelAccessDenied, KernelRefused);
pyo3::create_exception!(engine_py, KernelRegistryStale, KernelRefused);
pyo3::create_exception!(engine_py, KernelDatabaseError, PyRuntimeError);
pyo3::create_exception!(engine_py, KernelInternalError, PyRuntimeError);

pub fn from_kernel(error: anyhow::Error) -> PyErr {
    let message = format!("{error:#}");
    match ErrorKind::of(&error) {
        ErrorKind::Refused => KernelRefused::new_err(message),
        ErrorKind::AccessDenied => KernelAccessDenied::new_err(message),
        ErrorKind::RegistryStale => KernelRegistryStale::new_err(message),
        ErrorKind::Database => KernelDatabaseError::new_err(message),
        ErrorKind::Internal => KernelInternalError::new_err(message),
    }
}

pub fn register(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    let module = PyModule::new(py, "rust_engine_errors")?;
    module.add("KernelRefused", py.get_type::<KernelRefused>())?;
    module.add("KernelAccessDenied", py.get_type::<KernelAccessDenied>())?;
    module.add("KernelRegistryStale", py.get_type::<KernelRegistryStale>())?;
    module.add("KernelDatabaseError", py.get_type::<KernelDatabaseError>())?;
    module.add("KernelInternalError", py.get_type::<KernelInternalError>())?;
    py.import("sys")?
        .getattr("modules")?
        .set_item("rust_engine_errors", &module)?;
    Ok(module)
}
