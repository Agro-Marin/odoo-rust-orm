use crate::error::{refusal, refuse};
use std::collections::HashMap;

use anyhow::Result;
use tokio_postgres::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Boolean,
    Integer,
    Float,
    Monetary,
    Char,
    Text,
    Html,
    Date,
    Datetime,
    Selection,
    Many2one,
    One2many,
    Many2many,
    Binary,
    Json,
    Properties,
    Reference,
    Many2oneReference,
    Other,
}

impl FieldType {
    pub fn parse(s: &str) -> Self {
        match s {
            "boolean" => Self::Boolean,
            "integer" => Self::Integer,
            "float" => Self::Float,
            "monetary" => Self::Monetary,
            "char" => Self::Char,
            "text" => Self::Text,
            "html" => Self::Html,
            "date" => Self::Date,
            "datetime" => Self::Datetime,
            "selection" => Self::Selection,
            "many2one" => Self::Many2one,
            "one2many" => Self::One2many,
            "many2many" => Self::Many2many,
            "binary" => Self::Binary,
            "json" => Self::Json,
            "properties" | "properties_definition" => Self::Properties,
            "reference" => Self::Reference,
            "many2one_reference" => Self::Many2oneReference,
            _ => Self::Other,
        }
    }

    pub fn is_text(self) -> bool {
        matches!(self, Self::Char | Self::Text | Self::Html)
    }

    pub fn falsy_json_for_type(self, name: &str) -> Option<serde_json::Value> {
        use serde_json::json;
        if name == "id" {
            return None;
        }
        match self {
            Self::Char | Self::Text | Self::Html => Some(json!("")),
            Self::Integer => Some(json!(0)),
            Self::Float | Self::Monetary => Some(json!(0.0)),
            Self::Boolean => Some(json!(false)),
            Self::Many2oneReference => Some(json!(0)),
            _ => None,
        }
    }
}

pub const DEBUG_GROUP: &str = "base.group_no_one";

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ttype: FieldType,

    pub relation: Option<String>,

    pub relation_table: Option<String>,
    pub column1: Option<String>,
    pub column2: Option<String>,

    pub relation_field: Option<String>,
    pub company_dependent: bool,

    pub has_column: bool,

    pub stored: bool,

    pub pg_type: String,
    pub not_null: bool,

    pub translated: bool,

    pub translate_whole: bool,

    pub column_cast: Option<String>,

    pub related: Option<String>,

    pub domain: Option<serde_json::Value>,
    pub domain_callable: bool,

    pub model_field: Option<String>,

    pub index: Option<String>,

    pub cd_fallback: Option<serde_json::Value>,

    pub custom_search: bool,

    pub context: Option<serde_json::Value>,

    pub groups: Option<String>,

    pub falsy: Option<serde_json::Value>,
    pub bypass_search_access: Option<bool>,

    pub compute_sudo: bool,
    pub inherited: bool,
    pub required: bool,
    pub group_by_field: Option<String>,
    pub order_by_field: Option<String>,
    pub search_kind: Option<String>,
}

impl Field {
    pub fn falsy_json(&self) -> Option<serde_json::Value> {
        self.falsy.clone()
    }

    pub fn inequality_falsy_json(&self) -> Option<serde_json::Value> {
        self.falsy_json()
    }

    pub fn comodel(&self) -> Result<&str> {
        self.relation
            .as_deref()
            .ok_or_else(|| refusal!("field {} has no comodel", self.name))
    }

    fn computed_x2many_refusal(&self, kind: &str, reads: &str) -> Result<()> {
        if self.stored {
            return Ok(());
        }
        refuse!(
            "{kind} {} is computed in Python{}, so its {reads} does not hold its value",
            self.name,
            match &self.related {
                Some(r) => format!(" (related: {r})"),
                None => String::new(),
            }
        )
    }

    pub fn m2m_columns(&self) -> Result<(&str, &str, &str)> {
        self.computed_x2many_refusal("many2many", "relation table")?;
        match (
            self.relation_table.as_deref(),
            self.column1.as_deref(),
            self.column2.as_deref(),
        ) {
            (Some(t), Some(c1), Some(c2)) => Ok((t, c1, c2)),
            _ => refuse!(
                "many2many {} is stored but its relation table/columns are \
                 missing from the registry",
                self.name
            ),
        }
    }

    pub fn context_active_test(&self) -> Result<Option<bool>> {
        let Some(ctx) = &self.context else {
            return Ok(None);
        };
        let obj = ctx
            .as_object()
            .ok_or_else(|| refusal!("field {} has a non-object context", self.name))?;
        let mut out = None;
        for (key, value) in obj {
            match key.as_str() {
                "active_test" => {
                    out = Some(value.as_bool().ok_or_else(|| {
                        refusal!("field {} has a non-boolean active_test", self.name)
                    })?)
                }
                other => refuse!(
                    "field {} carries the context key {other:?}, which this kernel does \
                     not model; refusing rather than reading it as absent",
                    self.name
                ),
            }
        }
        Ok(out)
    }

    pub fn o2m_inverse_column(&self, owner: &str, co: &Model) -> Result<&str> {
        let inverse = self.o2m_inverse()?;
        match co.fields.get(inverse) {
            Some(f) if f.has_column => Ok(inverse),
            Some(_) => refuse!(
                "one2many {}.{} inverts {}.{}, which is computed in Python and \
                 has no column to join on",
                owner,
                self.name,
                co.name,
                inverse
            ),
            None => refuse!(
                "one2many {}.{} names an inverse {}.{} that the registry does \
                 not have",
                owner,
                self.name,
                co.name,
                inverse
            ),
        }
    }

    pub fn o2m_inverse(&self) -> Result<&str> {
        self.computed_x2many_refusal("one2many", "inverse column")?;
        self.relation_field
            .as_deref()
            .ok_or_else(|| refusal!("one2many {} has no inverse field", self.name))
    }
}

