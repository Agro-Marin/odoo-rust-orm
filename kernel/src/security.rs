use crate::error::{deny_access, refusal, refuse};
use anyhow::Result;
use serde_json::{Value as Json, json};

use crate::db::Db;
use crate::registry::{FieldType, Registry};

#[derive(Debug, Clone)]
pub enum PyExpr {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    None,
    Seq(Vec<PyExpr>),
    Name(Vec<String>),

    Add(Box<PyExpr>, Box<PyExpr>),
}

pub fn parse_py(src: &str) -> Result<PyExpr> {
    let chars: Vec<char> = src.chars().collect();
    let mut pos = 0usize;
    let expr = parse_expr(&chars, &mut pos)?;
    skip_ws(&chars, &mut pos);
    if pos != chars.len() {
        refuse!("trailing input in expression at {pos}: {src}");
    }
    Ok(expr)
}

fn skip_ws(c: &[char], p: &mut usize) {
    loop {
        while *p < c.len() && c[*p].is_whitespace() {
            *p += 1;
        }
        if c.get(*p) != Some(&'#') {
            return;
        }
        while *p < c.len() && c[*p] != '\n' {
            *p += 1;
        }
    }
}

fn parse_expr(c: &[char], p: &mut usize) -> Result<PyExpr> {
    let mut left = parse_atom(c, p)?;
    loop {
        skip_ws(c, p);
        if c.get(*p) != Some(&'+') {
            return Ok(left);
        }
        *p += 1;
        let right = parse_atom(c, p)?;
        left = PyExpr::Add(Box::new(left), Box::new(right));
    }
}

fn parse_atom(c: &[char], p: &mut usize) -> Result<PyExpr> {
    skip_ws(c, p);
    let Some(&ch) = c.get(*p) else {
        refuse!("unexpected end of expression");
    };
    match ch {
        '[' | '(' => {
            let close = if ch == '[' { ']' } else { ')' };
            *p += 1;
            let mut items = Vec::new();
            loop {
                skip_ws(c, p);
                if c.get(*p) == Some(&close) {
                    *p += 1;
                    break;
                }
                items.push(parse_expr(c, p)?);
                skip_ws(c, p);
                match c.get(*p) {
                    Some(',') => *p += 1,
                    Some(x) if *x == close => {}
                    other => refuse!("expected ',' or '{close}', got {other:?}"),
                }
            }
            Ok(PyExpr::Seq(items))
        }
        '\'' | '"' => {
            let quote = ch;
            *p += 1;
            let mut s = String::new();
            while let Some(&x) = c.get(*p) {
                *p += 1;
                if x == '\\' {
                    // the escapes safe_eval's literal parser gives; anything
                    // else is refused rather than read as the bare character
                    let Some(&y) = c.get(*p) else {
                        refuse!("unterminated escape in string");
                    };
                    *p += 1;
                    s.push(match y {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '\\' | '\'' | '"' => y,
                        other => refuse!("unsupported escape \\{other} in string"),
                    });
                } else if x == quote {
                    return Ok(PyExpr::Str(s));
                } else {
                    s.push(x);
                }
            }
            refuse!("unterminated string");
        }
        '-' | '0'..='9' => {
            let start = *p;
            if ch == '-' {
                *p += 1;
            }
            let mut is_float = false;
            while let Some(&x) = c.get(*p) {
                if x.is_ascii_digit() {
                    *p += 1;
                } else if x == '.' && !is_float {
                    is_float = true;
                    *p += 1;
                } else {
                    break;
                }
            }
            let text: String = c[start..*p].iter().collect();
            if is_float {
                Ok(PyExpr::Float(text.parse()?))
            } else {
                Ok(PyExpr::Int(text.parse()?))
            }
        }
        _ if ch.is_alphabetic() || ch == '_' => {
            let mut names = Vec::new();
            loop {
                let start = *p;
                while let Some(&x) = c.get(*p) {
                    if x.is_alphanumeric() || x == '_' {
                        *p += 1;
                    } else {
                        break;
                    }
                }
                if start == *p {
                    refuse!("expected identifier at {p:?}");
                }
                let seg: String = c[start..*p].iter().collect();

                if seg == "mapped" && c.get(*p) == Some(&'(') {
                    *p += 1;
                    let inner = parse_expr(c, p)?;
                    skip_ws(c, p);
                    if c.get(*p) != Some(&')') {
                        refuse!("expected ')' closing mapped(");
                    }
                    *p += 1;
                    match inner {
                        // a `*` marks the segments mapped() produced: their
                        // values are collected over every record, where a bare
                        // attribute on several records is a singleton error
                        PyExpr::Str(field) => {
                            names.extend(field.split('.').map(|f| format!("*{f}")))
                        }
                        other => refuse!("mapped() takes a field name, got {other:?}"),
                    }
                } else {
                    names.push(seg);
                }
                if c.get(*p) == Some(&'.') {
                    *p += 1;
                } else {
                    break;
                }
            }
            match names[0].as_str() {
                "True" | "False" | "None" if names.len() > 1 => {
                    refuse!("attribute access on the literal {}", names[0])
                }
                "True" => Ok(PyExpr::Bool(true)),
                "False" => Ok(PyExpr::Bool(false)),
                "None" => Ok(PyExpr::None),
                _ => Ok(PyExpr::Name(names)),
            }
        }
        other => refuse!("unexpected character {other:?} in expression"),
    }
}

