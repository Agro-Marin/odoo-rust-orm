//! The server's connection pool: a fixed set of `tokio-postgres` clients, a
//! lease that hands one back on drop, and the rule that a connection whose
//! transaction could not be ended is replaced rather than reused.
//!
//! Split out of `http.rs` so the pool's own tests sit at the end of the
//! pool's own file; the transport keeps the routes, the identity and the
//! dispatch.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio_postgres::Client;

use odoo_kernel::orm::StmtCache;

pub(crate) const ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) struct PooledConn {
    pub(crate) client: Client,
    pub(crate) stmts: StmtCache,
    /// Set when a request left the connection in a transaction state nobody
    /// can name -- its COMMIT/ROLLBACK failed, or it was abandoned on the
    /// request timeout with a statement still running. The next `acquire`
    /// replaces it instead of handing the next request a snapshot, or an
    /// aborted transaction, that belongs to the previous one.
    poisoned: std::sync::atomic::AtomicBool,
}

impl PooledConn {
    fn fresh(client: Client) -> Self {
        PooledConn {
            client,
            stmts: StmtCache::default(),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn poison(&self, why: &str) {
        tracing::warn!(why, "discarding the pooled connection after this request");
        self.poisoned
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub(crate) struct Pool {
    free: std::sync::Mutex<Vec<Arc<PooledConn>>>,
    permits: Semaphore,
    dsn: String,
}

impl Pool {
    pub(crate) async fn connect(dsn: &str, size: usize) -> Result<Self> {
        let mut free = Vec::with_capacity(size);
        for _ in 0..size {
            let (client, conn) = tokio_postgres::connect(dsn, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            free.push(Arc::new(PooledConn::fresh(client)));
        }
        Ok(Pool {
            free: std::sync::Mutex::new(free),
            permits: Semaphore::new(size),
            dsn: dsn.to_string(),
        })
    }

    pub(crate) async fn try_acquire(&self) -> Option<Lease<'_>> {
        match tokio::time::timeout(ACQUIRE_TIMEOUT, self.acquire()).await {
            Ok(lease) => Some(lease),
            Err(_) => {
                tracing::warn!("pool saturated for {ACQUIRE_TIMEOUT:?}; shedding");
                None
            }
        }
    }

    pub(crate) async fn acquire(&self) -> Lease<'_> {
        let permit = self.permits.acquire().await.expect("semaphore stays open");
        let mut conn = self
            .free
            .lock()
            .unwrap()
            .pop()
            .expect("a permit guarantees a free connection");
        permit.forget();

        if conn.client.is_closed() || conn.is_poisoned() {
            tracing::warn!(
                poisoned = conn.is_poisoned(),
                "pooled connection is unusable; reconnecting"
            );
            match tokio_postgres::connect(&self.dsn, tokio_postgres::NoTls).await {
                Ok((client, c)) => {
                    tokio::spawn(async move {
                        let _ = c.await;
                    });
                    conn = Arc::new(PooledConn::fresh(client));
                }
                Err(e) => tracing::error!(error = %e, "reconnect failed"),
            }
        }
        Lease {
            pool: self,
            conn: Some(conn),
        }
    }

    fn release(&self, conn: Arc<PooledConn>) {
        self.free.lock().unwrap().push(conn);
        self.permits.add_permits(1);
    }
}

pub(crate) struct Lease<'p> {
    pool: &'p Pool,
    conn: Option<Arc<PooledConn>>,
}

impl std::ops::Deref for Lease<'_> {
    type Target = PooledConn;
    fn deref(&self) -> &PooledConn {
        self.conn.as_ref().expect("held until drop")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.release(conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dsn() -> String {
        std::env::var("RUSTORM_TEST_DSN")
            .expect("RUSTORM_TEST_DSN names a database this test may use")
    }

    #[tokio::test]
    #[ignore = "needs RUSTORM_TEST_DSN"]
    async fn a_poisoned_connection_is_replaced_and_a_healthy_one_is_reused() {
        let pool = Pool::connect(&dsn(), 1).await.expect("connect");

        let first = pool.acquire().await;
        let first_ptr = Arc::as_ptr(first.conn.as_ref().unwrap());
        drop(first);
        let again = pool.acquire().await;
        assert_eq!(
            Arc::as_ptr(again.conn.as_ref().unwrap()),
            first_ptr,
            "a healthy connection goes back to the pool and comes out again"
        );

        again.poison("the test says so");
        assert!(again.is_poisoned());
        drop(again);
        let fresh = pool.acquire().await;
        assert_ne!(
            Arc::as_ptr(fresh.conn.as_ref().unwrap()),
            first_ptr,
            "a poisoned connection is replaced before it is handed out"
        );
        assert!(!fresh.is_poisoned(), "the replacement starts clean");
        fresh
            .client
            .simple_query("SELECT 1")
            .await
            .expect("and it works");
    }
}