#[derive(Debug, Clone)]
pub struct Model {
    pub name: String,
    pub table: String,

    pub order: String,
    pub fields: HashMap<String, Field>,

    pub rec_name: Option<String>,

    pub parent_name: Option<String>,
    pub parent_store: bool,

    pub read_path_pure: bool,

    pub impure_read_methods: Vec<String>,

    pub search_pure: bool,

    pub name_search_fields: Option<Vec<String>>,

    pub display_name_search_exact: Vec<String>,

    pub display_name_default: bool,

    pub order_pure: bool,

    pub read_group_pure: bool,

    pub display_name_access_pure: bool,

    pub check_access_pure: bool,

    pub active_name: Option<String>,

    pub display_name_column: Vec<String>,
    pub display_name_guard: Option<String>,
}

impl Model {
    pub fn overridden_for(&self, method: &str) -> Option<String> {
        const ALL: &[&str] = &[
            "_search",
            "read",
            "search_read",
            "_read_group",
            "search_count",
            "search",
            "search_fetch",
            "fetch",
            "_fetch_query",
            "_field_to_sql",
        ];
        let needs: &[&str] = match method {
            "search_read" => &[
                "_search",
                "search_read",
                "search_fetch",
                "fetch",
                "_fetch_query",
                "_field_to_sql",
            ],
            "search_count" => &["_search", "search_count"],
            "read_group" => &["_search", "_read_group", "_field_to_sql"],
            "labels" => &["search_fetch", "fetch", "_fetch_query", "_field_to_sql"],
            _ => ALL,
        };
        let hit: Vec<&str> = self
            .impure_read_methods
            .iter()
            .map(String::as_str)
            .filter(|m| needs.contains(m))
            .collect();
        (!hit.is_empty()).then(|| hit.join(" / "))
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub groups: Vec<i32>,
    pub restrict: bool,
    pub domain_force: Option<String>,
    pub parsed: Option<std::result::Result<crate::security::PyExpr, String>>,
}

#[derive(Debug, Default, Clone)]
pub struct Security {
    pub implied: HashMap<i32, Vec<i32>>,

    pub user_groups: HashMap<i32, Vec<i32>>,

    pub access: HashMap<String, Vec<Option<i32>>>,

    pub rules: HashMap<String, Vec<Rule>>,

    pub user_companies: HashMap<i32, Vec<i32>>,
}

impl Security {
    pub fn groups_of(&self, uid: i32) -> std::collections::HashSet<i32> {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<i32> = self.user_groups.get(&uid).cloned().unwrap_or_default();
        while let Some(g) = stack.pop() {
            if seen.insert(g)
                && let Some(implied) = self.implied.get(&g)
            {
                stack.extend(implied.iter().copied());
            }
        }
        seen
    }
}

pub const SIGNALING_TABLES: [&str; 7] = [
    "orm_signaling_registry",
    "orm_signaling_default",
    "orm_signaling_assets",
    "orm_signaling_stable",
    "orm_signaling_templates",
    "orm_signaling_routing",
    "orm_signaling_groups",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Bootstrap,

    Export,
}

pub type Signals = Vec<Option<i64>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalChange {
    None,

    Security,

    Registry,

    Irrelevant,
}

pub const SECURITY_SIGNAL_TABLES: [&str; 3] = [
    "orm_signaling_default",
    "orm_signaling_groups",
    "orm_signaling_stable",
];

pub fn classify_signal_change(tables: &[String], old: &Signals, new: &Signals) -> SignalChange {
    if old == new {
        return SignalChange::None;
    }
    let moved = |name: &str| {
        tables
            .iter()
            .enumerate()
            .any(|(i, t)| t == name && old.get(i) != new.get(i))
    };
    if moved("orm_signaling_registry") {
        SignalChange::Registry
    } else if SECURITY_SIGNAL_TABLES.iter().any(|t| moved(t)) {
        SignalChange::Security
    } else {
        SignalChange::Irrelevant
    }
}

pub fn security_signals(tables: &[String], signals: &Signals) -> Signals {
    tables
        .iter()
        .zip(signals)
        .filter(|(t, _)| SECURITY_SIGNAL_TABLES.contains(&t.as_str()))
        .map(|(_, v)| *v)
        .collect()
}

#[derive(Debug, Clone)]
pub struct Dynamic {
    pub security: Security,

    pub defaults: HashMap<String, HashMap<String, CompanyDefault>>,

    pub signals: Signals,

    pub langs: Vec<String>,

    pub week_start: HashMap<String, i32>,
}

#[derive(Debug)]
pub struct Registry {
    pub models: HashMap<String, Model>,

    pub has_unaccent: bool,

    pub has_trigram: bool,

    pub source: Source,

    signal_tables: Vec<String>,

    inherits: HashMap<String, Vec<(String, String)>>,

    pub group_ids: HashMap<String, i32>,

    pub timezones: std::collections::HashSet<String>,

    pub timezone_aliases: HashMap<String, String>,

    signals_sql: Option<String>,
    dynamic: std::sync::RwLock<std::sync::Arc<Dynamic>>,
}

#[derive(Debug, Default, Clone)]
pub struct CompanyDefault {
    pub ordered: Vec<(Option<i32>, serde_json::Value)>,
}

impl CompanyDefault {
    pub fn fallback(&self, company_id: i32) -> Option<&serde_json::Value> {
        self.ordered
            .iter()
            .find(|(c, _)| c.is_none_or(|c| c == company_id))
            .map(|(_, v)| v)
            .filter(|v| !v.is_null())
    }
}

impl Registry {
    pub fn new(
        models: HashMap<String, Model>,
        langs: Vec<String>,
        has_unaccent: bool,
        dynamic: Dynamic,
    ) -> Self {
        Registry::with_signals_sql(models, langs, has_unaccent, None, dynamic, Source::Export)
    }

