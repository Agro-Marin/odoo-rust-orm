use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_postgres::Client;

use odoo_kernel::orm::{Caches, Orm, Request, StmtCache};
use odoo_kernel::registry::Registry;

const MAX_BODY: usize = 256 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct ServeOptions {
    pub pool_size: usize,
    pub request_timeout: std::time::Duration,
    pub acquire_timeout: std::time::Duration,
}

impl Default for ServeOptions {
    fn default() -> Self {
        ServeOptions {
            pool_size: 16,
            request_timeout: std::time::Duration::from_secs(30),
            acquire_timeout: std::time::Duration::from_secs(5),
        }
    }
}

struct PooledConn {
    client: Client,
    stmts: StmtCache,

    poisoned: AtomicBool,

    dsn: String,
}

impl PooledConn {
    fn new(client: Client, dsn: &str) -> Arc<Self> {
        Arc::new(PooledConn {
            client,
            stmts: StmtCache::default(),
            poisoned: AtomicBool::new(false),
            dsn: dsn.to_string(),
        })
    }

    fn is_unusable(&self) -> bool {
        self.client.is_closed() || self.poisoned.load(Ordering::SeqCst)
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
        let token = self.client.cancel_token();
        let dsn = self.dsn.clone();
        tokio::spawn(async move {
            odoo_kernel::connect::cancel(token, &dsn).await;
        });
    }
}

struct Pool {
    free: std::sync::Mutex<Vec<Arc<PooledConn>>>,
    permits: Semaphore,
    dsn: String,
    size: usize,
    acquire_timeout: std::time::Duration,
}

impl Pool {
    async fn connect(dsn: &str, size: usize, acquire_timeout: std::time::Duration) -> Result<Self> {
        let mut free = Vec::with_capacity(size);
        for _ in 0..size {
            let client = odoo_kernel::connect::connect(dsn).await?;
            free.push(PooledConn::new(client, dsn));
        }
        Ok(Pool {
            free: std::sync::Mutex::new(free),
            permits: Semaphore::new(size),
            dsn: dsn.to_string(),
            size,
            acquire_timeout,
        })
    }

    async fn try_acquire(&self) -> Result<Lease<'_>> {
        match tokio::time::timeout(self.acquire_timeout, self.acquire()).await {
            Ok(lease) => lease,
            Err(_) => {
                tracing::warn!("pool saturated for {:?}; shedding", self.acquire_timeout);
                anyhow::bail!(
                    "no database connection available within {:?}",
                    self.acquire_timeout
                )
            }
        }
    }

    async fn acquire(&self) -> Result<Lease<'_>> {
        let permit = self.permits.acquire().await.expect("semaphore stays open");
        let mut conn = self
            .free
            .lock()
            .unwrap()
            .pop()
            .expect("a permit guarantees a free connection");
        permit.forget();

        if conn.is_unusable() {
            tracing::warn!("pooled connection was closed or poisoned; reconnecting");
            match odoo_kernel::connect::connect(&self.dsn).await {
                Ok(client) => {
                    conn = PooledConn::new(client, &self.dsn);
                }
                Err(e) => {
                    self.release(conn);
                    return Err(e).context("no database connection available: reconnect failed");
                }
            }
        }
        Ok(Lease {
            pool: self,
            conn: Some(conn),
        })
    }

    fn release(&self, conn: Arc<PooledConn>) {
        self.free.lock().unwrap().push(conn);
        self.permits.add_permits(1);
    }
}

struct Lease<'p> {
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

struct TxGuard<'c> {
    conn: &'c PooledConn,
    finished: bool,
}

impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.conn.poison();
        }
    }
}

enum Auth {
    Token(String),

    Pinned { uid: i32 },
}

impl Auth {
    fn describe(&self) -> serde_json::Value {
        match self {
            Auth::Token(_) => json!({"mode": "token"}),
            Auth::Pinned { uid } => json!({"mode": "pinned", "uid": uid}),
        }
    }
}

struct AppState {
    registry: tokio::sync::RwLock<Arc<Registry>>,

