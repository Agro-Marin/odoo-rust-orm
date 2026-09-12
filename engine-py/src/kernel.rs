use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use pyo3::prelude::*;

use odoo_kernel::orm::{Caches, Orm, Request};
use odoo_kernel::registry::Registry;

use crate::cursor::{RustConn, RustDb};
use crate::errors::{KernelRefused, KernelRegistryStale, from_kernel};

static REGISTRY_GENERATION: AtomicU64 = AtomicU64::new(1);

#[pyclass]
pub struct RustKernel {
    registry: Registry,
    caches: Arc<Caches>,
    generation: u64,
    registry_sequence: i64,
    stale: AtomicBool,
}

impl RustKernel {
    pub fn model_count_pub(&self) -> usize {
        self.registry.models.len()
    }
}

#[pymethods]
impl RustKernel {
    #[staticmethod]
    pub fn build(py: Python<'_>, db: &RustDb, export_json: &str) -> PyResult<Self> {
        crate::logbridge::install();
        let t0 = std::time::Instant::now();
        tracing::info!(
            target: "odoo_kernel::bridge",
            export_bytes = export_json.len(),
            pid = std::process::id(),
            "building the kernel for this worker from the live Python registry export"
        );
        let export: serde_json::Value =
            serde_json::from_str(export_json).map_err(|e| KernelRefused::new_err(e.to_string()))?;
        let registry_sequence = export["registry_sequence"].as_i64().ok_or_else(|| {
            KernelRefused::new_err("export has no registry_sequence; regenerate it")
        })?;
        let conn = db.connect(py, None)?;
        let handle = conn.handle().clone();
        let client = conn.client();
        // Read the model map and security/default watermark from one snapshot.
        // This private transaction never changes the caller's transaction.
        conn.ensure_tx(py)?;
        let registry = py
            .detach(|| handle.block_on(Registry::from_export(&client, &export)))
            .map_err(from_kernel);
        conn.rollback(py)?;
        let registry = registry?;
        let generation = REGISTRY_GENERATION.fetch_add(1, Ordering::SeqCst);
        // The generation is what invalidates a connection's kernel statement
        // cache: plans prepared against the previous registry are dropped on
        // the next dispatch that names a newer one.
        tracing::info!(
            target: "odoo_kernel::bridge",
            models = registry.models.len(),
            registry_sequence,
            generation,
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "kernel built"
        );
        Ok(RustKernel {
            registry,
            caches: Arc::new(Caches::default()),
            generation,
            registry_sequence,
            stale: AtomicBool::new(false),
        })
    }

    #[getter]
    fn model_count(&self) -> usize {
        self.registry.models.len()
    }

    fn dispatch(&self, py: Python<'_>, conn: &RustConn, request_json: &str) -> PyResult<String> {
        if self.stale.load(Ordering::Acquire) {
            // once stale, every call refuses until the registry reloads and
            // the addon publishes a replacement kernel
            tracing::debug!(
                target: "odoo_kernel::bridge",
                generation = self.generation,
                "refusing: this kernel was marked stale by an earlier dispatch"
            );
            return Err(KernelRegistryStale::new_err(
                odoo_kernel::orm::RegistryStale.to_string(),
            ));
        }
        let req: Request = serde_json::from_str(request_json)
            .map_err(|e| KernelRefused::new_err(e.to_string()))?;
        if req
            .registry_sequence
            .is_some_and(|sequence| sequence != self.registry_sequence)
        {
            tracing::debug!(
                target: "odoo_kernel::bridge",
                request = ?req.registry_sequence,
                kernel = self.registry_sequence,
                "refusing: the caller's Python registry is not the one this kernel was built from"
            );
            return Err(KernelRefused::new_err(
                "the request's Python registry does not match this kernel's generation",
            ));
        }
        if conn.autocommit() {
            tracing::debug!(
                target: "odoo_kernel::bridge",
                "refusing: the cursor is in autocommit, so there is no snapshot to read from"
            );
            return Err(KernelRefused::new_err(
                "kernel reads require a repeatable-read transaction",
            ));
        }
        conn.ensure_tx(py)?;
        let client = conn.client();
        let handle = conn.handle().clone();
        let stmts = conn.kernel_stmts_at(self.generation);
        // The GIL is released for the whole dispatch: `detached_ms` minus the
        // kernel's own dispatch time is what another Python thread got back.
        let t0 = std::time::Instant::now();
        let out = py.detach(|| {
            let orm = Orm::new(&self.registry, &client, self.caches.clone(), &stmts);
            let result = handle.block_on(orm.dispatch(&req));
            if result
                .as_ref()
                .err()
                .is_some_and(odoo_kernel::orm::is_registry_stale)
            {
                // The holder of a fresh live Python registry publishes the
                // replacement. Our old export cannot supply new groups/hooks.
                tracing::warn!(
                    target: "odoo_kernel::bridge",
                    generation = self.generation,
                    "the Odoo registry moved under this kernel; marking it stale \
                     and dropping its prepared statements"
                );
                self.stale.store(true, Ordering::Release);
                stmts.clear();
            }
            result.map_err(from_kernel)
        });
        tracing::debug!(
            target: "odoo_kernel::bridge",
            model = %req.model,
            method = %req.method,
            ok = out.is_ok(),
            detached_ms = t0.elapsed().as_secs_f64() * 1000.0,
            "returned to Python"
        );
        out
    }
}