pub struct UserCtx {
    pub uid: i32,
    pub company_id: i32,
    pub company_ids: Vec<i32>,

    pub groups: std::sync::Arc<std::collections::HashSet<i32>>,
}

#[derive(Debug, Default, Clone)]
pub struct RuleSet {
    domains: std::collections::HashMap<String, crate::domain::Node>,
    unevaluated: std::collections::HashMap<String, String>,

    ruled: std::collections::HashSet<String>,
    compiled: std::collections::HashSet<String>,
}

impl RuleSet {
    pub fn with_ruled(ruled: std::collections::HashSet<String>) -> Self {
        RuleSet {
            ruled,
            ..Default::default()
        }
    }

    pub fn insert(&mut self, model: String, node: crate::domain::Node) {
        self.compiled.insert(model.clone());
        self.domains.insert(model, node);
    }

    pub fn mark_unrestricted(&mut self, model: String) {
        self.compiled.insert(model);
    }

    pub fn mark_unevaluated(&mut self, model: String, reason: String) {
        self.compiled.insert(model.clone());
        self.unevaluated.insert(model, reason);
    }

    pub fn is_compiled(&self, model: &str) -> bool {
        self.compiled.contains(model)
    }

    pub fn compiled_models(&self) -> impl Iterator<Item = &String> {
        self.compiled.iter()
    }

    pub fn get(&self, model: &str) -> Option<&crate::domain::Node> {
        self.domains.get(model)
    }

    pub fn is_unevaluated(&self, model: &str) -> bool {
        self.unevaluated.contains_key(model)
    }

    pub fn ensure_evaluated(&self, model: &str) -> Result<()> {
        tracing::trace!(
            target: "odoo_kernel::rules",
            %model,
            restricted = self.domains.contains_key(model),
            compiled = self.compiled.contains(model),
            ruled = self.ruled.contains(model),
            "checking that this model's rules were compiled for the request"
        );
        if let Some(reason) = self.unevaluated.get(model) {
            refuse!(
                "record rules on {model} could not be evaluated ({reason}); \
                 refusing to read it without them"
            );
        }
        if self.ruled.contains(model) && !self.compiled.contains(model) {
            refuse!(
                "record rules on {model} were not compiled for this request -- the \
                 reachability walk did not reach it. Refusing rather than reading \
                 it as unrestricted."
            );
        }
        Ok(())
    }

    pub fn unevaluated_models(&self) -> impl Iterator<Item = (&String, &String)> {
        self.unevaluated.iter()
    }

    pub fn restricted_count(&self) -> usize {
        self.domains.len()
    }
}