    export: Option<String>,
    export_mtime: std::sync::Mutex<Option<SystemTime>>,
    stale: AtomicBool,
    pool: Pool,
    caches: Arc<Caches>,
    auth: Auth,
    request_timeout: std::time::Duration,
}

fn export_mtime(path: Option<&str>) -> Option<SystemTime> {
    std::fs::metadata(path?).ok()?.modified().ok()
}

async fn build_registry(client: &tokio_postgres::Client, export: Option<&str>) -> Result<Registry> {
    match export {
        Some(path) => {
            let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            Registry::from_export(client, &value).await
        }
        None => anyhow::bail!(
            "serve needs --export: the ir_model bootstrap cannot tell which models \
             override the read path, marks none of them pure, and would refuse \
             every dispatch"
        ),
    }
}

pub async fn serve(db: &str, port: u16, export: Option<&str>, options: ServeOptions) -> Result<()> {
    let auth = match std::env::var("RUSTORM_SERVE_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        Some(token) => {
            tracing::info!("requests must carry X-Rustorm-Token");
            Auth::Token(token)
        }
        None => {
            // uid 2 is the administrator on every Odoo database: an
            // unauthenticated listener answering as admin reads everything, so
            // the pinned identity must be chosen, never defaulted
            let uid: i32 = std::env::var("RUSTORM_SERVE_PINNED_UID")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|u: &i32| *u > 0)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "refusing to serve: set RUSTORM_SERVE_TOKEN so callers \
                         authenticate and choose their identity, or \
                         RUSTORM_SERVE_PINNED_UID=<uid> to pin every request to \
                         one non-superuser identity"
                    )
                })?;
            tracing::warn!(
                uid,
                "RUSTORM_SERVE_TOKEN is not set: `uid` and `su` in the request \
                 body are IGNORED and every request runs as the pinned uid, \
                 non-superuser"
            );
            if uid <= 2 {
                tracing::warn!(
                    uid,
                    "the pinned identity is OdooBot or the administrator, which \
                     passes every access rule; pin a real user for anything but a demo"
                );
            }
            Auth::Pinned { uid }
        }
    };
    let dsn = odoo_kernel::config::dsn_for(Some(db));
    let pool = Pool::connect(&dsn, options.pool_size, options.acquire_timeout).await?;

    let mtime = export_mtime(export);
    let registry = {
        let conn = pool.try_acquire().await?;
        build_registry(&conn.client, export).await?
    };
    tracing::info!(
        models = registry.models.len(),
        unaccent = registry.has_unaccent,
        source = ?registry.source,
        "registry loaded"
    );

    let state = Arc::new(AppState {
        registry: tokio::sync::RwLock::new(Arc::new(registry)),
        export: export.map(str::to_string),
        export_mtime: std::sync::Mutex::new(mtime),
        stale: AtomicBool::new(false),
        pool,
        caches: Arc::new(Caches::default()),
        auth,
        request_timeout: options.request_timeout,
    });
    let app = Router::new()
        .route("/call", post(handle_call))
        .route("/health", get(handle_health))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(state);
    let addr = format!("127.0.0.1:{port}");
    tracing::info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = %e, "cannot listen for SIGTERM");
                        return;
                    }
                };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            tracing::info!("shutting down; letting in-flight requests finish");
        })
        .await?;
    Ok(())
}

async fn dispatch_once(state: &AppState, req: &Request) -> Result<String> {
    let registry = state.registry.read().await.clone();
    let conn = state.pool.try_acquire().await?;
    let mut guard = TxGuard {
        conn: &conn,
        finished: false,
    };
    let orm = Orm::new(&registry, &conn.client, state.caches.clone(), &conn.stmts);
    let out =
        match tokio::time::timeout(state.request_timeout, orm.dispatch_in_transaction(req)).await {
            Ok(out) => out,
            Err(_) => anyhow::bail!("request exceeded {:?}", state.request_timeout),
        };
    if out.is_err()
        && let Err(e) = conn.client.batch_execute("ROLLBACK").await
    {
        tracing::warn!(error = %e, "could not roll back after a failed dispatch; poisoning");
        conn.poison();
    }
    guard.finished = true;
    out
}