    pub fn with_signals_sql(
        models: HashMap<String, Model>,
        langs: Vec<String>,
        has_unaccent: bool,
        signals_sql: Option<String>,
        mut dynamic: Dynamic,
        source: Source,
    ) -> Self {
        dynamic.langs = langs;
        Registry {
            models,
            has_unaccent,
            has_trigram: false,
            source,
            signal_tables: Vec::new(),
            inherits: HashMap::new(),
            group_ids: HashMap::new(),
            timezones: std::collections::HashSet::new(),
            timezone_aliases: HashMap::new(),
            signals_sql,
            dynamic: std::sync::RwLock::new(std::sync::Arc::new(dynamic)),
        }
    }

    pub fn field_readable(
        &self,
        field: &Field,
        user_groups: &std::collections::HashSet<i32>,
    ) -> bool {
        let Some(spec) = field.groups.as_deref() else {
            return true;
        };
        if spec == "." {
            return false;
        }
        let (mut positives, mut negatives) = (Vec::new(), Vec::new());
        for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match token.strip_prefix('!') {
                Some(rest) => negatives.push(rest),
                None => positives.push(token),
            }
        }
        if positives.is_empty() && negatives.is_empty() {
            return false;
        }
        let holds = |xmlid: &str| {
            xmlid != DEBUG_GROUP
                && self
                    .group_ids
                    .get(xmlid)
                    .is_some_and(|id| user_groups.contains(id))
        };
        if negatives.iter().any(|x| holds(x)) {
            return false;
        }
        if positives.iter().any(|x| holds(x)) {
            return true;
        }
        positives.is_empty()
    }

    pub async fn load_group_ids(client: &Client) -> Result<HashMap<String, i32>> {
        let t0 = std::time::Instant::now();
        let mut out = HashMap::new();
        for row in client
            .query(
                "SELECT DISTINCT ON (res_id) module || '.' || name, res_id FROM ir_model_data
                  WHERE model = 'res.groups' ORDER BY res_id, id",
                &[],
            )
            .await?
        {
            out.insert(row.get(0), row.get(1));
        }
        tracing::debug!(
            target: "odoo_kernel::registry",
            groups = out.len(),
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "loaded the xmlid -> res.groups map that field `groups=` specs resolve through"
        );
        Ok(out)
    }

    pub fn inherits_of(&self, model: &str) -> &[(String, String)] {
        self.inherits.get(model).map_or(&[], Vec::as_slice)
    }

    pub fn set_inherits(&mut self, inherits: HashMap<String, Vec<(String, String)>>) {
        self.inherits = inherits;
    }

    pub async fn load_inherits(client: &Client) -> Result<HashMap<String, Vec<(String, String)>>> {
        let t0 = std::time::Instant::now();
        let mut out: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for row in client
            .query(
                "SELECT cm.model, pm.model, f.name
                   FROM ir_model_inherit i
                   JOIN ir_model cm ON i.model_id = cm.id
                   JOIN ir_model pm ON i.parent_id = pm.id
                   JOIN ir_model_fields f ON i.parent_field_id = f.id
                  ORDER BY cm.model, pm.model",
                &[],
            )
            .await?
        {
            out.entry(row.get(0))
                .or_default()
                .push((row.get(1), row.get(2)));
        }
        tracing::debug!(
            target: "odoo_kernel::registry",
            delegating_models = out.len(),
            links = out.values().map(Vec::len).sum::<usize>(),
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "loaded the _inherits graph"
        );
        Ok(out)
    }

    pub fn resolve_timezone(&self, name: &str) -> Option<String> {
        if self.timezones.contains(name) {
            return Some(name.to_string());
        }
        self.timezone_aliases
            .get(name)
            .filter(|c| self.timezones.contains(c.as_str()))
            .cloned()
    }

    pub fn dynamic(&self) -> std::sync::Arc<Dynamic> {
        self.dynamic.read().unwrap().clone()
    }

    pub async fn refresh_dynamic(
        &self,
        client: &Client,
        signals: Signals,
    ) -> Result<std::sync::Arc<Dynamic>> {
        let t0 = std::time::Instant::now();
        let fresh = Dynamic {
            security: Self::load_security(client).await?,
            defaults: Self::load_defaults(client).await?,
            langs: Self::load_langs(client).await?,
            week_start: Self::load_week_starts(client).await?,
            signals,
        };
        tracing::info!(
            target: "odoo_kernel::registry",
            ruled_models = fresh.security.rules.len(),
            access_models = fresh.security.access.len(),
            default_models = fresh.defaults.len(),
            langs = fresh.langs.len(),
            signals = ?fresh.signals,
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "refreshed the dynamic snapshot (security, defaults, languages)"
        );
        let fresh = std::sync::Arc::new(fresh);
        *self.dynamic.write().unwrap() = fresh.clone();
        Ok(fresh)
    }

    pub fn set_dynamic(&self, fresh: Dynamic) {
        *self.dynamic.write().unwrap() = std::sync::Arc::new(fresh);
    }

    pub fn stamp_signals(
        &self,
        checked: &std::sync::Arc<Dynamic>,
        signals: Signals,
    ) -> std::sync::Arc<Dynamic> {
        let mut guard = self.dynamic.write().unwrap();
        let mut fresh = (**checked).clone();
        fresh.signals = signals;
        let fresh = std::sync::Arc::new(fresh);
        let published = std::sync::Arc::ptr_eq(&guard, checked);
        if published {
            *guard = fresh.clone();
        }
        tracing::debug!(
            target: "odoo_kernel::signal",
            published,
            signals = ?fresh.signals,
            "stamped the watermark onto the snapshot this request read"
        );
        fresh
    }

