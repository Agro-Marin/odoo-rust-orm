use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::{bail, Result};
use sea_query::{Cond, Condition, ExprTrait};
use serde_json::{json, Value as Json};
use tokio_postgres::Client;

use crate::db::Db;
use crate::domain::{self};
use crate::registry::{Field, FieldType, Model, Registry, SignalChange};
use crate::security::{self, RuleSet, UserCtx};
use crate::sqlgen::{col, Compiler, ExprCtx};

#[derive(Debug, Clone, Copy)]
pub struct RegistryStale;

impl std::fmt::Display for RegistryStale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the Odoo registry changed (orm_signaling_registry moved); this \
             kernel's model map is stale and must be rebuilt",
        )
    }
}

impl std::error::Error for RegistryStale {}

fn collect_leaves(node: &domain::Node, out: &mut Vec<(String, Json)>) {
    match node {
        domain::Node::And(v) | domain::Node::Or(v) => v.iter().for_each(|n| collect_leaves(n, out)),
        domain::Node::Not(n) => collect_leaves(n, out),
        domain::Node::Leaf(l) => out.push((l.field.clone(), l.value.clone())),
        _ => {}
    }
}

pub fn is_registry_stale(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<RegistryStale>().is_some())
}

pub use crate::db::StmtCache;

type EnvKey = (i32, crate::registry::Signals);

type EnvValue = (i32, Vec<i32>);

type HierMemo = HashMap<(String, String, bool, Vec<i32>), Vec<i64>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RuleKey {
    uid: i32,
    company_id: i32,
    company_ids: Vec<i32>,

    signals: crate::registry::Signals,

    seeds: Vec<String>,
}

#[derive(Default)]
pub struct Caches {
    rule_cache: tokio::sync::Mutex<HashMap<RuleKey, Arc<RuleSet>>>,

    env_cache: tokio::sync::Mutex<HashMap<EnvKey, EnvValue>>,
}

const MAX_IDENTITIES: usize = 512;

impl Caches {
    pub async fn clear(&self) {
        self.rule_cache.lock().await.clear();
        self.env_cache.lock().await.clear();
    }

    async fn evict(&self, current: &crate::registry::Signals) {
        let mut rules = self.rule_cache.lock().await;
        if rules.len() > MAX_IDENTITIES {
            let before = rules.len();
            rules.retain(|k, _| &k.signals == current);
            if rules.len() > MAX_IDENTITIES {
                rules.clear();
            }
            tracing::info!(
                target: "odoo_kernel::rules",
                before, after = rules.len(), "evicted cached rule sets"
            );
        }
        let mut envs = self.env_cache.lock().await;
        if envs.len() > MAX_IDENTITIES {
            envs.retain(|(_, sig), _| sig == current);
            if envs.len() > MAX_IDENTITIES {
                envs.clear();
            }
        }
    }
}

pub struct Orm<'a> {
    pub registry: &'a Registry,
    pub client: &'a Client,
    pub(crate) db: Db<'a>,
    caches: Arc<Caches>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Request {
    pub id: Option<String>,
    pub model: String,
    pub method: String,
    #[serde(default)]
    pub domain: Json,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub order: Option<String>,

    #[serde(default)]
    pub groupby: Json,
    #[serde(default)]
    pub aggregates: Vec<String>,

    #[serde(default)]
    pub uid: Option<UidSpec>,

    #[serde(default)]
    pub su: bool,
    #[serde(default)]
    pub lang: Option<String>,

    #[serde(default)]
    pub allowed_company_ids: Option<Vec<i32>>,

    #[serde(default)]
    pub groupby_labels: Option<bool>,

    #[serde(default)]
    pub active_test: Option<bool>,
}

impl Request {
    pub fn groupby_names(&self) -> Result<Vec<String>> {
        match &self.groupby {
            Json::String(s) => Ok(vec![s.clone()]),
            Json::Array(items) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow::anyhow!("bad groupby {v}"))
                })
                .collect(),
            Json::Null => bail!("read_group requires groupby"),
            other => bail!("bad groupby {other}"),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum UidSpec {
    Id(i32),
    Symbol(String),
}