pub async fn eval_py(
    expr: &PyExpr,
    registry: &Registry,
    db: &Db<'_>,
    user: &UserCtx,
) -> Result<Json> {
    Ok(match expr {
        PyExpr::Str(s) => json!(s),
        PyExpr::Int(i) => json!(i),
        PyExpr::Float(f) => json!(f),
        PyExpr::Bool(b) => json!(b),
        PyExpr::None => Json::Null,
        PyExpr::Seq(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(Box::pin(eval_py(item, registry, db, user)).await?);
            }
            Json::Array(out)
        }
        PyExpr::Name(chain) => resolve_name(chain, registry, db, user).await?,
        PyExpr::Add(a, b) => {
            let a = Box::pin(eval_py(a, registry, db, user)).await?;
            let b = Box::pin(eval_py(b, registry, db, user)).await?;
            add_values(a, b)?
        }
    })
}

fn add_values(a: Json, b: Json) -> Result<Json> {
    Ok(match (a, b) {
        (Json::Array(mut x), Json::Array(y)) => {
            x.extend(y);
            Json::Array(x)
        }
        (Json::String(x), Json::String(y)) => json!(format!("{x}{y}")),
        (Json::Number(x), Json::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => json!(i + j),
            _ => json!(x.as_f64().unwrap_or(0.0) + y.as_f64().unwrap_or(0.0)),
        },
        (x, y) => refuse!("unsupported operands for +: {x} and {y}"),
    })
}

