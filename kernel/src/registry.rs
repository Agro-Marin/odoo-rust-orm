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

    /// The `falsy_value` a field of this type USUALLY has.
    ///
    /// Odoo declares `falsy_value` on the field CLASS, so this is a
    /// derivation and not a reading: it is what the `ir_model` bootstrap has
    /// to fall back on, having no access to the classes. Two classes
    /// disagree with the type they report -- `id` is a `fields.Id` and not
    /// an `Integer`, so it has no `0`, and `Many2oneReference` has `0` where
    /// its relational siblings have none -- and `Field::falsy_json` prefers
    /// the exported value precisely so a third one does not go unnoticed.
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

    /// Whether a subquery THROUGH this field runs with the comodel's ACL and
    /// record rules turned off, as Odoo's `_optimize_any_with_rights` and
    /// `_search(bypass_access=True)` do.
    ///
    /// `None` means the registry could not see it: `ir_model_fields` does not
    /// record it, so a bootstrap registry has to say so rather than guess,
    /// and the compiler refuses a traversal it cannot decide instead of
    /// answering with rules Odoo would have skipped.
    pub bypass_search_access: Option<bool>,
}

impl Field {
    /// What Odoo stores in this column for an unset value, if anything, and
    /// therefore the whole of its answer to a comparison against `False`.
    pub fn falsy_json(&self) -> Option<serde_json::Value> {
        self.falsy.clone()
    }

