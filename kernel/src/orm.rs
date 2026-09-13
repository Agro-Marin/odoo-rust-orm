use crate::error::{deny_access, refusal, refuse};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Result;
use sea_query::{Cond, Condition, ExprTrait};
use serde_json::{Value as Json, json};
use tokio_postgres::Client;
use tracing::Instrument;

use crate::db::Db;
use crate::domain::{self};
use crate::registry::{FieldType, Model, Registry, SignalChange};
use crate::security::{self, RuleSet, UserCtx};
use crate::sqlgen::{Compiler, ExprCtx, col};

pub use crate::error::RegistryStale;

/// What `Orm::compile_where` hands back.
pub struct CompiledWhere {
    pub fragment: String,
    pub params: Vec<Json>,
    /// (model, field) for every column the fragment reads: the fields a caller
    /// must flush before running it.
    pub touched: Vec<(String, String)>,
    /// The signalling snapshot the compile ran against, for reuse later in the
    /// same transaction.
    pub snapshot: Arc<crate::registry::Dynamic>,
}

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

// default company, active companies, groups, and the user's tz — everything
// res.users._get_fields_invalidation clears the registry cache for
type EnvValue = (
    i32,
    Vec<i32>,
    Arc<std::collections::HashSet<i32>>,
    Option<String>,
);

// the terms a hierarchy leaf resolves to, per (model, link, direction, seeds)
type HierMemo = HashMap<(String, String, bool, Vec<i32>), Vec<Json>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RuleKey {
    uid: i32,
    company_id: i32,
    company_ids: Vec<i32>,

    signals: crate::registry::Signals,
}

#[derive(Default)]
pub struct Caches {
    rule_cache: tokio::sync::Mutex<HashMap<RuleKey, Arc<RuleSet>>>,

    env_cache: tokio::sync::Mutex<HashMap<EnvKey, EnvValue>>,
}

const MAX_IDENTITIES: usize = 512;

impl Caches {
    pub async fn clear(&self) {
        let rules = {
            let mut guard = self.rule_cache.lock().await;
            let n = guard.len();
            guard.clear();
            n
        };
        let envs = {
            let mut guard = self.env_cache.lock().await;
            let n = guard.len();
            guard.clear();
            n
        };
        if rules + envs > 0 {
            tracing::debug!(
                target: "odoo_kernel::cache",
                cache = "identity", rules, envs,
                "cleared the per-identity rule and environment caches"
            );
        }
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
            let before = envs.len();
            envs.retain(|(_, sig), _| sig == current);
            if envs.len() > MAX_IDENTITIES {
                envs.clear();
            }
            tracing::info!(
                target: "odoo_kernel::cache",
                cache = "env", before, after = envs.len(), max = MAX_IDENTITIES,
                "evicted cached environments"
            );
        }
    }
}

pub struct Orm<'a> {
    pub registry: &'a Registry,
    pub(crate) db: Db<'a>,
    caches: Arc<Caches>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: Option<String>,
    #[serde(default)]
    pub registry_sequence: Option<i64>,
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
    pub groupby_hidden_labels_empty: bool,

    /// Many2one fields `search_read` returns as the bare foreign key, with no
    /// label query. `web_search_read` resolves a many2one with rules of its
    /// own -- an unreadable target is still its id, not `False` -- and the
    /// shim hands the raw ids to web's resolver instead of a label that
    /// already redacted them.
    #[serde(default)]
    pub raw_many2one: Vec<String>,

    /// Many2one fields whose label `search_read` still renders, but whose
    /// target the label query hid comes back as the bare id instead of
    /// `False`. A visible target's label is web_read's answer too; only a
    /// hidden one needs web's resolver, and this is how the shim finds it.
    #[serde(default)]
    pub unredacted_many2one: Vec<String>,

    #[serde(default)]
    pub active_test: Option<bool>,

    #[serde(default)]
    pub x2many_active_test: Option<bool>,

    #[serde(default)]
    pub tz: Option<String>,

    /// Whether the ROOT model gets the implicit active filter, apart from
    /// `active_test`, which still governs every sub-query. The persistence
    /// port sends `false`: the domain it compiles has already been through
    /// `_search`, which decided the root filter -- including the
    /// `_search(active_test=False)` keyword the port never sees -- and wrote
    /// it into the domain when it applies.
    #[serde(default)]
    pub root_active_test: Option<bool>,

    /// The domain was composed by the ORM (`optimize_full` output handed to
    /// `StorageBackend.search`) rather than received from a caller, so an
    /// `any!` in it is a field's own bypass declaration and not a request for
    /// one. The port sets it, and so does the method shim for a domain it
    /// resolved through `optimize_full`.
    #[serde(default)]
    pub trusted_domain: bool,
}

