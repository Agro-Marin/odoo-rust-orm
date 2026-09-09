use std::sync::Arc;

use anyhow::Result;
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

const POOL_SIZE: usize = 16;

const MAX_BODY: usize = 256 * 1024;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

const ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct PooledConn {
    client: Client,
    stmts: StmtCache,
}

struct Pool {
    free: std::sync::Mutex<Vec<Arc<PooledConn>>>,
    permits: Semaphore,
    dsn: String,
}

impl Pool {
    async fn connect(dsn: &str, size: usize) -> Result<Self> {
        let mut free = Vec::with_capacity(size);
        for _ in 0..size {
            let (client, conn) = tokio_postgres::connect(dsn, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            free.push(Arc::new(PooledConn {
                client,
                stmts: StmtCache::default(),
            }));
        }
        Ok(Pool {
            free: std::sync::Mutex::new(free),
            permits: Semaphore::new(size),
            dsn: dsn.to_string(),
        })
    }

    async fn try_acquire(&self) -> Option<Lease<'_>> {
        match tokio::time::timeout(ACQUIRE_TIMEOUT, self.acquire()).await {
            Ok(lease) => Some(lease),
            Err(_) => {
                tracing::warn!("pool saturated for {ACQUIRE_TIMEOUT:?}; shedding");
                None
            }
        }
    }

    async fn acquire(&self) -> Lease<'_> {
        let permit = self.permits.acquire().await.expect("semaphore stays open");
        let mut conn = self
            .free
            .lock()
            .unwrap()
            .pop()
            .expect("a permit guarantees a free connection");
        permit.forget();

