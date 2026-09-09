use std::sync::Arc;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use tokio::runtime::Handle;

use odoo_kernel::orm::{Caches, Orm, Request};
use odoo_kernel::registry::Registry;

use crate::cursor::{RustConn, RustDb};

fn rerr(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(format!("{e}"))
}

impl RustKernel {
    fn rebuild(&self, client: &tokio_postgres::Client) -> anyhow::Result<Arc<Registry>> {
        let export: serde_json::Value = serde_json::from_str(&self.export)?;
        let fresh = Arc::new(
            self.handle
                .block_on(Registry::from_export(client, &export))?,
        );

        self.handle.block_on(self.caches.clear());
        *self.registry.write().unwrap() = fresh.clone();
        tracing::info!(models = fresh.models.len(), "kernel registry rebuilt");
        Ok(fresh)
    }
}

#[pyclass]
pub struct RustKernel {
    registry: std::sync::RwLock<Arc<Registry>>,

    export: String,
    caches: Arc<Caches>,
    handle: Handle,
}

impl RustKernel {
    pub fn model_count_pub(&self) -> usize {
        self.registry.read().unwrap().models.len()
    }
}

#[pymethods]
impl RustKernel {
    #[staticmethod]
    pub fn build(py: Python<'_>, db: &RustDb, export_json: &str) -> PyResult<Self> {
        crate::logbridge::install();
        let export: serde_json::Value = serde_json::from_str(export_json).map_err(rerr)?;
        let conn = db.connect(py, None)?;
        let handle = conn.handle().clone();
        let client = conn.client();
        let registry = py
            .detach(|| handle.block_on(Registry::from_export(&client, &export)))
            .map_err(rerr)?;
        Ok(RustKernel {
            registry: std::sync::RwLock::new(Arc::new(registry)),
            export: export_json.to_string(),
            caches: Arc::new(Caches::default()),
            handle,
        })
    }

    #[getter]
    fn model_count(&self) -> usize {
        self.registry.read().unwrap().models.len()
    }

    fn dispatch(&self, py: Python<'_>, conn: &RustConn, request_json: &str) -> PyResult<String> {
        let req: Request = serde_json::from_str(request_json).map_err(rerr)?;
        let client = conn.client();
        let stmts = conn.kernel_stmts();
        py.detach(|| {
            let run = |registry: &Registry| {
                let orm = Orm::new(registry, &client, self.caches.clone(), &stmts);
                self.handle.block_on(orm.dispatch(&req))
            };
            let registry = self.registry.read().unwrap().clone();
            let out = run(&registry);
            let Err(e) = out else {
                return out.map_err(|e| rerr(format!("{e:#}")));
            };
            if !odoo_kernel::orm::is_registry_stale(&e) {
                return Err(rerr(format!("{e:#}")));
            }

            match self.rebuild(&client) {
                Ok(fresh) => run(&fresh).map_err(|e| rerr(format!("{e:#}"))),
                Err(re) => Err(rerr(format!("{e:#} (rebuild also failed: {re:#})"))),
            }
        })
    }
}