impl Request {
    /// The groupby names the request carries, as a QUESTION: none is an answer.
    ///
    /// `groupby_names` refuses a missing groupby because `read_group` needs
    /// one. The reachability walk is asking which fields the request mentions,
    /// and a `search_read` mentioning none is not a refusal -- routing it
    /// through the demanding form filed one refusal per non-grouped request,
    /// 62% of a census over 8,254 sweep cases.
    pub fn groupby_names_seen(&self) -> Vec<String> {
        match &self.groupby {
            Json::String(s) => vec![s.clone()],
            Json::Array(items) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn groupby_names(&self) -> Result<Vec<String>> {
        match &self.groupby {
            Json::String(s) => Ok(vec![s.clone()]),
            Json::Array(items) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| refusal!("bad groupby {v}"))
                })
                .collect(),
            Json::Null => refuse!("read_group requires groupby"),
            other => refuse!("bad groupby {other}"),
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

    pub root_active_test: bool,

    pub x2many_active_test: bool,

    pub tz: Option<String>,

    // the zone a bare date in a domain names a day in: context tz, else user tz
    pub comparand_tz: Option<String>,

    pub week_start: Option<i32>,
    pub dynamic: Arc<crate::registry::Dynamic>,

    pub groups: Arc<std::collections::HashSet<i32>>,

    /// The columns every compile under this environment read; see
    /// `ExprCtx::touched`.
    pub touched: Arc<std::sync::Mutex<BTreeSet<(String, String)>>>,
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
            db: Db::new(client, stmts),
            caches,
        }
    }

    /// The same kernel on the same connection, but raising `NeedsRoundTrip`
    /// where it would send a statement.
    pub fn offline(mut self) -> Self {
        self.db.offline = true;
        self
    }

    /// The signalling snapshot a request runs against: the one `checked`
    /// earlier in the same transaction, else the watermark read now.
    ///
    /// There was a third source, and it was unsound: the caller's Python
    /// registry sequences, trusted when they matched the kernel's snapshot. The
    /// registry object is shared by every thread of the process and advances
    /// while a transaction is open, so an OLD transaction's registry can read
    /// the NEW sequences -- match the kernel's refreshed snapshot -- and the
    /// kernel would apply rules its database snapshot cannot see yet. The
    /// runtime contract "security refresh cannot make old transactions use
    /// future rule metadata" failed on it. Only the watermark this
    /// transaction can SEE says which snapshot it may use.
    async fn snapshot(
        &self,
        _req: &Request,
        checked: Option<Arc<crate::registry::Dynamic>>,
    ) -> Result<Arc<crate::registry::Dynamic>> {
        if let Some(dynamic) = checked {
            // Reuse skips READING the watermark, not COMPARING it: once another
            // connection has refreshed the kernel's registry or security
            // metadata past what this transaction checked, the transaction is
            // refused as `check_signaling` would refuse it. The kernel holds
            // one current security state, and some compile paths read it from
            // the registry rather than from the snapshot passed in. The
            // contract "security refresh cannot make old transactions use
            // future rule metadata" failed until this comparison was here.
            if self
                .registry
                .snapshot_precedes(&dynamic.signals, &self.registry.dynamic().signals)
            {
                refuse!(
                    "the request snapshot predates this kernel's registry or security metadata"
                );
            }
            return Ok(dynamic);
        }
        self.check_signaling().await
    }

    async fn check_signaling(&self) -> Result<Arc<crate::registry::Dynamic>> {
        let Some(sql) = self.registry.signals_sql() else {
            tracing::trace!(
                target: "odoo_kernel::signal",
                "no signalling tables on this database; the snapshot cannot be checked"
            );
            return Ok(self.registry.dynamic());
        };
        // Sent even offline: a read of the signalling tables' maximum ids
        // takes no lock and fails only when the connection has, which Python's
        // own statement would meet the same way -- a savepoint could not roll
        // it back. It is the one kernel statement the first compile of a
        // transaction cannot avoid, and it must not cost that compile a
        // savepoint.
        let rows = self.db.query_signals(sql).await?;
        let signals = Registry::signals_of(&rows[0]);
        let current = self.registry.dynamic();
        tracing::trace!(
            target: "odoo_kernel::signal",
            db = ?signals, local = ?current.signals,
            "compared the database watermark with this snapshot's"
        );
        if self.registry.snapshot_precedes(&signals, &current.signals) {
            refuse!("the request snapshot predates this kernel's registry or security metadata");
        }
        // check_signaling: `if db > local`; a connection whose snapshot is
        // behind another's must not swing the watermark back and forth
        let advanced = signals
            .iter()
            .zip(current.signals.iter())
            .any(|(db, local)| db > local);
        if !advanced && current.signals.len() == signals.len() {
            return Ok(current);
        }
        match self.registry.signal_change(&current.signals, &signals) {
            SignalChange::None => Ok(current),
            SignalChange::Irrelevant => {
                tracing::debug!(
                    target: "odoo_kernel::signal",
                    "a cache signal moved that carries no security; stamping the watermark"
                );
                Ok(self.registry.stamp_signals(&current, signals))
            }
            SignalChange::Security => {
                tracing::info!(
                    target: "odoo_kernel::signal",
                    "security snapshot changed; reloading rules, access and defaults"
                );
                let fresh = self
                    .registry
                    .refresh_dynamic(self.db.client, signals)
                    .await?;
                self.caches.clear().await;
                Ok(fresh)
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

    async fn build_env(
        &self,
        req: &Request,
        dynamic: Arc<crate::registry::Dynamic>,
    ) -> Result<Env> {
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
                    refusal!("no alternate identity on this database (uid \"other\")")
                })?,
            Some(UidSpec::Symbol(sym)) if matches!(sym.as_str(), "grouped" | "debug") => {
                // identities the sweep seeds under fixed logins (harness/cases.py)
                let login = format!("rustorm_sweep_{sym}");
                self.db
                    .query_opt(
                        "SELECT id FROM res_users WHERE login = $1 AND active",
                        &[&login],
                    )
                    .await?
                    .map(|r| r.get::<_, i32>(0))
                    .ok_or_else(|| refusal!("no seeded identity {login:?} (uid {sym:?})"))?
            }
            Some(UidSpec::Symbol(other)) => refuse!("unknown uid symbol {other:?}"),
        };
        // `Environment.lang` accepts en_US whether or not its row is active
        let lang = match &req.lang {
            Some(l) if l != "en_US" => {
                if !dynamic.langs.iter().any(|x| x == l) {
                    refuse!("unknown or inactive language {l}");
                }
                l.clone()
            }
            _ => "en_US".to_string(),
        };
        let env_key = (uid, self.registry.security_signals(&dynamic.signals));
        let cached = { self.caches.env_cache.lock().await.get(&env_key).cloned() };
        tracing::trace!(
            target: "odoo_kernel::env",
            uid, hit = cached.is_some(), "environment cache lookup"
        );
        let (default_company_id, user_company_ids, groups, user_tz) = match cached {
            Some(v) => v,
            None => {
                let row = self
                    .db
                    .query_opt(
                        "SELECT u.company_id, p.tz FROM res_users u \
                         JOIN res_partner p ON p.id = u.partner_id WHERE u.id = $1",
                        &[&uid],
                    )
                    .await?;
                let (cid, user_tz): (i32, Option<String>) = match row {
                    Some(r) => (
                        r.get::<_, Option<i32>>(0)
                            .ok_or_else(|| refusal!("unknown uid {uid}"))?,
                        r.get::<_, Option<String>>(1).filter(|t| !t.is_empty()),
                    ),
                    None => refuse!("unknown uid {uid}"),
                };
                let mut cids = dynamic
                    .security
                    .user_companies
                    .get(&uid)
                    .cloned()
                    .unwrap_or_default();
                cids.sort_unstable();
                let groups = Arc::new(dynamic.security.groups_of(uid));
                self.caches.env_cache.lock().await.insert(
                    env_key,
                    (cid, cids.clone(), groups.clone(), user_tz.clone()),
                );
                (cid, cids, groups, user_tz)
            }
        };

        let (company_id, company_ids) = match req.allowed_company_ids.as_deref() {
            Some([]) | None => (default_company_id, user_company_ids),
            Some(allowed) => {
                if !req.su
                    && let Some(bad) = allowed.iter().find(|c| !user_company_ids.contains(c))
                {
                    deny_access!("access denied: uid {uid} is not allowed in company {bad}");
                }
                tracing::trace!(
                    target: "odoo_kernel::env",
                    uid, requested = ?allowed, of = ?user_company_ids,
                    "the request pins allowed_company_ids; the first is the active company"
                );
                (allowed[0], allowed.to_vec())
            }
        };
        let week_start = req
            .lang
            .as_ref()
            .and_then(|l| dynamic.week_start.get(l).copied());
        // `Environment.tz` is the context's tz or the user's; a bare date in a
        // domain names a day in THAT zone, while a read_group granularity
        // reads the context's tz only (format.py)
        let request_tz = req.tz.as_deref().filter(|t| !t.is_empty());
        // Environment.tz falls back to UTC on a name get_timezone rejects
        let comparand_tz = request_tz
            .or(user_tz.as_deref())
            .and_then(|t| self.registry.resolve_timezone(t));
        // A bare date in a domain names a day in `comparand_tz` and a
        // read_group granularity reads the context tz only: the two zones
        // differ on purpose, and a wrong answer here moves rows between
        // days rather than erroring, so both are on the line
        tracing::debug!(
            target: "odoo_kernel::env",
            uid,
            su = req.su,
            %lang,
            company_id,
            companies = ?company_ids,
            groups = groups.len(),
            active_test = req.active_test.unwrap_or(true),
            x2many_active_test = req.x2many_active_test.unwrap_or(true),
            request_tz = ?request_tz,
            user_tz = ?user_tz,
            comparand_tz = ?comparand_tz,
            week_start = ?week_start,
            "resolved the request environment"
        );
        Ok(Env {
            uid,
            su: req.su,
            lang,
            company_id,
            company_ids,
            active_test: req.active_test.unwrap_or(true),
            root_active_test: req
                .root_active_test
                .unwrap_or(req.active_test.unwrap_or(true)),
            x2many_active_test: req.x2many_active_test.unwrap_or(true),
            tz: req
                .tz
                .as_deref()
                .filter(|t| !t.is_empty())
                .and_then(|t| self.registry.resolve_timezone(t)),
            comparand_tz,
            week_start,
            groups,
            dynamic,
            touched: Default::default(),
        })
    }

    pub(crate) async fn uid_rules(&self, req: &Request, env: &Env) -> Result<Arc<RuleSet>> {
        if env.su {
            tracing::trace!(
                target: "odoo_kernel::rules",
                uid = env.uid, "superuser: no record rules apply"
            );
            return Ok(Arc::new(RuleSet::default()));
        }

        let mut seeds = self.reachable_seeds(req, env)?;
        seeds.sort();
        seeds.dedup();
        tracing::trace!(
            target: "odoo_kernel::rules",
            uid = env.uid, count = seeds.len(), models = ?seeds,
            "models the request can reach; each one's rules must be compiled"
        );
        let security_signals = self.registry.security_signals(&env.dynamic.signals);
        let key = RuleKey {
            uid: env.uid,
            company_id: env.company_id,
            company_ids: env.company_ids.clone(),
            signals: security_signals.clone(),
        };
        let base = self.caches.rule_cache.lock().await.get(&key).cloned();
        let seeds: Vec<String> = seeds
            .into_iter()
            .filter(|m| !base.as_ref().is_some_and(|b| b.is_compiled(m)))
            .collect();
        if seeds.is_empty()
            && let Some(cached) = base
        {
            tracing::trace!(
                target: "odoo_kernel::rules",
                uid = env.uid,
                company_id = env.company_id,
                compiled = cached.compiled_models().count(),
                "rule cache hit: every reachable model is already compiled for this identity"
            );
            return Ok(cached);
        }
        tracing::debug!(
            target: "odoo_kernel::rules",
            uid = env.uid,
            company_id = env.company_id,
            reused = base.is_some(),
            to_compile = seeds.len(),
            "compiling record rules"
        );
        let user = UserCtx {
            uid: env.uid,
            company_id: env.company_id,
            company_ids: env.company_ids.clone(),
            groups: env.groups.clone(),
        };
        let t_rules = std::time::Instant::now();

        let ctx = self.ctx(env).for_rules();
        let ruled: std::collections::HashSet<String> =
            env.dynamic.security.rules.keys().cloned().collect();
        let mut rules = match &base {
            Some(b) => (**b).clone(),
            None => RuleSet::with_ruled(ruled),
        };
        let mut memo: HierMemo = HashMap::new();
        let mut pending: Vec<String> = seeds;
        let mut seen: std::collections::HashSet<String> = pending.iter().cloned().collect();
        seen.extend(rules.compiled_models().cloned());
        let mut models_seen = 0usize;
        while let Some(model_name) = pending.pop() {
            models_seen += 1;

            for (parent, _) in self.registry.inherits_of(&model_name) {
                if seen.insert(parent.clone()) {
                    pending.push(parent.clone());
                }
            }
            let Some(model) = self.registry.lookup(&model_name) else {
                continue;
            };
            let built =
                match security::rules_domain(self.registry, &self.db, &model_name, &user).await {
                    Ok(Some(domain_json)) => match domain::parse(&domain_json)
                        .and_then(|n| domain::reject_internal_operators(&n))
                    {
                        Err(e) => Err(e),
                        Ok(()) => self
                            .resolve_hierarchy(&ctx, model, &domain_json, &mut memo, None)
                            .await
                            .and_then(|resolved| domain::parse(&resolved))
                            .map(Some),
                    },
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                };
            match built {
                Ok(Some(node)) => {
                    // a rule domain can name a comodel the request never
                    // mentioned; that comodel's own rules have to be
                    // compiled too, which is what widens the walk
                    let reached = self.comodels_of_with(&ctx, model, &node);
                    tracing::trace!(
                        target: "odoo_kernel::rules",
                        uid = env.uid, model = %model_name, comodels = ?reached,
                        "model is restricted; its rule domain reaches these comodels"
                    );
                    for co in reached {
                        if seen.insert(co.clone()) {
                            pending.push(co);
                        }
                    }
                    rules.insert(model_name, node)
                }
                Ok(None) => {
                    tracing::trace!(
                        target: "odoo_kernel::rules",
                        uid = env.uid, model = %model_name,
                        "no record rule applies to this identity"
                    );
                    rules.mark_unrestricted(model_name)
                }
                // Neither of these says anything about the rule, and the set
                // built here is CACHED for the identity: marking the model
                // unevaluated would refuse it for every later request too.
                // An offline compile that needs the database is exactly that
                // -- the first sweep with offline compiles cached 786 refusals
                // of res.users this way.
                Err(e)
                    if e.downcast_ref::<tokio_postgres::Error>().is_some()
                        || crate::error::needs_round_trip(&e) =>
                {
                    return Err(e);
                }
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
        self.caches.evict(&security_signals).await;
        Ok(arc)
    }

    fn reachable_seeds(&self, req: &Request, env: &Env) -> Result<Vec<String>> {
        let mut out = vec![req.model.clone()];
        let Some(model) = self.registry.lookup(&req.model) else {
            tracing::trace!(
                target: "odoo_kernel::rules",
                model = %req.model,
                "not in the registry; the reachability walk stops at the request's own model"
            );
            return Ok(out);
        };
        let ctx = self.ctx(env);
        let bare = |spec: &str| spec.split(':').next().unwrap_or(spec).to_string();
        let mut named: Vec<String> = req.fields.iter().map(|f| bare(f)).collect();
        named.extend(req.aggregates.iter().map(|a| bare(a)));
        named.extend(req.groupby_names_seen().iter().map(|g| bare(g)));
        for fname in named {
            if let Some(f) = model.fields.get(&fname)
                && let Some(co) = &f.relation
            {
                out.push(co.clone());
            }
        }

        let empty = json!([]);
        let domain = match req.domain {
            Json::Null => &empty,
            _ => &req.domain,
        };
        if let Some(node) = crate::domain::parse_nested(domain) {
            out.extend(self.comodels_of_with(&ctx, model, &node));
        }
        Ok(out)
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
            let Some(path) = ctx.normalize_path_seen(model, &raw) else {
                continue;
            };
            let mut m = model;
            for seg in &path {
                let Some(f) = m.fields.get(seg) else { break };
                let Some(co) = &f.relation else { break };
                out.push(co.clone());
                let Some(next) = self.registry.lookup(co) else {
                    break;
                };
                m = next;
            }

            if let Some(last) = out.last()
                && let (Some(co), Some(sub)) =
                    (self.registry.lookup(last), domain::parse_nested(&value))
                && !matches!(sub, domain::Node::True)
            {
                out.extend(self.comodels_of_with(ctx, co, &sub));
            }
        }
        out
    }

    async fn seeds_by_name(
        &self,
        ctx: &ExprCtx<'_>,
        target: &Model,
        names: &[Json],
        id_seeds: &[i32],
        active_filter: bool,
        scope: Option<(&RuleSet, bool)>,
    ) -> Result<Vec<i32>> {
        let mut terms: Vec<domain::Node> = names
            .iter()
            .map(|n| {
                domain::Node::Leaf(domain::Leaf {
                    field: "display_name".into(),
                    op: "ilike".into(),
                    value: n.clone(),
                })
            })
            .collect();
        if !id_seeds.is_empty() {
            terms.push(domain::Node::Leaf(domain::Leaf {
                field: "id".into(),
                op: "in".into(),
                value: json!(id_seeds),
            }));
        }
        let node = if terms.len() == 1 {
            terms.pop().unwrap()
        } else {
            domain::Node::Or(terms)
        };
        let empty = RuleSet::default();
        let (rules, su) = scope.unwrap_or((&empty, true));
        if !su {
            if let Some((uid, groups)) = &ctx.access {
                security::check_read_access(&ctx.dynamic, &target.name, *uid, groups)?;
            }
            rules.ensure_evaluated(&target.name)?;
        }
        let compiler = Compiler::root(ctx, target, rules, su);
        let mut cond = Cond::all().add(compiler.compile(&node)?);
        if let (Some(active_name), true) = (target.active_name.as_deref(), active_filter) {
            cond = cond.add(col(&target.table, active_name).is_in([true]));
        }
        if !su && let Some(rule_node) = rules.get(&target.name) {
            cond = cond.add(compiler.compile_rules(rule_node)?);
        }
        let mut select = sea_query::Query::select();
        select
            .expr(col(&target.table, "id"))
            .from(sea_query::Alias::new(&target.table))
            .cond_where(cond)
            .order_by(
                (
                    sea_query::Alias::new(&target.table),
                    sea_query::Alias::new("id"),
                ),
                sea_query::Order::Asc,
            );
        let (sql, values) = sea_query_postgres::PostgresBinder::build_postgres(
            &select,
            sea_query::PostgresQueryBuilder,
        );
        let rows = self.db.query(&sql, &values.as_params()).await?;
        // a hierarchy seeded by NAME resolves to ids first; how many it found
        // decides the size of the membership the caller then compiles
        tracing::debug!(
            target: "odoo_kernel::hierarchy",
            target = %target.name,
            names = names.len(),
            id_seeds = id_seeds.len(),
            active_filter,
            found = rows.len(),
            "resolved hierarchy seeds by display name"
        );
        Ok(rows.iter().map(|r| r.get::<_, i32>(0)).collect())
    }

    async fn resolve_hierarchy(
        &self,
        ctx: &ExprCtx<'_>,
        model: &Model,
        domain_json: &Json,
        memo: &mut HierMemo,
        scope: Option<(&RuleSet, bool)>,
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
                "child_of" | "parent_of" if fname != "id" && fname.contains('.') => {
                    let (head, rest) = fname.split_once('.').unwrap();
                    let rewritten = json!([head, "any", [[rest, op, leaf[2].clone()]]]);
                    let resolved = Box::pin(self.resolve_hierarchy(
                        ctx,
                        model,
                        &json!([rewritten]),
                        memo,
                        scope,
                    ))
                    .await?;
                    out.extend(resolved.as_array().cloned().unwrap_or_default());
                }
                "child_of" | "parent_of" => {
                    let mut seed_active = ctx.active_test;
                    let mut seed_via_search = false;
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
                            .ok_or_else(|| refusal!("{op} on non-relational {fname}"))?;
                        seed_active = match f.ttype {
                            FieldType::Many2one => false,
                            _ => f.context_active_test()?.unwrap_or(ctx.active_test),
                        };
                        seed_via_search = f.ttype == FieldType::Many2many;
                        if comodel_name == holder.name && f.ttype != FieldType::Many2one {
                            refuse!(
                                "{op} on {}.{}: a self-referencing x2many is walked as \
                                 its own parent link in Python, which no column expresses",
                                holder.name,
                                f.name
                            );
                        }
                        if comodel_name == holder.name && norm.len() == 1 {
                            (holder, Some(norm[0].clone()), "id".to_string())
                        } else {
                            let co = self.registry.get(&comodel_name)?;
                            (co, self.parent_name(co), norm.join("."))
                        }
                    };
                    let raw: Vec<Json> = match &leaf[2] {
                        Json::Array(a) => a.clone(),
                        Json::Bool(false) => Vec::new(),
                        Json::Number(_) | Json::String(_) => vec![leaf[2].clone()],
                        other => refuse!("unsupported {op} value {other}"),
                    };
                    if raw.iter().any(|v| v == &Json::Bool(true)) {
                        refuse!("True is not a valid hierarchy value");
                    }
                    let mut seeds: Vec<i32> = raw
                        .iter()
                        .filter_map(|v| v.as_i64().map(|i| i as i32))
                        .collect();
                    let names: Vec<Json> = raw.iter().filter(|v| v.is_string()).cloned().collect();
                    if !names.is_empty() || (seed_via_search && !seeds.is_empty()) {
                        let by_search = if seed_via_search {
                            std::mem::take(&mut seeds)
                        } else {
                            Vec::new()
                        };
                        let found = self
                            .seeds_by_name(
                                ctx,
                                target_model,
                                &names,
                                &by_search,
                                seed_active,
                                scope,
                            )
                            .await?;
                        seeds.extend(found);
                    }
                    let Some(parent_link) = parent_link else {
                        refuse!("{} has no parent field for {op}", target_model.name);
                    };
                    let down = op == "child_of";
                    let key = (
                        target_model.name.clone(),
                        parent_link.clone(),
                        down,
                        seeds.clone(),
                    );
                    tracing::debug!(
                        target: "odoo_kernel::hierarchy",
                        %op,
                        field = %fname,
                        target = %target_model.name,
                        link = %parent_link,
                        seeds = seeds.len(),
                        seeded_by_name = !names.is_empty(),
                        key_field = %key_field,
                        "resolving a hierarchy leaf"
                    );
                    let terms = match memo.get(&key) {
                        Some(hit) => {
                            tracing::trace!(
                                target: "odoo_kernel::hierarchy",
                                %op, target = %target_model.name,
                                "memo hit: the same hierarchy was already walked for this request"
                            );
                            hit.clone()
                        }
                        None => {
                            let terms =
                                if down && self.can_use_parent_path(target_model, &parent_link) {
                                    // _operator_child_of_domain keeps the prefix match in
                                    // the query rather than materialising every descendant
                                    let prefixes =
                                        self.parent_path_prefixes(target_model, &seeds).await?;
                                    tracing::debug!(
                                        target: "odoo_kernel::hierarchy",
                                        target = %target_model.name,
                                        prefixes = prefixes.len(),
                                        "using parent_path: the descendants stay a prefix match \
                                         in the query instead of being materialised"
                                    );
                                    let leaves: Vec<Json> = prefixes
                                        .iter()
                                        .map(|p| json!(["parent_path", "=like", p]))
                                        .collect();
                                    let or_domain = security::or_leaves(leaves);
                                    if key_field == "id" {
                                        or_domain
                                    } else {
                                        vec![json!([key_field, "any!", or_domain])]
                                    }
                                } else {
                                    let ids = if down {
                                        self.descendants(target_model, &parent_link, &seeds).await?
                                    } else {
                                        self.ancestors(target_model, &parent_link, &seeds).await?
                                    };
                                    // no parent_path to prefix-match, so the
                                    // whole closure is materialised as an id
                                    // list -- the row count here is the cost
                                    tracing::debug!(
                                        target: "odoo_kernel::hierarchy",
                                        target = %target_model.name,
                                        direction = if down { "descendants" } else { "ancestors" },
                                        resolved = ids.len(),
                                        "walked the hierarchy row by row"
                                    );
                                    vec![json!([key_field, "in", ids])]
                                };
                            memo.insert(key, terms.clone());
                            terms
                        }
                    };
                    out.extend(terms);
                }
                "any" | "not any" | "any!" | "not any!" => {
                    let path: Vec<String> = fname.split('.').map(str::to_string).collect();
                    let norm = ctx.normalize_path(model, &path)?;
                    let comodel = ctx.path_target(model, &norm)?;
                    let sub = Box::pin(self.resolve_hierarchy(ctx, comodel, &leaf[2], memo, scope))
                        .await?;
                    out.push(json!([fname, op, sub]));
                }
                _ => out.push(item.clone()),
            }
        }
        Ok(Json::Array(out))
    }

    fn parent_name(&self, model: &Model) -> Option<String> {
        let declared = model.parent_name.as_deref()?;
        let f = model.fields.get(declared)?;
        (f.has_column
            && f.ttype == FieldType::Many2one
            && f.relation.as_deref() == Some(model.name.as_str()))
        .then(|| declared.to_string())
    }

    fn can_use_parent_path(&self, model: &Model, parent_link: &str) -> bool {
        model.parent_store
            && model
                .fields
                .get("parent_path")
                .is_some_and(|f| f.has_column)
            && self.parent_name(model).as_deref() == Some(parent_link)
    }

    async fn parent_path_prefixes(&self, model: &Model, seeds: &[i32]) -> Result<Vec<String>> {
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        let rows = self
            .db
            .query(
                &format!(
                    "SELECT parent_path FROM {} WHERE id = ANY($1) ORDER BY id",
                    crate::db::ident(&model.table)
                ),
                &[&seeds],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get::<_, Option<String>>(0))
            .map(|p| format!("{p}%"))
            .collect())
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

        let mut all: BTreeSet<i32> = seeds.iter().copied().collect();
        let mut frontier: Vec<i32> = seeds.to_vec();
        let mut rounds = 0usize;
        while !frontier.is_empty() {
            rounds += 1;
            let rows = self
                .db
                .query(
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
        // one query per level of the tree: a deep hierarchy is round trips,
        // which is the argument for parent_store on the model
        tracing::debug!(
            target: "odoo_kernel::hierarchy",
            model = %model.name, link = %parent_link,
            seeds = seeds.len(), total = all.len(), rounds,
            "materialised the descendants one level at a time"
        );
        Ok(all.into_iter().map(|i| i as i64).collect())
    }

    async fn ancestors(&self, model: &Model, parent_link: &str, seeds: &[i32]) -> Result<Vec<i64>> {
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        if self.can_use_parent_path(model, parent_link) {
            let rows = self
                .db
                .query(
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
            tracing::debug!(
                target: "odoo_kernel::hierarchy",
                model = %model.name, seeds = seeds.len(), total = ids.len(),
                "read the ancestors out of parent_path in one query"
            );
            return Ok(ids.into_iter().collect());
        }
        let mut all: BTreeSet<i32> = seeds.iter().copied().collect();
        let mut frontier: Vec<i32> = seeds.to_vec();
        while !frontier.is_empty() {
            let rows = self
                .db
                .query(
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
        self.dispatch_with(req, None).await.map(|(raw, _)| raw)
    }

    /// `dispatch`, reusing a snapshot `checked` earlier in the caller's
    /// transaction and returning the one it ran against, so the caller can
    /// keep it for the rest of that transaction. See `snapshot`.
    pub async fn dispatch_with(
        &self,
        req: &Request,
        checked: Option<Arc<crate::registry::Dynamic>>,
    ) -> Result<(String, Arc<crate::registry::Dynamic>)> {
        let span = tracing::info_span!(
            "dispatch",
            id = req.id.as_deref().unwrap_or("-"),
            model = %req.model,
            method = %req.method,
            uid = ?req.uid,
            su = req.su,
        );
        {
            let _guard = span.enter();
            tracing::debug!(
                target: "odoo_kernel::dispatch",
                domain = %req.domain,
                fields = ?req.fields,
                groupby = %req.groupby,
                aggregates = ?req.aggregates,
                limit = ?req.limit,
                offset = ?req.offset,
                order = ?req.order,
                lang = ?req.lang,
                companies = ?req.allowed_company_ids,
                active_test = ?req.active_test,
                tz = ?req.tz,
                "request"
            );
        }
        let t0 = std::time::Instant::now();
        let out = self
            .dispatch_inner(req, checked)
            .instrument(span.clone())
            .await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let _guard = span.enter();
        match &out {
            Ok((raw, _)) => {
                tracing::debug!(
                    target: "odoo_kernel::dispatch",
                    ms,
                    bytes = raw.len(),
                    prepared = self.db.stmts.len(),
                    "ok"
                )
            }

            // The reason a routed call fell back to Python. Aggregated over a
            // corpus this is the backlog: each distinct reason is one
            // capability the kernel does not have, and `odoo_kernel::refusal`
            // carries the source line that produced it.
            Err(e) => tracing::info!(
                target: "odoo_kernel::dispatch",
                ms,
                kind = ?crate::error::ErrorKind::of(e),
                reason = %format!("{e:#}"),
                "refused"
            ),
        }
        out
    }

    pub async fn dispatch_in_transaction(&self, req: &Request) -> Result<String> {
        self.db
            .client
            .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await?;
        tracing::trace!(
            target: "odoo_kernel::sql",
            "opened a REPEATABLE READ READ ONLY transaction for this dispatch"
        );
        let out = self.dispatch(req).await;

        let end = if out.is_ok() { "COMMIT" } else { "ROLLBACK" };
        if let Err(e) = self.db.client.batch_execute(end).await {
            tracing::warn!(target: "odoo_kernel::sql", error = %e, "could not {end} the read transaction");
        }
        out
    }

    async fn dispatch_inner(
        &self,
        req: &Request,
        checked: Option<Arc<crate::registry::Dynamic>>,
    ) -> Result<(String, Arc<crate::registry::Dynamic>)> {
        Self::validate_shape(req)?;
        let t_signal = std::time::Instant::now();
        let dynamic = self.snapshot(req, checked).await?;
        let snapshot = dynamic.clone();
        let signal_ms = t_signal.elapsed().as_secs_f64() * 1000.0;
        let t_env = std::time::Instant::now();
        let env = self.build_env(req, dynamic).await?;
        // one line per dispatch, so `debug` like the phases in the reader it
        // precedes -- at `trace` a debug-level run saw the scan phases and not
        // these, and the two do not add up to the total without them
        tracing::debug!(
            target: "odoo_kernel::dispatch",
            signal_ms,
            env_ms = t_env.elapsed().as_secs_f64() * 1000.0,
            "preamble: checked the signalling watermark and resolved the identity"
        );

        // `_search` checks the ACL before anything else, so a denial on a model
        // the kernel cannot serve is still reported as the denial Python gives
        if !env.su {
            security::check_read_access(&env.dynamic, &req.model, env.uid, &env.groups)?;
        }
        let model = self.registry.get(&req.model)?;
        if let Some(over) = model.overridden_for(&req.method) {
            // the one refusal worth the model's whole capability line: which
            // of its read paths are pure decides what a future session has to
            // reimplement to route it
            self.registry.log_model_capabilities(model);
            refuse!(
                "{} overrides the read path in Python ({over}); the kernel cannot \
                 reproduce it from the columns",
                req.model
            );
        }
        let raw = match req.method.as_str() {
            "search_read" => self.search_read(req, &env).await,
            "search_count" => self.search_count(req, &env).await,
            "read_group" => self.read_group(req, &env).await,
            m => refuse!("unknown method {m}"),
        }?;
        Ok((raw, snapshot))
    }

    /// The WHERE clause `StorageBackend.search` would add for this domain and
    /// the caller's record rules, as Odoo's own SQL dialect: `%s` for every
    /// parameter, a literal `%` doubled, and the root table named by its table
    /// name, which is the alias Odoo's `Query` gives it.
    ///
    /// It is a fragment and not a statement on purpose. The caller attaches it
    /// to a lazy `Query` that the ORM goes on composing -- ordering, limits, a
    /// `search_count` around it, a sub-select inside another domain -- so what
    /// the kernel owns is exactly what `domain._to_sql` and the rule domain's
    /// `_to_sql` would have contributed, and nothing the query does after.
    ///
    /// `checked` is the snapshot a previous compile on this TRANSACTION already
    /// verified against the signalling watermark. The caller's transaction is
    /// REPEATABLE READ, so the watermark it can see does not move inside it, and
    /// checking again reads the same row; passing the snapshot back is what
    /// removes that round trip from every search after the first. It must be
    /// the snapshot that check produced and not `registry.dynamic()`, which
    /// another connection may have refreshed from a commit this transaction
    /// cannot see. The snapshot used is returned so the caller can keep it.
    pub async fn compile_where(
        &self,
        req: &Request,
        checked: Option<Arc<crate::registry::Dynamic>>,
    ) -> Result<CompiledWhere> {
        let dynamic = self.snapshot(req, checked).await?;
        let snapshot = dynamic.clone();
        let env = self.build_env(req, dynamic).await?;
        let model = self.registry.get(&req.model)?;
        let rules = if env.su {
            Arc::new(RuleSet::default())
        } else {
            self.uid_rules(req, &env).await?
        };
        let cond = self
            .build_condition(model, &req.domain, &env, &rules, req.trusted_domain)
            .await?;
        let mut select = sea_query::Query::select();
        select
            .expr(sea_query::Expr::cust("1"))
            .from(sea_query::Alias::new(&model.table))
            .cond_where(cond);
        let (sql, values) = sea_query_postgres::PostgresBinder::build_postgres(
            &select,
            sea_query::PostgresQueryBuilder,
        );
        let prefix = format!("SELECT 1 FROM {}", crate::db::ident(&model.table));
        let rest = sql.strip_prefix(&prefix).ok_or_else(|| {
            refusal!("compile_where: the rendered statement does not start with {prefix}: {sql}")
        })?;
        let fragment = match rest.strip_prefix(" WHERE ") {
            Some(w) => w,
            None if rest.is_empty() => "TRUE",
            None => refuse!("compile_where: unexpected clause after FROM: {rest}"),
        };
        let values: Vec<sea_query::Value> = values.0.into_iter().map(|v| v.0).collect();
        let (fragment, params) = crate::write::to_odoo_dialect(fragment, &values)?;
        let touched = env
            .touched
            .lock()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        Ok(CompiledWhere {
            fragment,
            params,
            touched,
            snapshot,
        })
    }

    fn validate_shape(req: &Request) -> Result<()> {
        tracing::trace!(
            target: "odoo_kernel::dispatch",
            method = %req.method, "validating the request shape"
        );
        let unsupported = |name: &str| -> Result<()> {
            refuse!(
                "{} does not support `{name}`; refusing rather than ignoring it",
                req.method
            )
        };
        let groupby_given = !matches!(req.groupby, Json::Null)
            && !matches!(&req.groupby, Json::Array(a) if a.is_empty());
        match req.method.as_str() {
            "search_read" => {
                if req.fields.is_empty() {
                    refuse!(
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
                if req.x2many_active_test.is_some() {
                    unsupported("x2many_active_test")?;
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
                if req.x2many_active_test.is_some() {
                    unsupported("x2many_active_test")?;
                }
                if let Some(order) = &req.order {
                    let specs = req.groupby_names()?;
                    for part in order.split(',') {
                        let Some(term) = crate::sqlgen::parse_order_term(part)? else {
                            continue;
                        };
                        let known = term.field == "__count"
                            || specs.iter().any(|s| s == term.field)
                            || req.aggregates.iter().any(|a| a == term.field);
                        if !known {
                            refuse!(
                                "read_group order term {:?} is neither a groupby nor one \
                                 of the requested aggregates; refusing {order:?} rather \
                                 than guessing",
                                term.field
                            );
                        }
                    }
                }
            }
            other => refuse!("unknown method {other}"),
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
        let mut ctx = ctx.with_tz(env.comparand_tz.clone());
        ctx.touched = env.touched.clone();
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
        trusted: bool,
    ) -> Result<Condition> {
        let empty = json!([]);
        let domain_json = if domain_json.is_null() {
            &empty
        } else {
            domain_json
        };

        let mut memo: HierMemo = HashMap::new();
        let ctx = self.ctx(env);
        let t0 = std::time::Instant::now();
        if !trusted {
            domain::reject_internal_operators(&domain::parse(domain_json)?)?;
        }
        let resolved = self
            .resolve_hierarchy(&ctx, model, domain_json, &mut memo, Some((rules, env.su)))
            .await?;
        let hierarchy_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let node = domain::parse(&resolved)?;

        let t_compile = std::time::Instant::now();
        let compiler = Compiler::root(&ctx, model, rules, env.su);
        let mut cond = Cond::all().add(compiler.compile(&node)?);

        let mut implicit_active = false;
        if let Some(active_name) = model.active_name.as_deref()
            && env.root_active_test
        {
            let mut referenced = Vec::new();
            domain::referenced_fields(&node, &mut referenced);
            // Odoo adds the active clause only when the domain does not name
            // the field itself; a domain that does opts out of active_test
            if !referenced.iter().any(|f| f == active_name) {
                implicit_active = true;
                ctx.touch(&model.name, active_name);
                cond = cond.add(col(&model.table, active_name).is_in([true]));
            }
        }
        let mut rule_applied = false;
        if !env.su {
            rules.ensure_evaluated(&model.name)?;
            if let Some(rule_node) = rules.get(&model.name) {
                rule_applied = true;
                cond = cond.add(compiler.compile_rules(rule_node)?);
            }
        }
        tracing::debug!(
            target: "odoo_kernel::compile",
            model = %model.name,
            hierarchy_ms,
            hierarchy_queries = memo.len(),
            compile_ms = t_compile.elapsed().as_secs_f64() * 1000.0,
            implicit_active,
            rule_applied,
            "built the WHERE condition"
        );
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
            langs: Vec::new(),
            week_start: HashMap::new(),
        }
    }

    fn key(signals: &crate::registry::Signals) -> RuleKey {
        RuleKey {
            uid: 2,
            company_id: 1,
            company_ids: vec![1],
            signals: signals.clone(),
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