    pub fn snapshot_precedes(&self, snapshot: &Signals, current: &Signals) -> bool {
        self.signal_tables.iter().enumerate().any(|(i, name)| {
            (name == "orm_signaling_registry" || SECURITY_SIGNAL_TABLES.contains(&name.as_str()))
                && snapshot.get(i) < current.get(i)
        })
    }

    pub fn security_signals(&self, signals: &Signals) -> Signals {
        security_signals(&self.signal_tables, signals)
    }

    pub async fn load_week_starts(client: &Client) -> Result<HashMap<String, i32>> {
        Ok(client
            .query("SELECT code, week_start FROM res_lang WHERE active", &[])
            .await?
            .iter()
            .filter_map(|r| {
                let code: String = r.get(0);
                let raw: Option<String> = r.get(1);
                raw?.trim().parse::<i32>().ok().map(|w| (code, w))
            })
            .collect())
    }

    pub async fn load_langs(client: &Client) -> Result<Vec<String>> {
        let langs: Vec<String> = client
            .query("SELECT code FROM res_lang WHERE active", &[])
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        tracing::debug!(
            target: "odoo_kernel::registry",
            count = langs.len(),
            langs = ?langs,
            "loaded the active languages a request may name"
        );
        Ok(langs)
    }

    pub fn get(&self, model: &str) -> Result<&Model> {
        self.models
            .get(model)
            .ok_or_else(|| refusal!("unknown or table-less model {model}"))
    }

    pub fn lookup(&self, model: &str) -> Option<&Model> {
        self.models.get(model)
    }

    pub fn log_model_capabilities(&self, model: &Model) {
        tracing::debug!(
            target: "odoo_kernel::registry",
            model = %model.name,
            table = %model.table,
            fields = model.fields.len(),
            order = %model.order,
            read_path_pure = model.read_path_pure,
            search_pure = model.search_pure,
            order_pure = model.order_pure,
            read_group_pure = model.read_group_pure,
            display_name_default = model.display_name_default,
            display_name_access_pure = model.display_name_access_pure,
            check_access_pure = model.check_access_pure,
            impure_read_methods = ?model.impure_read_methods,
            rec_name = ?model.rec_name,
            active_name = ?model.active_name,
            parent_store = model.parent_store,
            "model capabilities"
        );
    }

    pub async fn load_has_trigram(client: &Client) -> Result<bool> {
        Ok(client
            .query_opt(
                "SELECT 1 FROM pg_opclass WHERE opcname = 'gin_trgm_ops'",
                &[],
            )
            .await?
            .is_some())
    }

    pub async fn load_has_unaccent(client: &Client) -> Result<bool> {
        let row = client
            .query_opt(
                "SELECT p.provolatile FROM pg_proc p
                 WHERE p.proname = 'unaccent'
                   AND p.pronamespace = current_schema::regnamespace
                   AND p.pronargs = 1",
                &[],
            )
            .await?;
        Ok(row.is_some())
    }

    pub async fn build_signals_sql(client: &Client) -> Result<Option<String>> {
        Ok(Self::build_signals_plan(client).await?.map(|(sql, _)| sql))
    }

    pub async fn build_signals_plan(client: &Client) -> Result<Option<(String, Vec<String>)>> {
        let existing: Vec<String> = client
            .query(
                "SELECT table_name FROM information_schema.tables
                 WHERE table_schema = current_schema AND table_name = ANY($1)
                 ORDER BY table_name",
                &[&SIGNALING_TABLES.to_vec()],
            )
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        if existing.is_empty() {
            return Ok(None);
        }

        let selects: Vec<String> = existing
            .iter()
            .map(|t| format!("(SELECT max(id)::bigint FROM {t})"))
            .collect();
        Ok(Some((format!("SELECT {}", selects.join(", ")), existing)))
    }

    pub fn signal_change(&self, old: &Signals, new: &Signals) -> SignalChange {
        classify_signal_change(&self.signal_tables, old, new)
    }

    pub fn signals_sql(&self) -> Option<&str> {
        self.signals_sql.as_deref()
    }

    pub fn signals_of(row: &tokio_postgres::Row) -> Signals {
        (0..row.len())
            .map(|i| row.get::<_, Option<i64>>(i))
            .collect()
    }

    pub async fn load_schema(
        client: &Client,
    ) -> Result<HashMap<String, HashMap<String, (String, bool)>>> {
        let t0 = std::time::Instant::now();
        let mut schema: HashMap<String, HashMap<String, (String, bool)>> = HashMap::new();
        for row in client
            .query(
                "SELECT table_name, column_name, udt_name, is_nullable
                 FROM information_schema.columns
                 WHERE table_schema = current_schema",
                &[],
            )
            .await?
        {
            let table: String = row.get(0);
            let col: String = row.get(1);
            let udt: String = row.get(2);
            let nullable: String = row.get(3);
            schema
                .entry(table)
                .or_default()
                .insert(col, (udt, nullable == "NO"));
        }
        tracing::debug!(
            target: "odoo_kernel::registry",
            tables = schema.len(),
            columns = schema.values().map(HashMap::len).sum::<usize>(),
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "read information_schema; a field with no column here is not stored"
        );
        Ok(schema)
    }