async fn resolve_name(
    chain: &[String],
    registry: &Registry,
    db: &Db<'_>,
    user: &UserCtx,
) -> Result<Json> {
    // Every hop here is one query, run while a request waits: a rule domain
    // reading `user.partner_id.country_id.code` costs three. They are cached
    // per identity afterwards, so this line fires on a cold cache only.
    tracing::trace!(
        target: "odoo_kernel::rules",
        uid = user.uid, name = ?chain,
        "resolving a name chain in a rule domain"
    );
    let (mut model_name, mut current_ids, rest): (String, Vec<i32>, &[String]) =
        match chain[0].as_str() {
            "user" => ("res.users".into(), vec![user.uid], &chain[1..]),
            "company_id" => return Ok(json!(user.company_id)),
            "company_ids" => return Ok(json!(user.company_ids)),
            other => refuse!("unknown name {other:?} in rule expression"),
        };

    let mut mapped = false;
    let mut path: Vec<String> = rest
        .iter()
        .map(|seg| match seg.strip_prefix('*') {
            Some(bare) => {
                mapped = true;
                bare.to_string()
            }
            None => seg.clone(),
        })
        .collect();
    if model_name == "res.users" && path.first().is_some_and(|a| a == "env") {
        match path.get(1).map(String::as_str) {
            Some("companies") => {
                model_name = "res.company".into();
                current_ids = user.company_ids.clone();
                path.drain(..2);
            }
            Some("company") => {
                model_name = "res.company".into();
                current_ids = vec![user.company_id];
                path.drain(..2);
            }
            Some("user") => {
                path.drain(..2);
            }
            other => refuse!("unsupported user.env.{other:?} in a rule domain"),
        }
    }
    let mut i = 0usize;
    let mut guard = 0;
    let mut hops = 0usize;
    while i < path.len() {
        guard += 1;
        if guard > 32 {
            refuse!("related expansion loop in {chain:?}");
        }
        let attr = path[i].clone();
        if attr == "id" || attr == "ids" {
            if i != path.len() - 1 {
                refuse!("attribute after .id in {chain:?}");
            }
            break;
        }

        if model_name == "res.users" && (attr == "all_group_ids" || attr == "group_ids") {
            let tail = &path[i + 1..];
            if !matches!(tail, [] | [_] if tail.first().is_none_or(|t| t == "ids")) {
                refuse!("unsupported attribute path after {model_name}.{attr}: {tail:?}");
            }
            // a recordset attribute over several users is their union
            let dynamic = registry.dynamic();
            let mut groups: Vec<i32> = if attr == "all_group_ids" {
                let mut all: std::collections::HashSet<i32> = std::collections::HashSet::new();
                for uid in &current_ids {
                    if *uid == user.uid {
                        all.extend(user.groups.iter().copied());
                    } else {
                        all.extend(dynamic.security.groups_of(*uid));
                    }
                }
                all.into_iter().collect()
            } else {
                let mut all: std::collections::HashSet<i32> = std::collections::HashSet::new();
                for uid in &current_ids {
                    all.extend(
                        dynamic
                            .security
                            .user_groups
                            .get(uid)
                            .cloned()
                            .unwrap_or_default(),
                    );
                }
                all.into_iter().collect()
            };
            groups.sort_unstable();
            return Ok(json!(groups));
        }
        let model = registry.get(&model_name)?;
        let field = model
            .fields
            .get(&attr)
            .ok_or_else(|| refusal!("unknown attr {model_name}.{attr}"))?;
        if let Some(related) = &field.related {
            let expansion: Vec<String> = related.split('.').map(str::to_string).collect();
            tracing::trace!(
                target: "odoo_kernel::rules",
                model = %model_name, attr = %attr, %related,
                "expanded a related field in place; the path grows rather than hopping"
            );
            path.splice(i..=i, expansion);
            continue;
        }
        if field.company_dependent {
            refuse!(
                "cannot traverse {model_name}.{attr}: a company-dependent value is a \
                 jsonb the rule evaluator does not resolve"
            );
        }
        let active_clause = |co: &crate::registry::Model, alias: &str| -> Result<String> {
            Ok(
                match (
                    co.active_name.as_deref(),
                    field.context_active_test()?.unwrap_or(true),
                ) {
                    (Some(active), true) => {
                        format!(" AND {}.{} = TRUE", alias, crate::db::ident(active))
                    }
                    _ => String::new(),
                },
            )
        };
        match field.ttype {
            FieldType::One2many if field.stored => {
                let co = registry.get(field.comodel()?)?;
                let inverse = field.o2m_inverse_column(&model_name, co)?;
                let sql = format!(
                    "SELECT co.id FROM {} co WHERE co.{} = ANY($1){}",
                    crate::db::ident(&co.table),
                    crate::db::ident(inverse),
                    active_clause(co, "co")?
                );
                let rows = db.query(&sql, &[&current_ids]).await?;
                model_name = field.relation.clone().unwrap();
                current_ids = rows
                    .iter()
                    .map(|r| r.try_get::<_, i32>(0))
                    .collect::<std::result::Result<_, _>>()?;
                hops += 1;
                tracing::trace!(
                    target: "odoo_kernel::rules",
                    attr = %attr, kind = "one2many", to = %model_name, ids = current_ids.len(),
                    "hopped"
                );
                i += 1;
            }
            FieldType::Many2many if field.stored => {
                let (rel, c1, c2) = field.m2m_columns()?;
                let co = registry.get(field.comodel()?)?;
                let sql = format!(
                    "SELECT rel.{c2q} FROM {relq} rel JOIN {coq} co ON co.id = rel.{c2q} \
                     WHERE rel.{c1q} = ANY($1){active}",
                    c2q = crate::db::ident(c2),
                    relq = crate::db::ident(rel),
                    coq = crate::db::ident(&co.table),
                    c1q = crate::db::ident(c1),
                    active = active_clause(co, "co")?
                );
                let rows = db.query(&sql, &[&current_ids]).await?;
                model_name = field.relation.clone().unwrap();
                current_ids = rows
                    .iter()
                    .map(|r| r.try_get::<_, i32>(0))
                    .collect::<std::result::Result<_, _>>()?;
                hops += 1;
                tracing::trace!(
                    target: "odoo_kernel::rules",
                    attr = %attr, kind = "many2many", to = %model_name, ids = current_ids.len(),
                    "hopped"
                );
                i += 1;
            }
            _ if !field.has_column => {
                refuse!("cannot traverse {model_name}.{attr}: it is computed in Python");
            }
            FieldType::Many2one => {
                let sql = format!(
                    "SELECT {} FROM {} WHERE id = ANY($1)",
                    crate::db::ident(&field.name),
                    crate::db::ident(&model.table)
                );
                let rows = db.query(&sql, &[&current_ids]).await?;
                model_name = field.relation.clone().unwrap();
                current_ids = rows
                    .iter()
                    .map(|r| r.try_get::<_, Option<i32>>(0))
                    .filter_map(|r| r.transpose())
                    .collect::<std::result::Result<_, _>>()?;
                hops += 1;
                tracing::trace!(
                    target: "odoo_kernel::rules",
                    attr = %attr, kind = "many2one", to = %model_name, ids = current_ids.len(),
                    "hopped"
                );
                i += 1;
            }
            _ => {
                if i != path.len() - 1 {
                    refuse!("scalar {model_name}.{attr} mid-path in {chain:?}");
                }

                if current_ids.is_empty() {
                    return Ok(json!(false));
                }

                if current_ids.len() > 1 && !mapped {
                    refuse!(
                        "{model_name}.{attr} read on {} records: Python raises \
                         'Expected singleton'; use mapped() for a list",
                        current_ids.len()
                    );
                }
                if current_ids.len() > 1 {
                    let sql = format!(
                        "SELECT {}::text FROM {} WHERE id = ANY($1) ORDER BY id",
                        crate::db::ident(&field.name),
                        crate::db::ident(&model.table)
                    );
                    let rows = db.query(&sql, &[&current_ids]).await?;
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        out.push(match row.try_get::<_, Option<String>>(0)? {
                            Some(s) => scalar_json(field, &s)?,
                            None => json!(false),
                        });
                    }
                    return Ok(Json::Array(out));
                }
                let id = current_ids[0];
                let sql = format!(
                    "SELECT {}::text FROM {} WHERE id = $1",
                    crate::db::ident(&field.name),
                    crate::db::ident(&model.table)
                );
                let v: Option<String> = match db.query_opt(&sql, &[&id]).await? {
                    Some(r) => r.try_get(0)?,
                    None => None,
                };
                return Ok(match v {
                    Some(s) => scalar_json(field, &s)?,
                    None => json!(false),
                });
            }
        }
    }

    tracing::trace!(
        target: "odoo_kernel::rules",
        uid = user.uid, name = ?chain, hops, resolved = current_ids.len(),
        "name chain resolved to a recordset"
    );
    match path.last().map(String::as_str) {
        Some("ids") => Ok(json!(current_ids)),
        _ => match current_ids[..] {
            [] => Ok(json!(false)),
            [one] => Ok(json!(one)),

            _ => Ok(json!(current_ids)),
        },
    }
}

