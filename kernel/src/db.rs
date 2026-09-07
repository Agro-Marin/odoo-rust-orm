use anyhow::Result;
use std::collections::HashMap;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Row};

pub const MAX_PREPARED: usize = 256;

#[derive(Default)]
pub struct StmtCache {
    inner: std::sync::Mutex<StmtCacheInner>,
}

#[derive(Default)]
struct StmtCacheInner {
    map: HashMap<String, tokio_postgres::Statement>,

    order: std::collections::VecDeque<String>,
}

impl StmtCache {
    pub fn get(&self, sql: &str) -> Option<tokio_postgres::Statement> {
        self.inner.lock().unwrap().map.get(sql).cloned()
    }

    pub fn insert(&self, sql: String, stmt: tokio_postgres::Statement) {
        let mut inner = self.inner.lock().unwrap();
        if inner.map.insert(sql.clone(), stmt).is_none() {
            inner.order.push_back(sql);
        }
        while inner.map.len() > MAX_PREPARED {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            inner.map.remove(&oldest);
        }
    }

    pub fn remove(&self, sql: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.map.remove(sql);
        inner.order.retain(|k| k != sql);
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.map.clear();
        inner.order.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn is_stale_plan<T>(result: &std::result::Result<T, tokio_postgres::Error>) -> bool {
    result.as_ref().err().and_then(|e| e.code())
        == Some(&tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED)
}

pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[derive(Clone, Copy)]
pub struct Db<'a> {
    pub client: &'a Client,
    pub stmts: &'a StmtCache,
}

impl<'a> Db<'a> {
    pub fn new(client: &'a Client, stmts: &'a StmtCache) -> Self {
        Db { client, stmts }
    }

    pub async fn query(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>> {
        let (stmt, prepared) = match self.stmts.get(sql) {
            Some(s) => (s, false),
            None => {
                let s = self.client.prepare(sql).await?;
                self.stmts.insert(sql.to_string(), s.clone());
                (s, true)
            }
        };
        let t0 = std::time::Instant::now();
        let mut rows = self.client.query(&stmt, params).await;

        if !prepared && is_stale_plan(&rows) {
            tracing::info!(
                target: "odoo_kernel::sql",
                %sql, "cached plan is stale; dropping it and retrying once"
            );
            self.stmts.remove(sql);
            if let Ok(fresh) = self.client.prepare(sql).await {
                let retried = self.client.query(&fresh, params).await;
                if retried.is_ok() {
                    self.stmts.insert(sql.to_string(), fresh);
                    rows = retried;
                }
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match &rows {
            Ok(r) => tracing::debug!(
                target: "odoo_kernel::sql",
                rows = r.len(),
                params = params.len(),
                prepared,
                ms,
                %sql,
                "query"
            ),
            Err(e) => tracing::warn!(
                target: "odoo_kernel::sql",
                ms, %sql, error = %e, "query failed"
            ),
        }
        Ok(rows?)
    }

    pub async fn query_opt(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>> {
        Ok(self.query(sql, params).await?.into_iter().next())
    }
}
