use anyhow::Result;
use std::collections::HashMap;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Row};

pub const MAX_PREPARED: usize = 256;

#[derive(Default)]
pub struct StmtCache {
    inner: std::sync::Mutex<RecencyMap<tokio_postgres::Statement>>,
}

struct CacheEntry<T> {
    value: T,
    used: u64,
}

// Hits are the hot path: one lookup and an age update, with no allocation or
// queue scan. Bounded O(capacity) work happens only when an insert evicts.
struct RecencyMap<T> {
    map: HashMap<String, CacheEntry<T>>,
    clock: u64,
}

impl<T> Default for RecencyMap<T> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            clock: 0,
        }
    }
}

impl<T> RecencyMap<T> {
    fn next_tick(&mut self) -> u64 {
        if self.clock == u64::MAX {
            let mut entries: Vec<_> = self.map.values_mut().collect();
            entries.sort_unstable_by_key(|entry| entry.used);
            for (i, entry) in entries.iter_mut().enumerate() {
                entry.used = i as u64;
            }
            self.clock = entries.len() as u64;
        }
        self.clock += 1;
        self.clock
    }

    fn get(&mut self, key: &str) -> Option<&T> {
        let used = self.next_tick();
        let entry = self.map.get_mut(key)?;
        entry.used = used;
        Some(&entry.value)
    }

    fn insert(&mut self, key: String, value: T, capacity: usize) -> Option<String> {
        let used = self.next_tick();
        self.map.insert(key, CacheEntry { value, used });
        if self.map.len() > capacity {
            let oldest = self
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
                .expect("over capacity means nonempty");
            self.map.remove(&oldest);
            return Some(oldest);
        }
        None
    }
}

impl StmtCache {
    pub fn get(&self, sql: &str) -> Option<tokio_postgres::Statement> {
        self.inner.lock().unwrap().get(sql).cloned()
    }

    pub fn insert(&self, sql: String, stmt: tokio_postgres::Statement) {
        let evicted = self.inner.lock().unwrap().insert(sql, stmt, MAX_PREPARED);
        if let Some(oldest) = evicted {
            tracing::debug!(
                target: "odoo_kernel::cache",
                cache = "stmt", max = MAX_PREPARED, evicted = %oldest,
                "evicted the least recently used prepared statement"
            );
        }
    }

    pub fn remove(&self, sql: &str) {
        let (present, len) = {
            let mut inner = self.inner.lock().unwrap();
            (inner.map.remove(sql).is_some(), inner.map.len())
        };
        tracing::debug!(
            target: "odoo_kernel::cache",
            cache = "stmt", present, len, %sql,
            "dropped one prepared statement"
        );
    }

    pub fn clear(&self) {
        let before = {
            let mut inner = self.inner.lock().unwrap();
            let before = inner.map.len();
            inner.map.clear();
            inner.clock = 0;
            before
        };
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

#[cfg(test)]
mod cache_tests {
    use super::RecencyMap;

    #[test]
    fn recency_matches_a_reference_queue_through_replacement_and_overflow() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_test_writer()
            .try_init();
        let mut cache = RecencyMap::default();
        let mut queue = std::collections::VecDeque::new();
        let mut values = std::collections::HashMap::new();
        // Deterministic mixed accesses; forced wrap exercises age rebasing.
        for step in 0..2000 {
            if step == 1000 {
                cache.clock = u64::MAX;
            }
            let key = format!("k{}", (step * 17 + step / 7) % 11);
            if step % 3 == 0 {
                let got = cache.get(&key).copied();
                let expected = values.get(&key).copied();
                assert_eq!(got, expected);
                if expected.is_some() {
                    queue.retain(|k| k != &key);
                    queue.push_back(key);
                }
            } else {
                queue.retain(|k| k != &key);
                queue.push_back(key.clone());
                values.insert(key.clone(), step);
                let evicted = if queue.len() > 4 {
                    let old = queue.pop_front().unwrap();
                    values.remove(&old);
                    Some(old)
                } else {
                    None
                };
                assert_eq!(cache.insert(key, step, 4), evicted);
            }
            assert_eq!(cache.map.len(), values.len());
            for (key, value) in &values {
                assert_eq!(cache.map[key].value, *value);
            }
        }
        tracing::debug!(
            operations = 2000,
            "recency matched the independent queue model"
        );
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
    pub offline: bool,
}

impl<'a> Db<'a> {
    pub fn new(client: &'a Client, stmts: &'a StmtCache) -> Self {
        Db {
            client,
            stmts,
            offline: false,
        }
    }

    pub async fn query(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>> {
        if self.offline {
            tracing::trace!(
                target: "odoo_kernel::sql",
                %sql,
                "offline: this statement would need a round trip; not sent"
            );
            return Err(crate::error::NeedsRoundTrip.into());
        }
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

    pub async fn query_signals(&self, sql: &str) -> Result<Vec<tokio_postgres::Row>> {
        Db {
            offline: false,
            ..*self
        }
        .query(sql, &[])
        .await
    }

    pub async fn query_opt(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>> {
        Ok(self.query(sql, params).await?.into_iter().next())
    }
}