fn scalar_json(field: &crate::registry::Field, s: &str) -> Result<Json> {
    Ok(match field.ttype {
        FieldType::Integer => json!(s.parse::<i64>()?),
        FieldType::Boolean => json!(s == "true" || s == "t"),
        FieldType::Float | FieldType::Monetary => json!(s.parse::<f64>()?),
        _ => json!(s),
    })
}

// `Domain.OR` over single leaves: `['|'] * (n-1) + leaves`, FALSE for none
pub fn or_leaves(leaves: Vec<Json>) -> Vec<Json> {
    if leaves.is_empty() {
        return vec![json!([0, "=", 1])];
    }
    let mut out: Vec<Json> = Vec::with_capacity(leaves.len() * 2);
    for _ in 1..leaves.len() {
        out.push(json!("|"));
    }
    out.extend(leaves);
    out
}

fn or_domains(domains: Vec<Json>) -> Json {
    let mut terms: Vec<Vec<Json>> = Vec::with_capacity(domains.len());
    for d in domains {
        let items = d.as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            return Json::Array(Vec::new());
        }
        terms.push(items);
    }
    if terms.is_empty() {
        return json!([[0, "=", 1]]);
    }
    let mut out: Vec<Json> = Vec::new();
    for _ in 1..terms.len() {
        out.push(json!("|"));
    }
    for t in terms {
        out.extend(normalize_domain(t));
    }
    Json::Array(out)
}

fn normalize_domain(items: Vec<Json>) -> Vec<Json> {
    let mut out: Vec<Json> = Vec::with_capacity(items.len());
    let mut expected = 1i32;
    for item in items {
        if expected == 0 {
            out.insert(0, json!("&"));
            expected = 1;
        }
        match item.as_str() {
            Some("&") | Some("|") => expected += 1,
            Some("!") => {}
            _ => expected -= 1,
        }
        out.push(item);
    }
    out
}

pub async fn rules_domain(
    registry: &Registry,
    db: &Db<'_>,
    model: &str,
    user: &UserCtx,
) -> Result<Option<Json>> {
    Box::pin(rules_domain_inner(registry, db, model, user, 0)).await
}