    pub async fn load(client: &Client) -> Result<Registry> {
        let t0 = std::time::Instant::now();
        tracing::info!(
            target: "odoo_kernel::registry",
            source = "bootstrap",
            "building the registry from ir_model; no model is marked pure, so \
             every dispatch will refuse"
        );
        let schema = Self::load_schema(client).await?;

        let mut models: HashMap<String, Model> = HashMap::new();
        let mut skipped_tableless = 0usize;
        for row in client
            .query("SELECT model, \"order\" FROM ir_model", &[])
            .await?
        {
            let name: String = row.get(0);
            let order: String = row.get(1);
            let table = name.replace('.', "_");

            if !schema.contains_key(&table) {
                tracing::trace!(
                    target: "odoo_kernel::registry",
                    model = %name, %table,
                    "skipped: ir_model names it but no table backs it"
                );
                skipped_tableless += 1;
                continue;
            }
            models.insert(
                name.clone(),
                Model {
                    name,
                    table,
                    order,
                    fields: HashMap::new(),
                    rec_name: None,
                    parent_name: None,
                    parent_store: false,
                    active_name: None,
                    display_name_column: Vec::new(),
                    display_name_guard: None,

                    read_path_pure: false,
                    impure_read_methods: vec!["_search".to_string()],
                    search_pure: false,
                    display_name_default: false,
                    order_pure: false,
                    read_group_pure: false,
                    display_name_access_pure: false,
                    check_access_pure: false,
                    name_search_fields: None,
                    display_name_search_exact: Vec::new(),
                },
            );
        }

        for row in client
            .query(
                "SELECT model, name, ttype, relation, relation_table, column1, column2,
                        relation_field, COALESCE(company_dependent, false), store,
                        related
                 FROM ir_model_fields WHERE store = true OR related IS NOT NULL",
                &[],
            )
            .await?
        {
            let model_name: String = row.get(0);
            let Some(model) = models.get_mut(&model_name) else {
                continue;
            };
            let name: String = row.get(1);
            let ttype = FieldType::parse(row.get::<_, String>(2).as_str());
            let company_dependent: bool = row.get(8);
            let store: bool = row.get(9);
            let related: Option<String> = row.get(10);
            let col_info = if store {
                schema.get(&model.table).and_then(|cols| cols.get(&name))
            } else {
                None
            };
            let (pg_type, not_null) = col_info.map(|(t, n)| (t.clone(), *n)).unwrap_or_default();
            let translated = ttype.is_text() && pg_type == "jsonb" && !company_dependent;
            let falsy = ttype.falsy_json_for_type(&name);
            model.fields.insert(
                name.clone(),
                Field {
                    name,
                    ttype,
                    relation: row.get(3),
                    relation_table: row.get(4),
                    column1: row.get(5),
                    column2: row.get(6),
                    relation_field: row.get(7),
                    company_dependent,
                    has_column: col_info.is_some(),
                    stored: store,
                    pg_type,
                    not_null,
                    translated,
                    translate_whole: false,
                    column_cast: None,
                    related: if store { None } else { related },

                    domain: None,
                    domain_callable: false,
                    model_field: None,
                    index: None,
                    cd_fallback: None,

                    custom_search: false,
                    context: None,

                    groups: None,
                    falsy,
                    bypass_search_access: None,
                    compute_sudo: false,
                    inherited: false,
                    required: false,
                    group_by_field: None,
                    order_by_field: None,
                    search_kind: None,
                },
            );
        }

        tracing::debug!(
            target: "odoo_kernel::registry",
            models = models.len(),
            skipped_tableless,
            fields = models.values().map(|m| m.fields.len()).sum::<usize>(),
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "read ir_model and ir_model_fields"
        );
        Self::finalize(client, models, Source::Bootstrap).await
    }