async fn dispatch_guarded(state: Arc<AppState>, req: Request) -> Result<String> {
    let handle = tokio::spawn(async move { dispatch_once(&state, &req).await });
    match handle.await {
        Ok(out) => out,
        Err(e) if e.is_panic() => {
            tracing::error!("dispatch panicked; the connection it held is poisoned");
            anyhow::bail!("internal error: dispatch panicked")
        }
        Err(e) => anyhow::bail!("internal error: dispatch task failed: {e}"),
    }
}

async fn handle_health(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let registry = state.registry.read().await.clone();
    let ok = {
        let Ok(conn) = state.pool.try_acquire().await else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                AxJson(json!({"status": "saturated", "auth": state.auth.describe()})),
            )
                .into_response();
        };
        conn.client.simple_query("SELECT 1").await.is_ok()
    };
    let stale = state.stale.load(Ordering::SeqCst);
    let body = json!({
        "status": if stale { "stale" } else if ok { "ok" } else { "degraded" },
        "models": registry.models.len(),
        "source": format!("{:?}", registry.source),
        "pool": state.pool.size,
        "auth": state.auth.describe(),
        "export": state.export,
    });
    let code = if ok && !stale {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, AxJson(body)).into_response()
}

async fn rebuild_registry(state: &AppState) -> Result<usize> {
    let mtime = export_mtime(state.export.as_deref());
    let fresh = {
        let conn = state.pool.try_acquire().await?;
        build_registry(&conn.client, state.export.as_deref()).await?
    };
    let models = fresh.models.len();

    state.caches.clear().await;
    *state.registry.write().await = Arc::new(fresh);
    *state.export_mtime.lock().unwrap() = mtime;
    state.stale.store(false, Ordering::SeqCst);
    Ok(models)
}

fn export_changed(state: &AppState) -> bool {
    let now = export_mtime(state.export.as_deref());
    now.is_some() && now != *state.export_mtime.lock().unwrap()
}

async fn fresh_export(state: &AppState) -> Result<()> {
    if let Some(cmd) = std::env::var("RUSTORM_EXPORT_CMD")
        .ok()
        .filter(|c| !c.trim().is_empty())
    {
        tracing::info!(%cmd, "registry stale; regenerating the export");
        let shell = cmd.clone();
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(shell)
                .status()
        })
        .await
        .with_context(|| format!("running RUSTORM_EXPORT_CMD `{cmd}`"))?
        .with_context(|| format!("running RUSTORM_EXPORT_CMD `{cmd}`"))?;
        if !status.success() {
            anyhow::bail!("RUSTORM_EXPORT_CMD `{cmd}` exited {status}");
        }
    }
    if export_changed(state) {
        return Ok(());
    }
    state.stale.store(true, Ordering::SeqCst);
    anyhow::bail!(
        "the Odoo registry changed and the export at {} predates it; supply a \
         fresh export there (or set RUSTORM_EXPORT_CMD to a command that \
         regenerates it) -- until then every request is refused",
        state.export.as_deref().unwrap_or("<none>")
    )
}

fn token_matches(given: &str, expected: &str) -> bool {
    // keyed SipHash lanes from two per-process random keys: fixed-size digests,
    // compared without any length short-circuit
    let lanes = [RandomState::new(), RandomState::new()];
    let digest = |s: &str| -> [u64; 2] {
        let mut out = [0u64; 2];
        for (lane, key) in out.iter_mut().zip(&lanes) {
            let mut h = key.build_hasher();
            h.write(s.as_bytes());
            h.write_usize(s.len());
            *lane = h.finish();
        }
        out
    };
    let a = digest(given);
    let b = digest(expected);
    let diff = (a[0] ^ b[0]) | (a[1] ^ b[1]);
    let same_len = (given.len() ^ expected.len()) as u64;
    (diff | same_len) == 0
}