#[derive(Clone)]
pub struct Env {
    pub uid: i32,
    pub su: bool,
    pub lang: String,
    pub company_id: i32,
    pub company_ids: Vec<i32>,
    pub active_test: bool,
    pub dynamic: Arc<crate::registry::Dynamic>,

    pub groups: Arc<std::collections::HashSet<i32>>,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("uid", &self.uid)
            .field("su", &self.su)
            .field("lang", &self.lang)
            .field("company_id", &self.company_id)
            .field("company_ids", &self.company_ids)
            .field("active_test", &self.active_test)
            .finish_non_exhaustive()
    }
}

impl<'a> Orm<'a> {
    pub fn new(
        registry: &'a Registry,
        client: &'a Client,
        caches: Arc<Caches>,
        stmts: &'a StmtCache,
    ) -> Self {
        Orm {
            registry,
            client,
            db: Db::new(client, stmts),
            caches,
        }
    }

    pub(crate) async fn query_cached(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>> {
        self.db.query(sql, params).await
    }

    async fn check_signaling(&self) -> Result<()> {
        let Some(sql) = self.registry.signals_sql() else {
            return Ok(());
        };
        let rows = self.query_cached(sql, &[]).await?;
        let signals = Registry::signals_of(&rows[0]);
        let current = self.registry.dynamic();
        match self.registry.signal_change(&current.signals, &signals) {
            SignalChange::None => Ok(()),
            SignalChange::Security => {
                tracing::info!(
                    target: "odoo_kernel::signal",
                    "security snapshot changed; reloading rules, access and defaults"
                );
                self.registry.refresh_dynamic(self.client, signals).await?;
                self.caches.clear().await;
                Ok(())
            }

            SignalChange::Registry => {
                tracing::error!(
                    target: "odoo_kernel::signal",
                    "orm_signaling_registry moved; this kernel's model map is stale"
                );
                Err(RegistryStale.into())
            }
        }
    }

    async fn build_env(&self, req: &Request) -> Result<Env> {
        let dynamic = self.registry.dynamic();
        let uid = match &req.uid {
            None => 2,
            Some(UidSpec::Id(id)) => *id,
            Some(UidSpec::Symbol(sym)) if sym == "other" => self
                .db
                .query_opt(
                    "SELECT id FROM res_users WHERE active AND id NOT IN (1, 2) \
                     ORDER BY id LIMIT 1",
                    &[],
                )
                .await?
                .map(|r| r.get::<_, i32>(0))
                .ok_or_else(|| {
                    anyhow::anyhow!("no alternate identity on this database (uid \"other\")")
                })?,
            Some(UidSpec::Symbol(other)) => bail!("unknown uid symbol {other:?}"),
        };
        let lang = match &req.lang {
            Some(l) => {
                if !self.registry.langs.iter().any(|x| x == l) {
                    bail!("unknown or inactive language {l}");
                }
                l.clone()
            }
            None => "en_US".to_string(),
        };
        let env_key = (uid, dynamic.signals.clone());
        let cached = { self.caches.env_cache.lock().await.get(&env_key).cloned() };
        let (default_company_id, user_company_ids) = match cached {
            Some(v) => v,
            None => {
                let row = self
                    .db
                    .query_opt("SELECT company_id FROM res_users WHERE id = $1", &[&uid])
                    .await?;
                let cid: i32 = row
                    .and_then(|r| r.get::<_, Option<i32>>(0))
                    .ok_or_else(|| anyhow::anyhow!("unknown uid {uid}"))?;
                let mut cids = dynamic
                    .security
                    .user_companies
                    .get(&uid)
                    .cloned()
                    .unwrap_or_default();
                if cids.is_empty() {
                    cids = vec![cid];
                }
                cids.sort_unstable();
                self.caches
                    .env_cache
                    .lock()
                    .await
                    .insert(env_key, (cid, cids.clone()));
                (cid, cids)
            }
        };

        let (company_id, company_ids) = match req.allowed_company_ids.as_deref() {
            Some([]) | None => (default_company_id, user_company_ids),
            Some(allowed) => {
                if !req.su {
                    if let Some(bad) = allowed.iter().find(|c| !user_company_ids.contains(c)) {
                        bail!("uid {uid} is not allowed in company {bad}");
                    }
                }
                (allowed[0], allowed.to_vec())
            }
        };
        Ok(Env {
            uid,
            su: req.su,
            lang,
            company_id,
            company_ids,
            active_test: req.active_test.unwrap_or(true),
            groups: Arc::new(dynamic.security.groups_of(uid)),
            dynamic,
        })
    }

    pub(crate) async fn uid_rules(&self, req: &Request, env: &Env) -> Result<Arc<RuleSet>> {
        if env.su {
            return Ok(Arc::new(RuleSet::default()));
        }

        let mut seeds = self.reachable_seeds(req, env)?;
        seeds.sort();
        seeds.dedup();
        let key = RuleKey {
            uid: env.uid,
            company_id: env.company_id,
            company_ids: env.company_ids.clone(),
            signals: env.dynamic.signals.clone(),
            seeds: seeds.clone(),
        };
        if let Some(cached) = self.caches.rule_cache.lock().await.get(&key) {
            tracing::trace!(target: "odoo_kernel::rules", uid = env.uid, "rule cache hit");
            return Ok(cached.clone());
        }
        let user = UserCtx {
            uid: env.uid,
            company_id: env.company_id,
            company_ids: env.company_ids.clone(),
            groups: env.groups.clone(),
        };
        let t_rules = std::time::Instant::now();

        let ctx = self.ctx(env);
        let ruled: std::collections::HashSet<String> =
            env.dynamic.security.rules.keys().cloned().collect();
        let mut rules = RuleSet::with_ruled(ruled);
        let mut memo: HierMemo = HashMap::new();
        let mut pending: Vec<String> = seeds;
        let mut seen: std::collections::HashSet<String> = pending.iter().cloned().collect();
        let mut models_seen = 0usize;
        while let Some(model_name) = pending.pop() {
            models_seen += 1;

            for (parent, _) in self.registry.inherits_of(&model_name) {
                if seen.insert(parent.clone()) {
                    pending.push(parent.clone());
                }
            }
            let Ok(model) = self.registry.get(&model_name) else {
                continue;
            };
            let built =
                match security::rules_domain(self.registry, &self.db, &model_name, &user).await {
                    Ok(Some(domain_json)) => self
                        .resolve_hierarchy(&ctx, model, &domain_json, &mut memo)
                        .await
                        .and_then(|resolved| domain::parse(&resolved))
                        .map(Some),
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                };
            match built {
                Ok(Some(node)) => {
                    for co in self.comodels_of(model, &node) {
                        if seen.insert(co.clone()) {
                            pending.push(co);
                        }
                    }
                    rules.insert(model_name, node)
                }
                Ok(None) => rules.mark_unrestricted(model_name),

                Err(e) => rules.mark_unevaluated(model_name, format!("{e:#}")),
            }
        }
        for (model, why) in rules.unevaluated_models() {
            tracing::warn!(
                target: "odoo_kernel::rules",
                uid = env.uid,
                model = %model,
                reason = %why,
                "record rules could not be compiled; model refused"
            );
        }
        tracing::debug!(
            target: "odoo_kernel::rules",
            uid = env.uid,
            company_id = env.company_id,
            models = models_seen,
            restricted = rules.restricted_count(),
            refused = rules.unevaluated_models().count(),
            hierarchy_queries = memo.len(),
            ms = t_rules.elapsed().as_secs_f64() * 1000.0,
            "compiled record rules for identity"
        );
        let arc = Arc::new(rules);
        self.caches.rule_cache.lock().await.insert(key, arc.clone());
        self.caches.evict(&env.dynamic.signals).await;
        Ok(arc)
    }

    fn reachable_seeds(&self, req: &Request, env: &Env) -> Result<Vec<String>> {
        let mut out = vec![req.model.clone()];
        let Ok(model) = self.registry.get(&req.model) else {
            return Ok(out);
        };
        let ctx = self.ctx(env);
        let named = req
            .fields
            .iter()
            .chain(req.aggregates.iter())
            .map(|f| f.split(':').next().unwrap_or(f))
            .chain(
                req.groupby_names()
                    .unwrap_or_default()
                    .iter()
                    .map(|g| g.split(':').next().unwrap_or(g))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
            .map(str::to_string)
            .collect::<Vec<String>>();
        for fname in named {
            if let Some(f) = model.fields.get(&fname) {
                if let Some(co) = &f.relation {
                    out.push(co.clone());
                }
            }
        }

        let empty = json!([]);
        let domain = match req.domain {
            Json::Null => &empty,
            _ => &req.domain,
        };
        if let Ok(node) = crate::domain::parse(domain) {
            out.extend(self.comodels_of_with(&ctx, model, &node));
        }
        Ok(out)
    }

    fn comodels_of(&self, model: &Model, node: &domain::Node) -> Vec<String> {
        let ctx = ExprCtx::new(self.registry, "en_US", 1);
        self.comodels_of_with(&ctx, model, node)
    }

    fn comodels_of_with(
        &self,
        ctx: &ExprCtx<'_>,
        model: &Model,
        node: &domain::Node,
    ) -> Vec<String> {
        let mut paths: Vec<(String, Json)> = Vec::new();
        collect_leaves(node, &mut paths);
        let mut out = Vec::new();
        for (field_expr, value) in paths {
            let raw: Vec<String> = field_expr.split('.').map(str::to_string).collect();
            let Ok(path) = ctx.normalize_path(model, &raw) else {
                continue;
            };
            let mut m = model;
            for seg in &path {
                let Some(f) = m.fields.get(seg) else { break };
                let Some(co) = &f.relation else { break };
                out.push(co.clone());
                let Ok(next) = self.registry.get(co) else {
                    break;
                };
                m = next;
            }

            if let Some(last) = out.last() {
                if let (Ok(co), Ok(sub)) = (self.registry.get(last), domain::parse(&value)) {
                    if !matches!(sub, domain::Node::True) {
                        out.extend(self.comodels_of_with(ctx, co, &sub));
                    }
                }
            }
        }
        out
    }

    async fn resolve_hierarchy(
        &self,
        ctx: &ExprCtx<'_>,
        model: &Model,
        domain_json: &Json,
        memo: &mut HierMemo,
    ) -> Result<Json> {
        let Json::Array(items) = domain_json else {
            return Ok(domain_json.clone());
        };
        let mut out: Vec<Json> = Vec::with_capacity(items.len());
        for item in items {
            let Json::Array(leaf) = item else {
                out.push(item.clone());
                continue;
            };
            if leaf.len() != 3 {
                out.push(item.clone());
                continue;
            }
            let (Some(fname), Some(op)) = (leaf[0].as_str(), leaf[1].as_str()) else {
                out.push(item.clone());
                continue;
            };
            match op {
                "child_of" | "parent_of" => {
                    let (target_model, parent_link, key_field) = if fname == "id" {
                        let p = self.parent_name(model);
                        (model, p, "id".to_string())
                    } else {
                        let path: Vec<String> = fname.split('.').map(str::to_string).collect();
                        let norm = ctx.normalize_path(model, &path)?;
                        let holder = ctx.path_target(model, &norm[..norm.len() - 1])?;
                        let f = holder.fields.get(&norm[norm.len() - 1]).unwrap();
                        let comodel_name = f
                            .relation
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("{op} on non-relational {fname}"))?;
                        if comodel_name == holder.name
                            && f.ttype == FieldType::Many2one
                            && norm.len() == 1
                        {
                            (holder, Some(norm[0].clone()), "id".to_string())
                        } else {
                            let co = self.registry.get(&comodel_name)?;
                            (co, self.parent_name(co), norm.join("."))
                        }
                    };
                    let seeds: Vec<i32> = match &leaf[2] {
                        Json::Number(n) => n.as_i64().map(|i| i as i32).into_iter().collect(),
                        Json::Array(a) => a
                            .iter()
                            .filter_map(|v| v.as_i64().map(|i| i as i32))
                            .collect(),
                        other => bail!("unsupported {op} value {other}"),
                    };
                    let Some(parent_link) = parent_link else {
                        bail!("{} has no parent field for {op}", target_model.name);
                    };
                    let down = op == "child_of";
                    let key = (
                        target_model.name.clone(),
                        parent_link.clone(),
                        down,
                        seeds.clone(),
                    );
                    let ids = match memo.get(&key) {
                        Some(hit) => hit.clone(),
                        None => {
                            let ids = if down {
                                self.descendants(target_model, &parent_link, &seeds).await?
                            } else {
                                self.ancestors(target_model, &parent_link, &seeds).await?
                            };
                            memo.insert(key, ids.clone());
                            ids
                        }
                    };
                    out.push(json!([key_field, "in", ids]));
                }
                "any" | "not any" | "any!" | "not any!" => {
                    let path: Vec<String> = fname.split('.').map(str::to_string).collect();
                    let norm = ctx.normalize_path(model, &path)?;
                    let comodel = ctx.path_target(model, &norm)?;
                    let sub =
                        Box::pin(self.resolve_hierarchy(ctx, comodel, &leaf[2], memo)).await?;
                    out.push(json!([fname, op, sub]));
                }
                _ => out.push(item.clone()),
            }
        }
        Ok(Json::Array(out))
    }

    fn parent_name(&self, model: &Model) -> Option<String> {
        let is_self_m2o = |f: &&Field| {
            f.has_column
                && f.ttype == FieldType::Many2one
                && f.relation.as_deref() == Some(model.name.as_str())
        };

        if let Some(declared) = model.parent_name.as_deref() {
            return model
                .fields
                .get(declared)
                .filter(is_self_m2o)
                .map(|_| declared.to_string());
        }
        if model.fields.get("parent_id").filter(is_self_m2o).is_some() {
            return Some("parent_id".to_string());
        }
        let selfs: Vec<&Field> = model.fields.values().filter(|f| is_self_m2o(f)).collect();
        match selfs.as_slice() {
            [only] => Some(only.name.clone()),
            _ => None,
        }
    }

    fn can_use_parent_path(&self, model: &Model, parent_link: &str) -> bool {
        model
            .fields
            .get("parent_path")
            .is_some_and(|f| f.has_column)
            && self.parent_name(model).as_deref() == Some(parent_link)
    }

    async fn descendants(
        &self,
        model: &Model,
        parent_link: &str,
        seeds: &[i32],
    ) -> Result<Vec<i64>> {
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        if self.can_use_parent_path(model, parent_link) {
            let rows = self
                .query_cached(
                    &format!(
                        "SELECT parent_path FROM {} WHERE id = ANY($1)",
                        crate::db::ident(&model.table)
                    ),
                    &[&seeds],
                )
                .await?;
            let prefixes: Vec<String> = rows
                .iter()
                .filter_map(|r| r.get::<_, Option<String>>(0))
                .map(|p| format!("{p}%"))
                .collect();
            if prefixes.is_empty() {
                return Ok(vec![]);
            }
            let rows = self
                .query_cached(
                    &format!(
                        "SELECT id FROM {} WHERE parent_path LIKE ANY($1) ORDER BY id",
                        crate::db::ident(&model.table)
                    ),
                    &[&prefixes],
                )
                .await?;
            return Ok(rows.iter().map(|r| r.get::<_, i32>(0) as i64).collect());
        }

        let mut all: BTreeSet<i32> = seeds.iter().copied().collect();
        let mut frontier: Vec<i32> = seeds.to_vec();
        while !frontier.is_empty() {
            let rows = self
                .query_cached(
                    &format!(
                        "SELECT id FROM {} WHERE {} = ANY($1)",
                        crate::db::ident(&model.table),
                        crate::db::ident(parent_link)
                    ),
                    &[&frontier],
                )
                .await?;
            frontier = rows
                .iter()
                .map(|r| r.get::<_, i32>(0))
                .filter(|id| all.insert(*id))
                .collect();
        }
        Ok(all.into_iter().map(|i| i as i64).collect())
    }

    async fn ancestors(&self, model: &Model, parent_link: &str, seeds: &[i32]) -> Result<Vec<i64>> {
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        if self.can_use_parent_path(model, parent_link) {
            let rows = self
                .query_cached(
                    &format!(
                        "SELECT parent_path FROM {} WHERE id = ANY($1)",
                        crate::db::ident(&model.table)
                    ),
                    &[&seeds],
                )
                .await?;
            let mut ids: BTreeSet<i64> = BTreeSet::new();
            for row in rows {
                if let Some(path) = row.get::<_, Option<String>>(0) {
                    for seg in path.split('/').filter(|s| !s.is_empty()) {
                        if let Ok(id) = seg.parse::<i64>() {
                            ids.insert(id);
                        }
                    }
                }
            }
            return Ok(ids.into_iter().collect());
        }
        let mut all: BTreeSet<i32> = seeds.iter().copied().collect();
        let mut frontier: Vec<i32> = seeds.to_vec();
        while !frontier.is_empty() {
            let rows = self
                .query_cached(
                    &format!(
                        "SELECT {} FROM {} WHERE id = ANY($1)",
                        crate::db::ident(parent_link),
                        crate::db::ident(&model.table)
                    ),
                    &[&frontier],
                )
                .await?;
            frontier = rows
                .iter()
                .filter_map(|r| r.get::<_, Option<i32>>(0))
                .filter(|id| all.insert(*id))
                .collect();
        }
        Ok(all.into_iter().map(|i| i as i64).collect())
    }

    pub async fn dispatch(&self, req: &Request) -> Result<String> {
        let span = tracing::info_span!(
            "dispatch",
            model = %req.model,
            method = %req.method,
            uid = ?req.uid,
            su = req.su,
        );
        let _guard = span.enter();
        let t0 = std::time::Instant::now();
        let out = self.dispatch_inner(req).await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match &out {
            Ok(raw) => {
                tracing::debug!(target: "odoo_kernel::dispatch", ms, bytes = raw.len(), "ok")
            }

            Err(e) => tracing::info!(
                target: "odoo_kernel::dispatch", ms, reason = %format!("{e:#}"), "refused"
            ),
        }
        out
    }

    pub async fn dispatch_in_transaction(&self, req: &Request) -> Result<String> {
        self.client
            .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await?;
        let out = self.dispatch(req).await;

        let end = if out.is_ok() { "COMMIT" } else { "ROLLBACK" };
        if let Err(e) = self.client.batch_execute(end).await {
            tracing::warn!(target: "odoo_kernel::sql", error = %e, "could not {end} the read transaction");
        }
        out
    }

    async fn dispatch_inner(&self, req: &Request) -> Result<String> {
        Self::validate_shape(req)?;
        self.check_signaling().await?;
        let env = self.build_env(req).await?;

        if !self.registry.get(&req.model)?.read_path_pure {
            bail!(
                "{} overrides the read path in Python (_search / read / \
                 search_read / _read_group / search_count); the kernel cannot \
                 reproduce it from the columns",
                req.model
            );
        }
        if !env.su {
            security::check_read_access(&env.dynamic, &req.model, env.uid, &env.groups)?;
        }
        match req.method.as_str() {
            "search_read" => self.search_read(req, &env).await,
            "search_count" => self.search_count(req, &env).await,
            "read_group" => self.read_group(req, &env).await,
            m => bail!("unknown method {m}"),
        }
    }

    fn validate_shape(req: &Request) -> Result<()> {
        let unsupported = |name: &str| -> Result<()> {
            bail!(
                "{} does not support `{name}`; refusing rather than ignoring it",
                req.method
            )
        };
        let groupby_given = !matches!(req.groupby, Json::Null)
            && !matches!(&req.groupby, Json::Array(a) if a.is_empty());
        match req.method.as_str() {
            "search_read" => {
                if req.fields.is_empty() {
                    bail!(
                        "search_read without `fields` returns every readable field in \
                         Odoo; this kernel does not enumerate them, and answering with \
                         the id alone is a different question"
                    );
                }
                if groupby_given {
                    unsupported("groupby")?;
                }
                if !req.aggregates.is_empty() {
                    unsupported("aggregates")?;
                }
            }
            "search_count" => {
                if !req.fields.is_empty() {
                    unsupported("fields")?;
                }
                if req.offset.is_some_and(|o| o > 0) {
                    unsupported("offset")?;
                }
                if req.order.is_some() {
                    unsupported("order")?;
                }
                if groupby_given {
                    unsupported("groupby")?;
                }
                if !req.aggregates.is_empty() {
                    unsupported("aggregates")?;
                }
            }
            "read_group" => {
                if !req.fields.is_empty() {
                    unsupported("fields")?;
                }
                if let Some(order) = &req.order {
                    let specs = req.groupby_names()?;
                    let terms: Vec<&str> = order
                        .split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .collect();
                    let is_sequence = terms.len() == specs.len()
                        && terms.iter().zip(&specs).all(|(t, spec)| {
                            let mut parts = t.split_whitespace();
                            parts.next() == Some(spec.as_str())
                                && matches!(
                                    parts.next().map(str::to_ascii_lowercase).as_deref(),
                                    None | Some("asc")
                                )
                                && parts.next().is_none()
                        });
                    if !is_sequence {
                        bail!(
                            "read_group supports `order` only as the groupby sequence; \
                             refusing {order:?} rather than ignoring it"
                        );
                    }
                }
            }
            other => bail!("unknown method {other}"),
        }
        Ok(())
    }

    pub(crate) fn ctx<'e>(&'e self, env: &'e Env) -> ExprCtx<'e> {
        let ctx = ExprCtx::pinned(
            self.registry,
            env.dynamic.clone(),
            &env.lang,
            env.company_id,
            env.active_test,
        );
        if env.su {
            ctx
        } else {
            ctx.with_access(env.uid, env.groups.clone())
        }
    }

    pub(crate) async fn build_condition(
        &self,
        model: &Model,
        domain_json: &Json,
        env: &Env,
        rules: &RuleSet,
    ) -> Result<Condition> {
        let empty = json!([]);
        let domain_json = if domain_json.is_null() {
            &empty
        } else {
            domain_json
        };

        let mut memo: HierMemo = HashMap::new();
        let ctx = self.ctx(env);
        let resolved = self
            .resolve_hierarchy(&ctx, model, domain_json, &mut memo)
            .await?;
        let node = domain::parse(&resolved)?;

        let compiler = Compiler::root(&ctx, model, rules, env.su);
        let mut cond = Cond::all().add(compiler.compile(&node)?);

        if let Some(active_name) = model.active_name.as_deref() {
            if env.active_test {
                let mut referenced = Vec::new();
                domain::referenced_fields(&node, &mut referenced);
                if !referenced.iter().any(|f| f == active_name) {
                    cond = cond.add(col(&model.table, active_name).is_in([true]));
                }
            }
        }
        if !env.su {
            rules.ensure_evaluated(&model.name)?;
            if let Some(rule_node) = rules.get(&model.name) {
                cond = cond.add(compiler.compile(rule_node)?);
            }
        }
        Ok(cond)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::registry::{Dynamic, Security};

    fn dynamic(signal: i64) -> Dynamic {
        Dynamic {
            security: Security::default(),
            defaults: HashMap::new(),
            signals: vec![Some(signal)],
        }
    }

    fn key(signals: &crate::registry::Signals) -> RuleKey {
        RuleKey {
            uid: 2,
            company_id: 1,
            company_ids: vec![1],
            signals: signals.clone(),
            seeds: vec!["res.partner".to_string()],
        }
    }

    #[tokio::test]
    async fn a_late_insert_from_a_stale_snapshot_is_never_served() {
        let old = dynamic(1).signals;
        let caches = Caches::default();

        caches.clear().await;
        let new = dynamic(2).signals;

        caches
            .rule_cache
            .lock()
            .await
            .insert(key(&old), Arc::new(RuleSet::default()));

        assert!(
            caches.rule_cache.lock().await.get(&key(&new)).is_none(),
            "a request at the new watermark must not see rules compiled at the old one"
        );
        assert!(
            caches.rule_cache.lock().await.get(&key(&old)).is_some(),
            "the stale entry is merely unreachable, not lost -- clear() collects it"
        );
    }

    #[tokio::test]
    async fn the_same_identity_at_the_same_watermark_still_hits() {
        let now = dynamic(7).signals;
        let caches = Caches::default();
        caches
            .rule_cache
            .lock()
            .await
            .insert(key(&now), Arc::new(RuleSet::default()));
        assert!(caches.rule_cache.lock().await.get(&key(&now)).is_some());
    }

    #[tokio::test]
    async fn clear_collects_entries_from_every_watermark() {
        let caches = Caches::default();
        for w in 1..=3 {
            caches
                .rule_cache
                .lock()
                .await
                .insert(key(&vec![Some(w)]), Arc::new(RuleSet::default()));
        }
        assert_eq!(caches.rule_cache.lock().await.len(), 3);
        caches.clear().await;
        assert!(caches.rule_cache.lock().await.is_empty());
    }
}
