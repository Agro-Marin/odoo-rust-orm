use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures_util::FutureExt;
use pyo3::prelude::*;

use odoo_kernel::orm::{Caches, Orm, Request};
use odoo_kernel::registry::Registry;

use crate::cursor::{RustConn, RustDb};
use crate::errors::{KernelRefused, KernelRegistryStale, from_kernel};

static REGISTRY_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The savepoint every dispatch reads inside. One name serves every dispatch
/// on a connection: they do not nest, and each releases or rolls back its own
/// before returning.
const SAVEPOINT_OPEN: &str = "SAVEPOINT rust_kernel_dispatch";
const SAVEPOINT_RELEASE: &str = "RELEASE SAVEPOINT rust_kernel_dispatch";
const SAVEPOINT_UNDO: &str =
    "ROLLBACK TO SAVEPOINT rust_kernel_dispatch; RELEASE SAVEPOINT rust_kernel_dispatch";

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
        let client = conn.client()?;
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

    /// The `UPDATE` `PostgresBackend.update_rows` would compose, composed
    /// from this kernel's registry instead.
    ///
    /// It is returned rather than executed: the caller binds the same
    /// parameters and runs it on its own cursor, so the statement reaches
    /// PostgreSQL through the same logging, metrics and savepoints as every
    /// other write. What moves here is the composition, which is the part
    /// decided by field metadata.
    ///
    /// `value_repeats` says how many parameters each column's value
    /// contributes, because a whole-value translated column binds its value
    /// three times -- the merge expression names it three times.
    #[pyo3(signature = (model, fnames, uniform, row_count))]
    fn update_rows_sql(
        &self,
        model: &str,
        fnames: Vec<String>,
        uniform: bool,
        row_count: usize,
    ) -> PyResult<(String, Vec<usize>)> {
        if self.stale.load(Ordering::Acquire) {
            return Err(KernelRegistryStale::new_err(
                odoo_kernel::orm::RegistryStale.to_string(),
            ));
        }
        let shape = if uniform {
            odoo_kernel::write::UpdateShape::Uniform
        } else {
            odoo_kernel::write::UpdateShape::Values
        };
        let sql =
            odoo_kernel::write::update_rows_sql(&self.registry, model, &fnames, shape, row_count)
                .map_err(from_kernel)?;
        let repeats = fnames
            .iter()
            .map(|fname| {
                self.registry
                    .lookup(model)
                    .and_then(|m| m.fields.get(fname))
                    .map_or(1, odoo_kernel::write::value_repeats)
            })
            .collect();
        Ok((sql, repeats))
    }

    /// The `INSERT` `PostgresBackend.create_rows` would run on its INSERT
    /// strategy, composed from this kernel's registry. Returned, not executed,
    /// for the same reason as `update_rows_sql`.
    #[pyo3(signature = (model, columns, row_count))]
    fn insert_rows_sql(
        &self,
        model: &str,
        columns: Vec<String>,
        row_count: usize,
    ) -> PyResult<String> {
        if self.stale.load(Ordering::Acquire) {
            return Err(KernelRegistryStale::new_err(
                odoo_kernel::orm::RegistryStale.to_string(),
            ));
        }
        odoo_kernel::write::insert_rows_sql(&self.registry, model, &columns, row_count)
            .map_err(from_kernel)
    }

    /// The WHERE fragment `StorageBackend.search` would add, compiled on the
    /// caller's connection as `(sql, payload_json)` in Odoo's `%s` dialect,
    /// the payload carrying `params` and `touched` -- the (model, field) pairs
    /// the fragment reads, which the caller flushes. See `Orm::compile_where`.
    ///
    /// `offline` asks for an answer that sends no statement, and returns None
    /// when one is needed -- a watermark not yet checked in this transaction,
    /// an identity or rule set not yet cached, a hierarchy lookup. The caller
    /// asks again online inside a savepoint. The watermark a compile verifies
    /// is kept on the connection for the rest of its transaction, so after the
    /// first search of a transaction the offline answer is the usual one.
    ///
    /// The same guards as `dispatch`: a stale kernel, another Python registry
    /// and an autocommit cursor all refuse.
    #[pyo3(signature = (conn, request_json, offline))]
    fn search_where(
        &self,
        py: Python<'_>,
        conn: &RustConn,
        request_json: &str,
        offline: bool,
    ) -> PyResult<Option<(String, String)>> {
        if self.stale.load(Ordering::Acquire) {
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
            return Err(KernelRefused::new_err(
                "the request's Python registry does not match this kernel's generation",
            ));
        }
        if conn.autocommit() {
            return Err(KernelRefused::new_err(
                "kernel reads require a repeatable-read transaction",
            ));
        }
        // Opening the transaction is a statement, and it is sent offline too:
        // the query this fragment ends up in would send the same BEGIN, so it
        // costs nothing Python's own path does not, and a BEGIN that fails has
        // broken the connection for Python as well -- there is nothing a
        // savepoint could roll back to. What offline withholds is every
        // statement that is the KERNEL's.
        conn.ensure_tx(py)?;
        let checked = conn.checked_signals(self.generation);
        let client = conn.client()?;
        let handle = conn.handle().clone();
        let stmts = conn.kernel_stmts_at(self.generation);
        py.detach(|| {
            let mut orm = Orm::new(&self.registry, &client, self.caches.clone(), &stmts);
            if offline {
                orm = orm.offline();
            }
            let result = handle.block_on(orm.compile_where(&req, checked.clone()));
            if let Err(e) = &result {
                if odoo_kernel::error::needs_round_trip(e) {
                    return Ok(None);
                }
                if odoo_kernel::orm::is_registry_stale(e) {
                    self.stale.store(true, Ordering::Release);
                    stmts.clear();
                }
            }
            let compiled = result.map_err(from_kernel)?;
            if checked.is_none() {
                conn.remember_signals(self.generation, compiled.snapshot);
            }
            let payload = serde_json::json!({
                "params": compiled.params,
                "touched": compiled.touched,
            });
            let payload = serde_json::to_string(&payload)
                .map_err(|e| KernelRefused::new_err(e.to_string()))?;
            Ok(Some((compiled.fragment, payload)))
        })
    }

    #[getter]
    fn registry_sequence(&self) -> i64 {
        self.registry_sequence
    }

    fn dispatch(&self, py: Python<'_>, conn: &RustConn, request_json: &str) -> PyResult<String> {
        if self.stale.load(Ordering::Acquire) {
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
        let checked = conn.checked_signals(self.generation);
        let client = conn.client()?;
        let handle = conn.handle().clone();
        let stmts = conn.kernel_stmts_at(self.generation);
        // The GIL is released for the whole dispatch: `detached_ms` minus the
        // kernel's own dispatch time is what another Python thread got back.
        let t0 = std::time::Instant::now();
        let out = py.detach(|| {
            // A failed statement must not abort the caller's transaction, or
            // Python could not answer the call the kernel gave up on, so the
            // dispatch runs inside a savepoint. SAVEPOINT and, on success,
            // RELEASE are only enqueued: tokio-postgres writes requests in the
            // order they are queued and pages past the replies of a dropped
            // future, so the kernel's first query goes out without waiting on
            // SAVEPOINT and Python's next statement queues behind RELEASE.
            // Python's own `cr.savepoint()` waited on both, two round trips of
            // a routed call whose query is one.
            let _ = client.batch_execute(SAVEPOINT_OPEN).now_or_never();
            let orm = Orm::new(&self.registry, &client, self.caches.clone(), &stmts);
            let result = handle.block_on(orm.dispatch_with(&req, checked.clone()));
            if result.is_ok() {
                let _ = client.batch_execute(SAVEPOINT_RELEASE).now_or_never();
            } else {
                // waited on: the caller's next statement needs the
                // transaction usable again
                let undone = handle.block_on(client.batch_execute(SAVEPOINT_UNDO));
                conn.clear_prepared_after_rollback();
                if let Err(e) = undone {
                    tracing::warn!(
                        target: "odoo_kernel::bridge",
                        error = %e,
                        "could not roll back the kernel's savepoint; the caller's \
                         transaction may be aborted"
                    );
                }
            }
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
            let (raw, snapshot) = result.map_err(from_kernel)?;
            // The signalling watermark cannot move inside the caller's
            // REPEATABLE READ transaction; the next dispatch in it reuses the
            // snapshot this one checked instead of reading the row again.
            if checked.is_none() {
                conn.remember_signals(self.generation, snapshot);
            }
            Ok(raw)
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