        if conn.client.is_closed() {
            tracing::warn!("pooled connection was closed; reconnecting");
            match tokio_postgres::connect(&self.dsn, tokio_postgres::NoTls).await {
                Ok((client, c)) => {
                    tokio::spawn(async move {
                        let _ = c.await;
                    });
                    conn = Arc::new(PooledConn {
                        client,
                        stmts: StmtCache::default(),
                    });
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

enum Auth {
    /// The caller presented the shared secret and may name a `uid`. Whether
    /// it may also claim `su` is a separate decision: the secret says the
    /// caller is trusted to pick an identity, not that every identity it
    /// picks should skip the ACL and the record rules.
    Token {
        secret: String,
        allow_su: bool,
    },

    Pinned {
        uid: i32,
    },
}

struct AppState {
    registry: tokio::sync::RwLock<Arc<Registry>>,

    export: Option<String>,
    pool: Pool,
    caches: Arc<Caches>,
    auth: Auth,
}

/// No short-circuit: the loop runs over the LONGER of the two and the length
/// mismatch is folded into the same accumulator, so the position of the first
/// differing byte is not measurable from the response time. The previous form
/// returned as soon as the lengths differed, before comparing a byte. The
/// loop's length is that of the longer input, so the secret's length itself
/// is the one thing a patient caller could still infer -- the trade every
/// constant-time compare makes.
fn token_matches(given: &str, expected: &str) -> bool {
    let (g, e) = (given.as_bytes(), expected.as_bytes());
    // `!=` and not `(a ^ b) as u8`: the cast would fold a difference that is
    // a multiple of 256 to zero, and the padding below is a zero byte too.
    let mut acc = u8::from(g.len() != e.len());
    for i in 0..g.len().max(e.len()) {
        let a = g.get(i).copied().unwrap_or(0);
        let b = e.get(i).copied().unwrap_or(0);
        acc |= a ^ b;
    }
    acc == 0 && !e.is_empty()
}

/// What the transport lets the body say about WHO is asking. A pinned server
/// overrides it; a token server takes the `uid` and refuses `su` unless the
/// operator opted in, because one shared secret must not be a superuser read
/// of the whole database by default.
fn admit(auth: &Auth, req: &mut Request) -> Result<(), (StatusCode, &'static str)> {
    match auth {
        Auth::Pinned { uid } => {
            req.uid = Some(odoo_kernel::orm::UidSpec::Id(*uid));
            req.su = false;
            Ok(())
        }
        Auth::Token { allow_su, .. } => {
            if req.su && !allow_su {
                return Err((
                    StatusCode::FORBIDDEN,
                    "su is not accepted over HTTP unless the server was started \
                     with RUSTORM_SERVE_ALLOW_SU=1",
                ));
            }
            Ok(())
        }
    }
}

async fn build_registry(client: &tokio_postgres::Client, export: Option<&str>) -> Result<Registry> {
    match export {
        Some(path) => {
            let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            Registry::from_export(client, &value).await
        }
        None => {
            tracing::warn!(
                "no --export given; falling back to the ir_model bootstrap, \
                 which cannot see `search=` fields or x2many field-level \
                 domains. Those reads will be refused or, for search=, \
                 compiled against the raw column."
            );
            Registry::load(client).await
        }
    }
}

pub async fn serve(db: &str, port: u16, export: Option<&str>) -> Result<()> {
    let auth = match std::env::var("RUSTORM_SERVE_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        Some(secret) => {
            let allow_su = std::env::var("RUSTORM_SERVE_ALLOW_SU").is_ok_and(|v| v == "1");
            if allow_su {
                tracing::warn!(
                    "requests must carry X-Rustorm-Token, and RUSTORM_SERVE_ALLOW_SU=1 \
                     lets a caller that has it claim `su`: the ACL and the record rules \
                     are then whatever the caller says they are"
                );
            } else {
                tracing::info!(
                    "requests must carry X-Rustorm-Token; a caller that has it may name \
                     a uid, and `su` is refused (RUSTORM_SERVE_ALLOW_SU=1 to allow it)"
                );
            }
            Auth::Token { secret, allow_su }
        }
        None => {
            tracing::warn!(
                "RUSTORM_SERVE_TOKEN is not set: `uid` and `su` in the request \
                 body are IGNORED and every request runs as uid 2, non-superuser. \
                 Set the variable to let an authenticated caller choose."
            );
            Auth::Pinned { uid: 2 }
        }
    };
    let dsn = odoo_kernel::config::dsn_for(Some(db));
    let pool = Pool::connect(&dsn, POOL_SIZE).await?;

    let registry = {
        let conn = pool.acquire().await;
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
        pool,
        caches: Arc::new(Caches::default()),
        auth,
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
    let Some(conn) = state.pool.try_acquire().await else {
        anyhow::bail!("no database connection available within {ACQUIRE_TIMEOUT:?}");
    };
    let orm = Orm::new(&registry, &conn.client, state.caches.clone(), &conn.stmts);
    match tokio::time::timeout(REQUEST_TIMEOUT, orm.dispatch_in_transaction(req)).await {
        Ok(out) => out,
        Err(_) => anyhow::bail!("request exceeded {REQUEST_TIMEOUT:?}"),
    }
}

async fn handle_health(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let registry = state.registry.read().await.clone();
    let ok = {
        let Some(conn) = state.pool.try_acquire().await else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                AxJson(json!({"status": "saturated"})),
            )
                .into_response();
        };
        conn.client.simple_query("SELECT 1").await.is_ok()
    };
    let body = json!({
        "status": if ok { "ok" } else { "degraded" },
        "models": registry.models.len(),
        "source": format!("{:?}", registry.source),
        "pool": POOL_SIZE,
    });
    let code = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, AxJson(body)).into_response()
}

async fn rebuild_registry(state: &AppState) -> Result<usize> {
    let fresh = {
        let conn = state.pool.acquire().await;
        build_registry(&conn.client, state.export.as_deref()).await?
    };
    let models = fresh.models.len();

    state.caches.clear().await;
    *state.registry.write().await = Arc::new(fresh);
    Ok(models)
}

async fn handle_call(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    AxJson(req): AxJson<Request>,
) -> axum::response::Response {
    let mut req = req;
    if let Auth::Token { secret, .. } = &state.auth {
        let given = headers
            .get("x-rustorm-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !token_matches(given, secret) {
            return (
                StatusCode::UNAUTHORIZED,
                AxJson(json!({"error": "missing or wrong X-Rustorm-Token"})),
            )
                .into_response();
        }
    }
    if let Err((code, why)) = admit(&state.auth, &mut req) {
        return (code, AxJson(json!({"error": why}))).into_response();
    }

    let mut result = dispatch_once(&state, &req).await;
    if result
        .as_ref()
        .err()
        .is_some_and(odoo_kernel::orm::is_registry_stale)
    {
        match rebuild_registry(&state).await {
            Ok(models) => {
                tracing::info!(models, "registry rebuilt after a change; retrying");
                result = dispatch_once(&state, &req).await;
            }
            Err(e) => tracing::error!(error = %format!("{e:#}"), "registry rebuild failed"),
        }
    }

    match result {
        Ok(raw) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            format!(r#"{{"result":{raw}}}"#),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            AxJson(json!({"error": format!("{e:#}")})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: serde_json::Value) -> Request {
        serde_json::from_value(body).expect("a well-formed request")
    }

    #[test]
    fn the_token_compare_does_not_short_circuit_on_length() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abd", "abc"));
        assert!(!token_matches("ab", "abc"), "a prefix is not the secret");
        assert!(
            !token_matches("abcd", "abc"),
            "nor is the secret plus one byte"
        );
        assert!(!token_matches("", "abc"));
        assert!(!token_matches("", ""), "an empty secret admits nobody");
    }

    #[test]
    fn a_pinned_server_ignores_whatever_the_body_says() {
        let auth = Auth::Pinned { uid: 2 };
        let mut req = request(serde_json::json!({
            "model": "res.partner", "method": "search_count", "uid": 1, "su": true
        }));
        admit(&auth, &mut req).expect("pinned never refuses");
        assert!(matches!(req.uid, Some(odoo_kernel::orm::UidSpec::Id(2))));
        assert!(!req.su);
    }

    #[test]
    fn a_token_server_refuses_su_unless_the_operator_allowed_it() {
        let mut req = request(serde_json::json!({
            "model": "res.partner", "method": "search_count", "uid": 7, "su": true
        }));
        let closed = Auth::Token {
            secret: "s".into(),
            allow_su: false,
        };
        let (code, why) = admit(&closed, &mut req).expect_err("su refused by default");
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(why.contains("RUSTORM_SERVE_ALLOW_SU"));
        assert!(
            matches!(req.uid, Some(odoo_kernel::orm::UidSpec::Id(7))),
            "uid untouched"
        );

        let open = Auth::Token {
            secret: "s".into(),
            allow_su: true,
        };
        admit(&open, &mut req).expect("opted in");
        assert!(req.su);

        let mut plain = request(serde_json::json!({
            "model": "res.partner", "method": "search_count", "uid": 7
        }));
        admit(&closed, &mut plain).expect("a uid without su is what the token is for");
        assert!(!plain.su);
    }
}