    pub async fn from_export(client: &Client, export: &serde_json::Value) -> Result<Registry> {
        let t0 = std::time::Instant::now();
        let expected_sequence = export["registry_sequence"].as_i64().ok_or_else(|| {
            refusal!("export has no registry_sequence; regenerate it from the live Python registry")
        })?;
        tracing::info!(
            target: "odoo_kernel::registry",
            source = "export",
            expected_sequence,
            exported_db = export["db"].as_str().unwrap_or("-"),
            "building the registry from a live-registry export"
        );
        if let Some(stamped) = export["db"].as_str() {
            let here: String = client
                .query_one("SELECT current_database()", &[])
                .await?
                .get(0);
            if stamped != here {
                refuse!(
                    "the export describes database {stamped:?}; this connection is to \
                     {here:?}. Re-export against the database being served"
                );
            }
        }
        let schema = Self::load_schema(client).await?;
        let mut models: HashMap<String, Model> = HashMap::new();
        let export_models = export["models"]
            .as_object()
            .ok_or_else(|| refusal!("export missing models"))?;
        let mut skipped_tableless = 0usize;
        let mut hooked_total = 0usize;
        for (name, em) in export_models {
            let table = em["table"].as_str().unwrap_or_default().to_string();
            let Some(cols) = schema.get(&table) else {
                tracing::trace!(
                    target: "odoo_kernel::registry",
                    model = %name, %table,
                    "skipped: the export names it but no table backs it here"
                );
                skipped_tableless += 1;
                continue;
            };
            let mut fields: HashMap<String, Field> = HashMap::new();
            for (fname, ef) in em["fields"].as_object().into_iter().flatten() {
                let ttype = FieldType::parse(ef["type"].as_str().unwrap_or(""));
                let store = ef["store"].as_bool().unwrap_or(false);
                let company_dependent = ef["company_dependent"].as_bool().unwrap_or(false);
                let col_info = if store { cols.get(fname) } else { None };
                let (pg_type, not_null) =
                    col_info.map(|(t, n)| (t.clone(), *n)).unwrap_or_default();
                let translated = ttype.is_text() && pg_type == "jsonb" && !company_dependent;
                let related = ef["related"].as_str().map(str::to_string);
                fields.insert(
                    fname.clone(),
                    Field {
                        name: fname.clone(),
                        ttype,
                        relation: ef["relation"].as_str().map(str::to_string),
                        relation_table: ef["relation_table"].as_str().map(str::to_string),
                        column1: ef["column1"].as_str().map(str::to_string),
                        column2: ef["column2"].as_str().map(str::to_string),
                        relation_field: ef["inverse_name"].as_str().map(str::to_string),
                        company_dependent,
                        has_column: col_info.is_some(),
                        stored: store,
                        pg_type,
                        not_null,
                        translated,
                        translate_whole: ef["translate_whole"].as_bool().unwrap_or(false),
                        column_cast: ef["column_cast"].as_str().map(str::to_string),
                        related: if store { None } else { related },
                        domain: ef["domain"]
                            .as_array()
                            .filter(|a| !a.is_empty())
                            .map(|a| serde_json::Value::Array(a.clone())),
                        domain_callable: ef["domain"]["__callable__"].as_bool().unwrap_or(false),
                        model_field: ef["model_field"].as_str().map(str::to_string),
                        index: ef["index"].as_str().map(str::to_string),
                        cd_fallback: match &ef["company_dependent_fallback"] {
                            serde_json::Value::Null => None,
                            v => Some(v.clone()),
                        },
                        custom_search: ef["custom_search"].as_bool().unwrap_or(false),
                        context: match &ef["context"] {
                            serde_json::Value::Null => None,
                            v => Some(v.clone()),
                        },
                        groups: ef["groups"].as_str().map(str::to_string),
                        falsy: match ef.get("falsy_value") {
                            None => ttype.falsy_json_for_type(fname),
                            Some(serde_json::Value::Null) => None,
                            Some(v) => Some(v.clone()),
                        },
                        bypass_search_access: ef
                            .get("bypass_search_access")
                            .and_then(serde_json::Value::as_bool),
                        compute_sudo: ef["compute_sudo"].as_bool().unwrap_or(false),
                        inherited: ef["inherited"].as_bool().unwrap_or(false),
                        required: ef["required"].as_bool().unwrap_or(false),
                        group_by_field: ef["group_by_field"].as_str().map(str::to_string),
                        order_by_field: ef["order_by_field"].as_str().map(str::to_string),
                        search_kind: ef["search_kind"].as_str().map(str::to_string),
                    },
                );
            }
            for hooked in em["hooked_fields"].as_array().into_iter().flatten() {
                if let Some(fname) = hooked.as_str() {
                    if fields.remove(fname).is_some() {
                        hooked_total += 1;
                        tracing::trace!(
                            target: "odoo_kernel::registry",
                            model = %name, field = %fname,
                            "dropped a field Python hooks; naming it will refuse"
                        );
                    }
                }
            }
            models.insert(
                name.clone(),
                Model {
                    name: name.clone(),
                    table,
                    order: em["order"].as_str().unwrap_or("id").to_string(),
                    fields,
                    rec_name: em["rec_name"].as_str().map(str::to_string),
                    parent_name: em["parent_name"].as_str().map(str::to_string),
                    parent_store: em["parent_store"].as_bool().unwrap_or(false),
                    active_name: em["active_name"].as_str().map(str::to_string),
                    display_name_column: em["display_name_column"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    display_name_guard: em["display_name_guard"].as_str().map(str::to_string),

                    name_search_fields: em["name_search_fields"].as_array().map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    }),
                    display_name_search_exact: em["display_name_search_exact"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    read_path_pure: em["read_path_pure"].as_bool().unwrap_or(false),
                    impure_read_methods: em["impure_read_methods"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    search_pure: em["search_pure"].as_bool().unwrap_or(false),
                    display_name_default: em["display_name_default"].as_bool().unwrap_or(false),
                    order_pure: em["order_pure"].as_bool().unwrap_or(false),
                    read_group_pure: em["read_group_pure"].as_bool().unwrap_or(false),
                    display_name_access_pure: em["display_name_access_pure"]
                        .as_bool()
                        .unwrap_or(false),
                    check_access_pure: em["check_access_pure"].as_bool().unwrap_or(false),
                },
            );
        }
        tracing::debug!(
            target: "odoo_kernel::registry",
            models = models.len(),
            exported = export_models.len(),
            skipped_tableless,
            hooked_fields_dropped = hooked_total,
            fields = models.values().map(|m| m.fields.len()).sum::<usize>(),
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "decoded the export"
        );
        Self::check_export_covers_db(client, &models).await?;
        let mut registry = Self::finalize(client, models, Source::Export).await?;
        {
            let dynamic = registry.dynamic();
            let current = registry
                .signal_tables
                .iter()
                .position(|name| name == "orm_signaling_registry")
                .and_then(|i| dynamic.signals.get(i).copied().flatten());
            if current != Some(expected_sequence) {
                tracing::warn!(
                    target: "odoo_kernel::signal",
                    expected_sequence,
                    ?current,
                    "the export was taken at a different orm_signaling_registry \
                     watermark than this database is at; refusing it as stale"
                );
                return Err(crate::error::RegistryStale.into());
            }
        }
        registry.timezone_aliases = export["timezone_aliases"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| v.as_str().map(|c| (k.clone(), c.to_string())))
            .collect();
        tracing::info!(
            target: "odoo_kernel::registry",
            models = registry.models.len(),
            unaccent = registry.has_unaccent,
            trigram = registry.has_trigram,
            timezones = registry.timezones.len(),
            timezone_aliases = registry.timezone_aliases.len(),
            total_ms = t0.elapsed().as_secs_f64() * 1000.0,
            "registry ready"
        );
        Ok(registry)
    }

    async fn check_export_covers_db(
        client: &Client,
        models: &HashMap<String, Model>,
    ) -> Result<()> {
        let rows = client
            .query(
                "SELECT m.model FROM ir_model m
                   JOIN information_schema.tables t
                     ON t.table_schema = current_schema
                    AND t.table_name = replace(m.model, '.', '_')
                  WHERE t.table_type = 'BASE TABLE'
                  ORDER BY m.model",
                &[],
            )
            .await?;
        let missing: Vec<String> = rows
            .iter()
            .map(|r| r.get::<_, String>(0))
            .filter(|m| !models.contains_key(m))
            .collect();
        tracing::debug!(
            target: "odoo_kernel::registry",
            table_backed = rows.len(),
            missing = missing.len(),
            "checked that the export covers every table-backed model in this database"
        );
        if missing.is_empty() {
            return Ok(());
        }
        refuse!(
            "the registry export is incomplete for this database: {} of {} \
             table-backed models are missing from it (e.g. {}). Re-run \
             export_registry against this database.",
            missing.len(),
            rows.len(),
            missing
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    pub async fn finalize(
        client: &Client,
        mut models: HashMap<String, Model>,
        source: Source,
    ) -> Result<Registry> {
        let t_finalize = std::time::Instant::now();
        let text_columns: HashMap<String, std::collections::HashSet<String>> = models
            .iter()
            .map(|(name, m)| {
                let cols = m
                    .fields
                    .values()
                    .filter(|f| f.has_column && f.ttype.is_text() && !f.translated)
                    .map(|f| f.name.clone())
                    .collect();
                (name.clone(), cols)
            })
            .collect();
        let t_normalize = std::time::Instant::now();
        for model in models.values_mut() {
            Self::normalize_model(model, &text_columns);
        }
        tracing::debug!(
            target: "odoo_kernel::registry",
            models = models.len(),
            renders_display_name = models.values().filter(|m| m.display_name_default).count(),
            ms = t_normalize.elapsed().as_secs_f64() * 1000.0,
            "normalised _rec_name, _active_name and the display-name columns"
        );

        let langs = Self::load_langs(client).await?;

        let plan = Self::build_signals_plan(client).await?;
        let (signals_sql, signal_tables) = match plan {
            None => (None, Vec::new()),
            Some((sql, tables)) => (Some(sql), tables),
        };
        let signals = match &signals_sql {
            None => Vec::new(),
            Some(sql) => Self::signals_of(&client.query_one(sql.as_str(), &[]).await?),
        };
        tracing::debug!(
            target: "odoo_kernel::signal",
            tables = ?signal_tables,
            ?signals,
            "read the signalling watermark this registry is pinned to"
        );
        let has_unaccent = Self::load_has_unaccent(client).await?;
        let inherits = Self::load_inherits(client).await?;
        let dynamic = Dynamic {
            defaults: Self::load_defaults(client).await?,
            security: Self::load_security(client).await?,
            signals,
            langs: Vec::new(),
            week_start: Self::load_week_starts(client).await?,
        };
        let mut registry =
            Registry::with_signals_sql(models, langs, has_unaccent, signals_sql, dynamic, source);
        registry.inherits = inherits;
        registry.has_trigram = Self::load_has_trigram(client).await?;
        registry.signal_tables = signal_tables;
        registry.group_ids = Self::load_group_ids(client).await?;
        registry.timezones = client
            .query("SELECT name FROM pg_timezone_names", &[])
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        tracing::info!(
            target: "odoo_kernel::registry",
            source = ?source,
            models = registry.models.len(),
            unaccent = registry.has_unaccent,
            trigram = registry.has_trigram,
            signal_tables = registry.signal_tables.len(),
            ms = t_finalize.elapsed().as_secs_f64() * 1000.0,
            "finalised the registry"
        );
        Ok(registry)
    }

    fn renders_display_name(f: &Field) -> bool {
        (f.has_column || f.related.is_some())
            && (f.ttype.is_text() || f.ttype == FieldType::Selection)
    }

    pub fn normalize_model(
        model: &mut Model,
        text_columns: &HashMap<String, std::collections::HashSet<String>>,
    ) {
        if !model.display_name_column.is_empty() {
            let delegated = |f: &Field| {
                let Some((head, tail)) = f.related.as_deref().and_then(|r| r.split_once('.'))
                else {
                    return false;
                };
                let Some(hf) = model.fields.get(head) else {
                    return false;
                };
                hf.ttype == FieldType::Many2one
                    && hf.has_column
                    && hf
                        .relation
                        .as_deref()
                        .and_then(|co| text_columns.get(co))
                        .is_some_and(|cols| !tail.contains('.') && cols.contains(tail))
            };
            let renderable = model.display_name_column.iter().all(|column| {
                model.fields.get(column).is_some_and(|f| {
                    (f.has_column && f.ttype.is_text() && !f.translated) || delegated(f)
                })
            });
            if renderable {
                model.display_name_default = true;
            } else {
                tracing::debug!(
                    target: "odoo_kernel::registry",
                    model = %model.name,
                    columns = ?model.display_name_column,
                    "_display_name_column is not renderable from the columns; dropping it"
                );
                model.display_name_column.clear();
            }
        }
        let is_bool_column = |n: &str| {
            model
                .fields
                .get(n)
                .is_some_and(|f| f.ttype == FieldType::Boolean && f.has_column)
        };
        match model.active_name.take() {
            Some(declared) if is_bool_column(&declared) => model.active_name = Some(declared),
            Some(declared) => {
                tracing::debug!(
                    target: "odoo_kernel::registry",
                    model = %model.name,
                    declared = %declared,
                    "_active_name is not a stored boolean column; refusing _search on this model"
                );
                model.active_name = None;
                model.read_path_pure = false;
                model.impure_read_methods.push("_search".to_string());
            }
            None => {
                model.active_name = ["active", "x_active"]
                    .iter()
                    .find(|n| is_bool_column(n))
                    .map(|n| n.to_string());
            }
        }

        let declared = model.rec_name.clone();
        if declared.is_none() {
            model.rec_name = ["name", "x_name"]
                .iter()
                .find(|n| {
                    model
                        .fields
                        .get(**n)
                        .is_some_and(Self::renders_display_name)
                })
                .map(|n| n.to_string());
        }
        if let Some(rn) = &model.rec_name
            && !model.fields.get(rn).is_some_and(Self::renders_display_name)
        {
            tracing::debug!(
                target: "odoo_kernel::registry",
                model = %model.name,
                rec_name = %rn,
                was_declared = declared.is_some(),
                "_rec_name does not render from a column; display_name will refuse"
            );
            model.rec_name = None;
            if declared.is_some() {
                model.display_name_default = false;
            }
        }
    }

    pub async fn load_defaults(
        client: &Client,
    ) -> Result<HashMap<String, HashMap<String, CompanyDefault>>> {
        let t0 = std::time::Instant::now();
        let mut defaults: HashMap<String, HashMap<String, CompanyDefault>> = HashMap::new();
        for row in client
            .query(
                "SELECT f.model, f.name, d.json_value, d.company_id
                 FROM ir_default d JOIN ir_model_fields f ON d.field_id = f.id
                 WHERE (d.user_id IS NULL OR d.user_id = 1) AND d.condition IS NULL
                 ORDER BY (d.user_id IS NOT NULL) DESC, (d.company_id IS NOT NULL) DESC, d.id",
                &[],
            )
            .await?
        {
            let (model, fname): (String, String) = (row.get(0), row.get(1));

            let raw: &str = row.get(2);
            let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
                refusal!(
                    "ir.default for {}.{} is not JSON ({e}): {raw:?}",
                    row.get::<_, &str>(0),
                    row.get::<_, &str>(1)
                )
            })?;
            let company: Option<i32> = row.get(3);
            let entry = defaults.entry(model).or_default().entry(fname).or_default();
            entry.ordered.push((company, value));
        }
        tracing::debug!(
            target: "odoo_kernel::registry",
            models = defaults.len(),
            fields = defaults.values().map(HashMap::len).sum::<usize>(),
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "loaded ir.default; these are the fallbacks a company-dependent column coalesces to"
        );
        Ok(defaults)
    }

    pub async fn load_security(client: &Client) -> Result<Security> {
        let t0 = std::time::Instant::now();
        let mut security = Security::default();
        for row in client
            .query("SELECT gid, hid FROM res_groups_implied_rel", &[])
            .await?
        {
            security
                .implied
                .entry(row.get(0))
                .or_default()
                .push(row.get(1));
        }
        for row in client
            .query("SELECT uid, gid FROM res_groups_users_rel", &[])
            .await?
        {
            security
                .user_groups
                .entry(row.get(0))
                .or_default()
                .push(row.get(1));
        }
        for row in client
            .query(
                "SELECT m.model, a.group_id FROM ir_model_access a
                 JOIN ir_model m ON a.model_id = m.id
                 WHERE a.active AND a.perm_read",
                &[],
            )
            .await?
        {
            security
                .access
                .entry(row.get(0))
                .or_default()
                .push(row.get(1));
        }
        let mut rule_groups: HashMap<i32, Vec<i32>> = HashMap::new();
        for row in client
            .query("SELECT rule_group_id, group_id FROM rule_group_rel", &[])
            .await?
        {
            rule_groups.entry(row.get(0)).or_default().push(row.get(1));
        }
        let has_composition = client
            .query_opt(
                "SELECT 1 FROM pg_attribute
                 WHERE attrelid = 'ir_rule'::regclass
                   AND attname = 'composition' AND NOT attisdropped",
                &[],
            )
            .await?
            .is_some();
        let rules_sql = if has_composition {
            "SELECT r.id, m.model, r.domain_force, \
                    COALESCE(r.composition = 'restrict', FALSE) \
             FROM ir_rule r JOIN ir_model m ON r.model_id = m.id \
             WHERE r.active AND r.perm_read ORDER BY r.id"
        } else {
            "SELECT r.id, m.model, r.domain_force, FALSE \
             FROM ir_rule r JOIN ir_model m ON r.model_id = m.id \
             WHERE r.active AND r.perm_read ORDER BY r.id"
        };
        for row in client.query(rules_sql, &[]).await? {
            let rid: i32 = row.get(0);
            let model: String = row.get(1);
            let domain_force = row
                .get::<_, Option<String>>(2)
                .filter(|d| !d.trim().is_empty());
            let parsed = domain_force.as_deref().map(|src| {
                crate::security::parse_py(src).inspect(|_| {
                    tracing::trace!(
                        target: "odoo_kernel::rules",
                        rule = rid, model = %model, groups = rule_groups.get(&rid).map_or(0, Vec::len),
                        "parsed a record rule's domain_force"
                    );
                }).map_err(|e| {
                    tracing::warn!(
                        target: "odoo_kernel::rules",
                        rule = rid, model = %model, reason = %format!("{e:#}"),
                        "record rule domain cannot be parsed; its model is refused"
                    );
                    format!("{e:#}")
                })
            });
            security.rules.entry(model).or_default().push(Rule {
                groups: rule_groups.get(&rid).cloned().unwrap_or_default(),
                restrict: row.get(3),
                domain_force,
                parsed,
            });
        }

        for row in client
            .query(
                "SELECT r.user_id, r.cid FROM res_company_users_rel r
                 JOIN res_company c ON c.id = r.cid AND c.active
                 ORDER BY r.cid",
                &[],
            )
            .await?
        {
            security
                .user_companies
                .entry(row.get(0))
                .or_default()
                .push(row.get(1));
        }
        let unparsable = security
            .rules
            .values()
            .flatten()
            .filter(|r| matches!(r.parsed, Some(Err(_))))
            .count();
        tracing::debug!(
            target: "odoo_kernel::rules",
            ruled_models = security.rules.len(),
            rules = security.rules.values().map(Vec::len).sum::<usize>(),
            unparsable,
            access_models = security.access.len(),
            group_implications = security.implied.len(),
            users_with_groups = security.user_groups.len(),
            users_with_companies = security.user_companies.len(),
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "loaded ir.rule, ir.model.access and the group/company memberships"
        );
        Ok(security)
    }
}