    pub fn comodel(&self) -> Result<&str> {
        self.relation
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("field {} has no comodel", self.name))
    }

    pub fn inverse_column(&self) -> Result<&str> {
        self.o2m_inverse()
    }

    pub fn m2m_columns(&self) -> Result<(&str, &str, &str)> {
        match (
            self.relation_table.as_deref(),
            self.column1.as_deref(),
            self.column2.as_deref(),
        ) {
            (Some(t), Some(c1), Some(c2)) => Ok((t, c1, c2)),

            _ if !self.stored => anyhow::bail!(
                "many2many {} is computed in Python{}, so it has no relation \
                 table to read",
                self.name,
                match &self.related {
                    Some(r) => format!(" (related: {r})"),
                    None => String::new(),
                }
            ),
            _ => anyhow::bail!(
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
            .ok_or_else(|| anyhow::anyhow!("field {} has a non-object context", self.name))?;
        let mut out = None;
        for (key, value) in obj {
            match key.as_str() {
                "active_test" => {
                    out = Some(value.as_bool().ok_or_else(|| {
                        anyhow::anyhow!("field {} has a non-boolean active_test", self.name)
                    })?)
                }
                other => anyhow::bail!(
                    "field {} carries the context key {other:?}, which this kernel does \
                     not model; refusing rather than reading it as absent",
                    self.name
                ),
            }
        }
        Ok(out)
    }

    /// The comodel COLUMN a one2many joins on.
    ///
    /// `store` on a one2many says its inverse is a column, and Odoo does not
    /// require the inverse itself to be stored:
    /// `account.analytic.account.line_ids` inverts
    /// `account.analytic.line.auto_account_id`, a non-stored many2one with a
    /// `search=` method, and no such column exists. Asking the one2many's own
    /// flag emits SQL PostgreSQL rejects, so the COMODEL's field is what has
    /// to be asked -- and both the filter path and the read path have to ask,
    /// which is why this is one function and not two checks.
    pub fn o2m_inverse_column(&self, owner: &str, co: &Model) -> Result<&str> {
        let inverse = self.o2m_inverse()?;
        match co.fields.get(inverse) {
            Some(f) if f.has_column => Ok(inverse),
            Some(_) => anyhow::bail!(
                "one2many {}.{} inverts {}.{}, which is computed in Python and \
                 has no column to join on",
                owner,
                self.name,
                co.name,
                inverse
            ),
            None => anyhow::bail!(
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
        match self.relation_field.as_deref() {
            Some(inv) => Ok(inv),
            None if !self.stored => anyhow::bail!(
                "one2many {} is computed in Python{}, so it has no inverse \
                 column to read",
                self.name,
                match &self.related {
                    Some(r) => format!(" (related: {r})"),
                    None => String::new(),
                }
            ),
            None => anyhow::bail!("one2many {} has no inverse field", self.name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Model {
    pub name: String,
    pub table: String,

    pub order: String,
    pub fields: HashMap<String, Field>,
    pub has_active: bool,

    pub rec_name: Option<String>,

    pub parent_name: Option<String>,

    pub read_path_pure: bool,

    pub search_pure: bool,

    pub name_search_fields: Option<Vec<String>>,

    pub display_name_default: bool,

    pub active_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub groups: Vec<i32>,
    pub domain_force: Option<String>,
}

#[derive(Debug, Default)]
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
}

#[derive(Debug)]
pub struct Dynamic {
    pub security: Security,

    pub defaults: HashMap<String, HashMap<String, CompanyDefault>>,

    pub signals: Signals,
}

#[derive(Debug)]
pub struct Registry {
    pub models: HashMap<String, Model>,

    pub langs: Vec<String>,

    pub has_unaccent: bool,

    /// Whether `pg_trgm` is installed, which is what makes the GIN index on
    /// a translated `index="trigram"` field usable at all.
    pub has_trigram: bool,

    pub source: Source,

    signal_tables: Vec<String>,

    inherits: HashMap<String, Vec<(String, String)>>,

    group_ids: HashMap<String, i32>,

    signals_sql: Option<String>,
    dynamic: std::sync::RwLock<std::sync::Arc<Dynamic>>,
}

#[derive(Debug, Default, Clone)]
pub struct CompanyDefault {
    pub by_company: HashMap<i32, serde_json::Value>,
    pub global: Option<serde_json::Value>,
}

impl CompanyDefault {
    pub fn fallback(&self, company_id: i32) -> Option<&serde_json::Value> {
        self.by_company
            .get(&company_id)
            .or(self.global.as_ref())
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
        dynamic: Dynamic,
        source: Source,
    ) -> Self {
        Registry {
            models,
            langs,
            has_unaccent,
            has_trigram: false,
            source,
            signal_tables: Vec::new(),
            inherits: HashMap::new(),
            group_ids: HashMap::new(),
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
            self.group_ids
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
        let mut out = HashMap::new();
        for row in client
            .query(
                "SELECT module || '.' || name, res_id FROM ir_model_data
                  WHERE model = 'res.groups'",
                &[],
            )
            .await?
        {
            out.insert(row.get(0), row.get(1));
        }
        Ok(out)
    }

    pub fn inherits_of(&self, model: &str) -> &[(String, String)] {
        self.inherits.get(model).map_or(&[], Vec::as_slice)
    }

    pub fn set_inherits(&mut self, inherits: HashMap<String, Vec<(String, String)>>) {
        self.inherits = inherits;
    }

    pub async fn load_inherits(client: &Client) -> Result<HashMap<String, Vec<(String, String)>>> {
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
        Ok(out)
    }

    pub fn dynamic(&self) -> std::sync::Arc<Dynamic> {
        self.dynamic.read().unwrap().clone()
    }

    pub async fn refresh_dynamic(&self, client: &Client, signals: Signals) -> Result<()> {
        let fresh = Dynamic {
            security: Self::load_security(client).await?,
            defaults: Self::load_defaults(client).await?,
            signals,
        };
        *self.dynamic.write().unwrap() = std::sync::Arc::new(fresh);
        Ok(())
    }

    pub fn get(&self, model: &str) -> Result<&Model> {
        self.models
            .get(model)
            .ok_or_else(|| anyhow::anyhow!("unknown or table-less model {model}"))
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
        if old == new {
            return SignalChange::None;
        }
        let moved_registry = self
            .signal_tables
            .iter()
            .enumerate()
            .any(|(i, t)| t == "orm_signaling_registry" && old.get(i) != new.get(i));
        if moved_registry {
            SignalChange::Registry
        } else {
            SignalChange::Security
        }
    }

    pub fn signals_sql(&self) -> Option<&str> {
        self.signals_sql.as_deref()
    }

    pub fn signals_of(row: &tokio_postgres::Row) -> Signals {
        (0..row.len())
            .map(|i| row.get::<_, Option<i64>>(i))
            .collect()
    }

    pub async fn read_signals(client: &Client) -> Result<Signals> {
        match Self::build_signals_sql(client).await? {
            None => Ok(Vec::new()),
            Some(sql) => Ok(Self::signals_of(&client.query_one(&sql, &[]).await?)),
        }
    }

    pub async fn load_schema(
        client: &Client,
    ) -> Result<HashMap<String, HashMap<String, (String, bool)>>> {
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
        Ok(schema)
    }

    pub async fn load(client: &Client) -> Result<Registry> {
        let schema = Self::load_schema(client).await?;

        let mut models: HashMap<String, Model> = HashMap::new();
        for row in client
            .query("SELECT model, \"order\" FROM ir_model", &[])
            .await?
        {
            let name: String = row.get(0);
            let order: String = row.get(1);
            let table = name.replace('.', "_");

            if !schema.contains_key(&table) {
                continue;
            }
            models.insert(
                name.clone(),
                Model {
                    name,
                    table,
                    order,
                    fields: HashMap::new(),
                    has_active: false,
                    rec_name: None,
                    parent_name: None,
                    active_name: None,

                    read_path_pure: false,
                    search_pure: false,
                    display_name_default: false,
                    name_search_fields: None,
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
                },
            );
        }

        Self::finalize(client, models, Source::Bootstrap).await
    }

    pub async fn from_export(client: &Client, export: &serde_json::Value) -> Result<Registry> {
        let schema = Self::load_schema(client).await?;
        let mut models: HashMap<String, Model> = HashMap::new();
        let export_models = export["models"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("export missing models"))?;
        for (name, em) in export_models {
            let table = em["table"].as_str().unwrap_or_default().to_string();
            let Some(cols) = schema.get(&table) else {
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

                        // The exported value is the READING; the type rule
                        // is only what an export written before this key
                        // existed can fall back on. `null` is a real answer
                        // here ("no falsy value") and must not be confused
                        // with the key being absent, which is why this is a
                        // `get` and not an index.
                        falsy: match ef.get("falsy_value") {
                            None => ttype.falsy_json_for_type(fname),
                            Some(serde_json::Value::Null) => None,
                            Some(v) => Some(v.clone()),
                        },
                        // An export written before this key existed knows
                        // nothing about it either, so it decodes the same way
                        // a bootstrap registry does rather than as `false`.
                        bypass_search_access: ef
                            .get("bypass_search_access")
                            .and_then(serde_json::Value::as_bool),
                    },
                );
            }
            models.insert(
                name.clone(),
                Model {
                    name: name.clone(),
                    table,
                    order: em["order"].as_str().unwrap_or("id").to_string(),
                    fields,
                    has_active: false,
                    rec_name: em["rec_name"].as_str().map(str::to_string),
                    parent_name: em["parent_name"].as_str().map(str::to_string),
                    active_name: None,

                    name_search_fields: em["name_search_fields"].as_array().map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    }),
                    read_path_pure: em["read_path_pure"].as_bool().unwrap_or(false),
                    search_pure: em["search_pure"].as_bool().unwrap_or(false),
                    display_name_default: em["display_name_default"].as_bool().unwrap_or(false),
                },
            );
        }
        Self::check_export_covers_db(client, &models).await?;
        Self::finalize(client, models, Source::Export).await
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
        if missing.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
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
        for model in models.values_mut() {
            model.active_name = ["active", "x_active"]
                .iter()
                .find(|n| {
                    model
                        .fields
                        .get(**n)
                        .is_some_and(|f| f.ttype == FieldType::Boolean && f.has_column)
                })
                .map(|n| n.to_string());
            model.has_active = model.active_name.is_some();
            if model.rec_name.is_none() {
                model.rec_name = ["name", "x_name"]
                    .iter()
                    .find(|n| {
                        model.fields.get(**n).is_some_and(|f| {
                            (f.has_column || f.related.is_some()) && f.ttype.is_text()
                        })
                    })
                    .map(|n| n.to_string());
            }

            if let Some(rn) = &model.rec_name {
                let ok = model
                    .fields
                    .get(rn)
                    .is_some_and(|f| (f.has_column || f.related.is_some()) && f.ttype.is_text());
                if !ok {
                    model.rec_name = None;
                }
            }
        }

        let langs = client
            .query("SELECT code FROM res_lang WHERE active", &[])
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();

        let plan = Self::build_signals_plan(client).await?;
        let (signals_sql, signal_tables) = match plan {
            None => (None, Vec::new()),
            Some((sql, tables)) => (Some(sql), tables),
        };
        let signals = match &signals_sql {
            None => Vec::new(),
            Some(sql) => Self::signals_of(&client.query_one(sql.as_str(), &[]).await?),
        };
        let has_unaccent = Self::load_has_unaccent(client).await?;
        let inherits = Self::load_inherits(client).await?;
        let dynamic = Dynamic {
            defaults: Self::load_defaults(client).await?,
            security: Self::load_security(client).await?,
            signals,
        };
        let mut registry =
            Registry::with_signals_sql(models, langs, has_unaccent, signals_sql, dynamic, source);
        registry.inherits = inherits;
        registry.signal_tables = signal_tables;
        registry.group_ids = Self::load_group_ids(client).await?;
        registry.has_trigram = Self::load_has_trigram(client).await?;
        Ok(registry)
    }

    /// Asked of the OPERATOR CLASS rather than the extension name, because
    /// `gin_trgm_ops` is the thing the index is declared with -- an
    /// extension installed in another schema would answer the wrong question.
    pub async fn load_has_trigram(client: &Client) -> Result<bool> {
        let row = client
            .query_opt(
                "SELECT 1 FROM pg_opclass WHERE opcname = 'gin_trgm_ops'",
                &[],
            )
            .await?;
        Ok(row.is_some())
    }

    pub async fn load_defaults(
        client: &Client,
    ) -> Result<HashMap<String, HashMap<String, CompanyDefault>>> {
        let mut defaults: HashMap<String, HashMap<String, CompanyDefault>> = HashMap::new();
        for row in client
            .query(
                "SELECT f.model, f.name, d.json_value, d.company_id
                 FROM ir_default d JOIN ir_model_fields f ON d.field_id = f.id
                 WHERE d.user_id IS NULL AND d.condition IS NULL",
                &[],
            )
            .await?
        {
            let (model, fname): (String, String) = (row.get(0), row.get(1));

            let raw: &str = row.get(2);
            let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
                anyhow::anyhow!(
                    "ir.default for {}.{} is not JSON ({e}): {raw:?}",
                    row.get::<_, &str>(0),
                    row.get::<_, &str>(1)
                )
            })?;
            let company: Option<i32> = row.get(3);
            let entry = defaults.entry(model).or_default().entry(fname).or_default();
            match company {
                Some(c) => {
                    entry.by_company.insert(c, value);
                }
                None => entry.global = Some(value),
            }
        }
        Ok(defaults)
    }

    pub async fn load_security(client: &Client) -> Result<Security> {
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
        for row in client
            .query(
                "SELECT r.id, m.model, r.domain_force FROM ir_rule r
                 JOIN ir_model m ON r.model_id = m.id
                 WHERE r.active AND r.perm_read ORDER BY r.id",
                &[],
            )
            .await?
        {
            let rid: i32 = row.get(0);
            security.rules.entry(row.get(1)).or_default().push(Rule {
                groups: rule_groups.get(&rid).cloned().unwrap_or_default(),
                domain_force: row
                    .get::<_, Option<String>>(2)
                    .filter(|d| !d.trim().is_empty()),
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
        Ok(security)
    }
}
