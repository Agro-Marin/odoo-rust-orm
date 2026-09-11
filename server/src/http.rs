use std::sync::Arc;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use serde_json::json;

use odoo_kernel::orm::{Caches, Orm, Request};
use odoo_kernel::registry::Registry;

use crate::pool::{ACQUIRE_TIMEOUT, Pool};

const POOL_SIZE: usize = 16;

const MAX_BODY: usize = 256 * 1024;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

enum Auth {
    Token(String),

    Pinned { uid: i32 },
}

struct AppState {
    registry: tokio::sync::RwLock<Arc<Registry>>,

    export: Option<String>,
    pool: Pool,
    caches: Arc<Caches>,
    auth: Auth,
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
        Some(token) => {
            tracing::info!("requests must carry X-Rustorm-Token");
            Auth::Token(token)
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
        Ok(out) => {
            if let Err(e) = &out
                && odoo_kernel::orm::is_tx_end_failure(e)
            {
                conn.poison("its COMMIT/ROLLBACK failed");
            }
            out
        }
        Err(_) => {
            // The future was dropped mid-statement: the server may still be
            // running it and the transaction is certainly still open.
            conn.poison("the request timed out inside its transaction");
            anyhow::bail!("request exceeded {REQUEST_TIMEOUT:?}")
        }
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
    match &state.auth {
        Auth::Token(expected) => {
            let given = headers
                .get("x-rustorm-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();

            let ok = given.len() == expected.len()
                && given
                    .bytes()
                    .zip(expected.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0;
            if !ok {
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
