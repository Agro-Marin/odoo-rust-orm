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
    // LRU: a hit moves the statement to the back, so a hot one outlives the
    // churn of distinct texts that used to evict it first-in first-out
    pub fn get(&self, sql: &str) -> Option<tokio_postgres::Statement> {
        let mut inner = self.inner.lock().unwrap();
        let hit = inner.map.get(sql).cloned();
        if hit.is_some() {
            inner.order.retain(|k| k != sql);
            inner.order.push_back(sql.to_string());
        }
        hit
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
            tracing::debug!(
                target: "odoo_kernel::cache",
                cache = "stmt",
                len = inner.map.len(),
                max = MAX_PREPARED,
                evicted = %oldest,
                "evicted the least recently used prepared statement"
            );
        }
    }

    pub fn remove(&self, sql: &str) {
        let mut inner = self.inner.lock().unwrap();
        let present = inner.map.remove(sql).is_some();
        inner.order.retain(|k| k != sql);
        tracing::debug!(
            target: "odoo_kernel::cache",
            cache = "stmt",
            present,
            len = inner.map.len(),
            %sql,
            "dropped one prepared statement"
        );
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.map.len();
        inner.map.clear();
        inner.order.clear();
        if before > 0 {
            tracing::debug!(
                target: "odoo_kernel::cache",
                cache = "stmt", before, "cleared every prepared statement"
            );
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn stale_plan_code(e: &tokio_postgres::Error) -> bool {
    e.code() == Some(&tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED)
}

pub fn is_stale_plan_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<tokio_postgres::Error>()
            .is_some_and(stale_plan_code)
    })
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
        let t_prepare = std::time::Instant::now();
        let (stmt, freshly_prepared) = match self.stmts.get(sql) {
            Some(s) => (s, false),
            None => {
                let s = self.client.prepare(sql).await?;
                self.stmts.insert(sql.to_string(), s.clone());
                (s, true)
            }
        };
        let prepare_ms = t_prepare.elapsed().as_secs_f64() * 1000.0;
        let t0 = std::time::Instant::now();
        let rows = self.client.query(&stmt, params).await;

        if !freshly_prepared && rows.as_ref().err().is_some_and(stale_plan_code) {
            tracing::info!(
                target: "odoo_kernel::sql",
                %sql,
                "cached plan is stale; dropped. The error has aborted the caller's \
                 transaction, so a retry can only happen in a new one"
            );
            self.stmts.remove(sql);
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match &rows {
            Ok(r) => tracing::debug!(
                target: "odoo_kernel::sql",
                rows = r.len(),
                params = params.len(),
                freshly_prepared,
                prepare_ms,
                cached = self.stmts.len(),
                ms,
                %sql,
                "query"
            ),
            Err(e) => tracing::warn!(
                target: "odoo_kernel::sql",
                ms,
                params = params.len(),
                freshly_prepared,
                sqlstate = e.code().map(|c| c.code()).unwrap_or("-"),
                %sql,
                error = %e,
                "query failed"
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