async fn rules_domain_inner(
    registry: &Registry,
    db: &Db<'_>,
    model: &str,
    user: &UserCtx,
    depth: usize,
) -> Result<Option<Json>> {
    if depth > 16 {
        refuse!("_inherits chain too deep at {model}");
    }
    let dynamic = registry.dynamic();

    let mut inherited: Vec<Json> = Vec::new();
    for (parent, via) in registry.inherits_of(model) {
        let field = registry
            .get(model)
            .ok()
            .and_then(|m| m.fields.get(via.as_str()));
        match field {
            Some(f) if f.has_column => {}
            Some(_) => continue,
            None => refuse!("_inherits field {model}.{via} is not in the registry"),
        }
        if let Some(dom) =
            Box::pin(rules_domain_inner(registry, db, parent, user, depth + 1)).await?
        {
            inherited.push(json!([via, "any", dom]));
        }
    }

    let Some(rules) = dynamic.security.rules.get(model) else {
        return Ok(if inherited.is_empty() {
            None
        } else {
            Some(Json::Array(inherited))
        });
    };
    let user_groups = &user.groups;
    let mut global_domains: Vec<Json> = Vec::new();
    let mut group_domains: Vec<Json> = Vec::new();
    let mut any_applied = false;
    let mut skipped = 0usize;
    for rule in rules {
        let is_group = !rule.groups.is_empty();
        if is_group && !rule.groups.iter().any(|g| user_groups.contains(g)) {
            skipped += 1;
            continue;
        }
        any_applied = true;
        let dom = match &rule.parsed {
            Some(Ok(parsed)) => eval_py(parsed, registry, db, user).await?,
            Some(Err(why)) => refuse!("rule on {model} does not parse: {why}"),
            None => json!([]),
        };
        if is_group && !rule.restrict {
            group_domains.push(dom);
        } else {
            global_domains.push(dom);
        }
    }
    if !any_applied {
        // every rule on the model is a group rule this identity is outside
        // of, so the model reads unrestricted -- which is Odoo's own answer
        tracing::trace!(
            target: "odoo_kernel::rules",
            %model, uid = user.uid, rules = rules.len(),
            "no rule applies to this identity's groups"
        );
        return Ok(if inherited.is_empty() {
            None
        } else {
            Some(Json::Array(inherited))
        });
    }
    // global rules AND together and group rules OR: the composition is what
    // decides whether a second group widens the answer or narrows it
    tracing::debug!(
        target: "odoo_kernel::rules",
        %model,
        uid = user.uid,
        depth,
        global = global_domains.len(),
        group = group_domains.len(),
        skipped,
        inherited = inherited.len(),
        "combining the record rules that apply"
    );

    let mut combined: Vec<Json> = inherited;
    for d in global_domains {
        combined.extend(d.as_array().cloned().unwrap_or_default());
    }
    if !group_domains.is_empty() {
        combined.extend(
            or_domains(group_domains)
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
    Ok(Some(Json::Array(combined)))
}

pub fn check_read_access(
    dynamic: &crate::registry::Dynamic,
    model: &str,
    uid: i32,
    groups: &std::collections::HashSet<i32>,
) -> Result<()> {
    let Some(rows) = dynamic.security.access.get(model) else {
        deny_access!("access denied: no ir.model.access read entry for {model}");
    };
    if rows
        .iter()
        .any(|g| g.is_none_or(|gid| groups.contains(&gid)))
    {
        tracing::trace!(
            target: "odoo_kernel::access",
            %model, uid, entries = rows.len(), "ir.model.access grants read"
        );
        Ok(())
    } else {
        deny_access!("access denied on {model} for uid {uid}")
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn dom(v: &str) -> Json {
        serde_json::from_str(v).unwrap()
    }

    #[test]
    fn mapped_is_rewritten_into_the_attribute_chain() {
        match parse_py("user.env.companies.mapped('country_code')").unwrap() {
            PyExpr::Name(chain) => {
                assert_eq!(chain, ["user", "env", "companies", "*country_code"]);
            }
            other => panic!("expected a name chain, got {other:?}"),
        }
    }

    #[test]
    fn mapped_of_a_dotted_field_expands_to_several_segments() {
        match parse_py("user.env.companies.mapped('country_id.code').ids").unwrap() {
            PyExpr::Name(chain) => {
                assert_eq!(
                    chain,
                    ["user", "env", "companies", "*country_id", "*code", "ids"]
                );
            }
            other => panic!("expected a name chain, got {other:?}"),
        }
    }

    #[test]
    fn string_escapes_follow_python_and_unknown_ones_are_refused() {
        match parse_py("'a\\nb'").unwrap() {
            PyExpr::Str(v) => assert_eq!(v, "a\nb"),
            other => panic!("{other:?}"),
        }
        match parse_py("'it\\'s'").unwrap() {
            PyExpr::Str(v) => assert_eq!(v, "it's"),
            other => panic!("{other:?}"),
        }
        assert!(parse_py("'\\x41'").is_err());
        assert!(parse_py("None.foo").is_err());
        assert!(parse_py("True.x").is_err());
    }

    #[test]
    fn mapped_of_a_non_literal_is_refused_not_guessed() {
        assert!(parse_py("user.env.companies.mapped(lambda c: c.id)").is_err());
    }

    #[test]
    fn a_comment_in_a_domain_is_skipped() {
        let e = parse_py("[  # who can see it\n ('a', '=', 1)  # by id\n ]").unwrap();
        match e {
            PyExpr::Seq(items) => assert_eq!(items.len(), 1),
            other => panic!("expected a sequence, got {other:?}"),
        }
    }

    #[test]
    fn a_comment_does_not_swallow_the_rest_of_the_expression() {
        let e = parse_py("[1,  # one\n 2]").unwrap();
        match e {
            PyExpr::Seq(items) => assert_eq!(items.len(), 2),
            other => panic!("expected 2 items, got {other:?}"),
        }
    }

    #[test]
    fn or_of_two_domains_emits_one_operator() {
        let out = or_domains(vec![dom(r#"[["a","=",1]]"#), dom(r#"[["b","=",2]]"#)]);
        assert_eq!(out, dom(r#"["|",["a","=",1],["b","=",2]]"#));
    }

    #[test]
    fn an_empty_member_makes_the_whole_disjunction_true() {
        let out = or_domains(vec![dom("[]"), dom(r#"[["b","=",2]]"#)]);
        assert_eq!(out, dom("[]"));
        assert!(crate::domain::parse(&out).is_ok());
    }

    #[test]
    fn a_member_with_several_terms_stays_one_conjunction_under_or() {
        let out = or_domains(vec![
            dom(r#"[["a","=",1],["b","=",2]]"#),
            dom(r#"[["c","=",3]]"#),
        ]);
        assert_eq!(out, dom(r#"["|","&",["a","=",1],["b","=",2],["c","=",3]]"#));
        let node = crate::domain::parse(&out).unwrap();
        let crate::domain::Node::Or(members) = node else {
            panic!("expected an OR at the top, got {node:?}");
        };
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn normalizing_leaves_an_explicit_prefix_domain_alone() {
        let items = dom(r#"["|",["a","=",1],"!",["b","=",2]]"#);
        let items = items.as_array().cloned().unwrap();
        assert_eq!(normalize_domain(items.clone()), items);
    }

    #[test]
    fn a_single_member_needs_no_operator() {
        let out = or_domains(vec![dom(r#"[["a","=",1]]"#)]);
        assert_eq!(out, dom(r#"[["a","=",1]]"#));
    }

    #[test]
    fn three_members_emit_two_operators_and_still_parse() {
        let out = or_domains(vec![
            dom(r#"[["a","=",1]]"#),
            dom(r#"[["b","=",2]]"#),
            dom(r#"[["c","=",3]]"#),
        ]);
        assert_eq!(out.as_array().unwrap()[..2], [json!("|"), json!("|")]);
        assert!(crate::domain::parse(&out).is_ok());
    }

    #[test]
    fn or_of_nothing_is_false_not_everything() {
        let out = or_domains(vec![]);
        assert!(matches!(
            crate::domain::parse(&out).unwrap(),
            crate::domain::Node::False
        ));
    }
}