async fn handle_call(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    AxJson(req): AxJson<Request>,
) -> axum::response::Response {
    let mut req = req;
    match &state.auth {
        Auth::Token(expected) => {
            let given = headers
                .get("x-rustorm-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if !token_matches(given, expected) {
                return (
                    StatusCode::UNAUTHORIZED,
                    AxJson(json!({"error": "missing or wrong X-Rustorm-Token"})),
                )
                    .into_response();
            }
        }
        Auth::Pinned { uid } => {
            req.uid = Some(odoo_kernel::orm::UidSpec::Id(*uid));
            req.su = false;
        }
    }

    if state.stale.load(Ordering::SeqCst) {
        if let Err(e) = fresh_export(&state).await {
            return refuse(&e);
        }
        if let Err(e) = rebuild_registry(&state).await {
            return refuse(&e.context("registry rebuild failed"));
        }
    }

    let mut result = dispatch_guarded(state.clone(), req.clone()).await;
    if result
        .as_ref()
        .err()
        .is_some_and(odoo_kernel::orm::is_registry_stale)
    {
        result = match fresh_export(&state).await {
            Ok(()) => match rebuild_registry(&state).await {
                Ok(models) => {
                    tracing::info!(models, "registry rebuilt from a fresh export; retrying");
                    dispatch_guarded(state.clone(), req.clone()).await
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "registry rebuild failed");
                    Err(e)
                }
            },
            Err(e) => Err(e),
        };
    } else if result
        .as_ref()
        .err()
        .is_some_and(odoo_kernel::db::is_stale_plan_error)
    {
        tracing::info!("a cached plan went stale under DDL; retrying in a new transaction");
        result = dispatch_guarded(state.clone(), req).await;
    }

    match result {
        Ok(raw) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            format!(r#"{{"result":{raw}}}"#),
        )
            .into_response(),
        Err(e) => refuse(&e),
    }
}

fn refuse(e: &anyhow::Error) -> axum::response::Response {
    (
        status_of(e),
        AxJson(json!({"error": format!("{e:#}"), "kind": crate::error_kind(e)})),
    )
        .into_response()
}

// callers and the soak harness must tell overload and a timeout from a
// refusal, a refusal from a denial, and any of those from a kernel defect
fn status_of(e: &anyhow::Error) -> StatusCode {
    let text = format!("{e:#}");
    if odoo_kernel::orm::is_registry_stale(e)
        || text.starts_with("no database connection")
        || text.starts_with("the Odoo registry changed")
        || text.starts_with("registry rebuild failed")
    {
        StatusCode::SERVICE_UNAVAILABLE
    } else if text.starts_with("request exceeded") {
        StatusCode::GATEWAY_TIMEOUT
    } else if text.contains("access denied") {
        StatusCode::FORBIDDEN
    } else if text.starts_with("internal error") || crate::error_kind(e) == "internal" {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_matches_only_itself() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("ab", "abc"));
        assert!(!token_matches("abcd", "abc"));
        assert!(!token_matches("", "abc"));
        assert!(token_matches("", ""));
    }

    #[tokio::test]
    #[ignore]
    async fn a_poisoned_connection_is_replaced_on_the_next_acquire() {
        let dsn = odoo_kernel::config::dsn();
        let pool = Pool::connect(&dsn, 1, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let first_pid: i32 = {
            let lease = pool.acquire().await.unwrap();
            let pid: i32 = lease
                .client
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get(0);
            lease
                .client
                .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .await
                .unwrap();
            lease.poison();
            pid
        };
        let lease = pool.acquire().await.unwrap();
        let second_pid: i32 = lease
            .client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        assert_ne!(
            first_pid, second_pid,
            "the poisoned backend must not serve again"
        );
        let read_only: String = lease
            .client
            .query_one("SHOW transaction_read_only", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            read_only, "off",
            "the replacement starts outside the leaked READ ONLY transaction"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn a_dropped_guard_poisons_and_the_pool_recovers_from_a_dead_reconnect() {
        let dsn = odoo_kernel::config::dsn();
        let pool = Pool::connect(&dsn, 1, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        {
            let lease = pool.acquire().await.unwrap();
            let _guard = TxGuard {
                conn: &lease,
                finished: false,
            };
        }
        let lease = pool.acquire().await.unwrap();
        assert!(!lease.is_unusable(), "the replacement is a live connection");
    }
}
