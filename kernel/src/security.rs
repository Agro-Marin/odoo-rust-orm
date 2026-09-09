use anyhow::{Result, bail};
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
        bail!("trailing input in expression at {pos}: {src}");
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
        bail!("unexpected end of expression");
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
                    other => bail!("expected ',' or '{close}', got {other:?}"),
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
                    if let Some(&y) = c.get(*p) {
                        s.push(y);
                        *p += 1;
                    }
                } else if x == quote {
                    return Ok(PyExpr::Str(s));
                } else {
                    s.push(x);
                }
            }
            bail!("unterminated string");
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
                    bail!("expected identifier at {p:?}");
                }
                let seg: String = c[start..*p].iter().collect();

                if seg == "mapped" && c.get(*p) == Some(&'(') {
                    *p += 1;
                    let inner = parse_expr(c, p)?;
                    skip_ws(c, p);
                    if c.get(*p) != Some(&')') {
                        bail!("expected ')' closing mapped(");
                    }
                    *p += 1;
                    match inner {
                        PyExpr::Str(field) => names.extend(field.split('.').map(str::to_string)),
                        other => bail!("mapped() takes a field name, got {other:?}"),
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
                "True" => Ok(PyExpr::Bool(true)),
                "False" => Ok(PyExpr::Bool(false)),
                "None" => Ok(PyExpr::None),
                _ => Ok(PyExpr::Name(names)),
            }
        }
        other => bail!("unexpected character {other:?} in expression"),
    }
}

pub struct UserCtx {
    pub uid: i32,
    pub company_id: i32,
    pub company_ids: Vec<i32>,

    pub groups: std::sync::Arc<std::collections::HashSet<i32>>,
}

#[derive(Debug, Default)]
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

    pub fn is_ruled(&self, model: &str) -> bool {
        self.ruled.contains(model)
    }

    pub fn get(&self, model: &str) -> Option<&crate::domain::Node> {
        self.domains.get(model)
    }

    pub fn is_unevaluated(&self, model: &str) -> bool {
        self.unevaluated.contains_key(model)
    }

    pub fn ensure_evaluated(&self, model: &str) -> Result<()> {
        if let Some(reason) = self.unevaluated.get(model) {
            bail!(
                "record rules on {model} could not be evaluated ({reason}); \
                 refusing to read it without them"
            );
        }
        if self.ruled.contains(model) && !self.compiled.contains(model) {
            bail!(
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
        (x, y) => bail!("unsupported operands for +: {x} and {y}"),
    })
}

async fn resolve_name(
    chain: &[String],
    registry: &Registry,
    db: &Db<'_>,
    user: &UserCtx,
) -> Result<Json> {
    let (mut model_name, mut current_ids, rest): (String, Vec<i32>, &[String]) =
        match chain[0].as_str() {
            "user" => ("res.users".into(), vec![user.uid], &chain[1..]),
            "uid" => return Ok(json!(user.uid)),
            "company_id" => return Ok(json!(user.company_id)),
            "company_ids" | "companies" => return Ok(json!(user.company_ids)),
            other => bail!("unknown name {other:?} in rule expression"),
        };

    let mut path: Vec<String> = rest.to_vec();
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
            other => bail!("unsupported user.env.{other:?} in a rule domain"),
        }
    }
    let mut i = 0usize;
    let mut guard = 0;
    while i < path.len() {
        guard += 1;
        if guard > 32 {
            bail!("related expansion loop in {chain:?}");
        }
        let attr = path[i].clone();
        if attr == "id" || attr == "ids" {
            if i != path.len() - 1 {
                bail!("attribute after .id in {chain:?}");
            }
            break;
        }

        if model_name == "res.users" && (attr == "all_group_ids" || attr == "group_ids") {
            let tail = &path[i + 1..];
            if !matches!(tail, [] | [_] if tail.first().is_none_or(|t| t == "ids")) {
                bail!("unsupported attribute path after {model_name}.{attr}: {tail:?}");
            }
            let [uid] = current_ids[..] else {
                return Ok(json!([]));
            };
            let dynamic = registry.dynamic();
            let mut groups: Vec<i32> = if attr == "all_group_ids" {
                if uid == user.uid {
                    user.groups.iter().copied().collect()
                } else {
                    dynamic.security.groups_of(uid).into_iter().collect()
                }
            } else {
                dynamic
                    .security
                    .user_groups
                    .get(&uid)
                    .cloned()
                    .unwrap_or_default()
            };
            groups.sort_unstable();
            return Ok(json!(groups));
        }
        let model = registry.get(&model_name)?;
        let field = model
            .fields
            .get(&attr)
            .ok_or_else(|| anyhow::anyhow!("unknown attr {model_name}.{attr}"))?;
        if let Some(related) = &field.related {
            let expansion: Vec<String> = related.split('.').map(str::to_string).collect();
            path.splice(i..=i, expansion);
            continue;
        }
        match field.ttype {
            FieldType::One2many if field.stored => {
                let inverse = field.inverse_column()?;
                let co = registry.get(field.comodel()?)?;
                let sql = format!(
                    "SELECT id FROM {} WHERE {} = ANY($1)",
                    crate::db::ident(&co.table),
                    crate::db::ident(inverse)
                );
                let rows = db.query(&sql, &[&current_ids]).await?;
                model_name = field.relation.clone().unwrap();
                current_ids = rows.iter().map(|r| r.get::<_, i32>(0)).collect();
                i += 1;
            }
            FieldType::Many2many if field.stored => {
                let (rel, c1, c2) = field.m2m_columns()?;
                let sql = format!(
                    "SELECT {} FROM {} WHERE {} = ANY($1)",
                    crate::db::ident(c2),
                    crate::db::ident(rel),
                    crate::db::ident(c1)
                );
                let rows = db.query(&sql, &[&current_ids]).await?;
                model_name = field.relation.clone().unwrap();
                current_ids = rows.iter().map(|r| r.get::<_, i32>(0)).collect();
                i += 1;
            }
            _ if !field.has_column => {
                bail!("cannot traverse {model_name}.{attr}: it is computed in Python");
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
                    .filter_map(|r| r.get::<_, Option<i32>>(0))
                    .collect();
                i += 1;
            }
            _ => {
                if i != path.len() - 1 {
                    bail!("scalar {model_name}.{attr} mid-path in {chain:?}");
                }

                if current_ids.is_empty() {
                    return Ok(json!(false));
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
                        out.push(match row.get::<_, Option<String>>(0) {
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
                let v: Option<String> = db.query_opt(&sql, &[&id]).await?.and_then(|r| r.get(0));
                return Ok(match v {
                    Some(s) => scalar_json(field, &s)?,
                    None => json!(false),
                });
            }
        }
    }

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
        out.extend(t);
    }
    Json::Array(out)
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
        bail!("_inherits chain too deep at {model}");
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
            None => bail!("_inherits field {model}.{via} is not in the registry"),
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
    let mut applied: Vec<(&crate::registry::Rule, Json)> = Vec::new();
    for rule in rules {
        if !rule.groups.is_empty() && !rule.groups.iter().any(|g| user_groups.contains(g)) {
            continue;
        }
        let dom = match &rule.domain_force {
            Some(src) => {
                let parsed = parse_py(src)?;
                eval_py(&parsed, registry, db, user).await?
            }
            None => json!([]),
        };
        applied.push((rule, dom));
    }
    Ok(combine_rules(inherited, applied))
}

/// Odoo's combination (`ir_rule.py::_get_domain_accessible_records`): the
/// `_inherits` parents' domains and every applicable GLOBAL rule are ANDed,
/// and the applicable GRANTING group rules are ORed together and ANDed onto
/// that. This fork lets a group rule RESTRICT instead
/// (`ir.rule.composition`): such a rule is ANDed like a global, narrowing
/// what the user's other rules grant rather than widening it. Treating it as
/// a grant -- which is what classifying on "has groups" alone did -- turned
/// a restriction into an additional way in.
///
/// `None` when no rule applied and there is no parent domain, which is what
/// the caller reads as "unruled".
pub fn combine_rules(
    inherited: Vec<Json>,
    applied: Vec<(&crate::registry::Rule, Json)>,
) -> Option<Json> {
    if applied.is_empty() {
        return if inherited.is_empty() {
            None
        } else {
            Some(Json::Array(inherited))
        };
    }
    let mut combined: Vec<Json> = inherited;
    let mut group_domains: Vec<Json> = Vec::new();
    for (rule, dom) in applied {
        if rule.is_granting_group() {
            group_domains.push(dom);
        } else {
            combined.extend(dom.as_array().cloned().unwrap_or_default());
        }
    }
    if !group_domains.is_empty() {
        combined.extend(
            or_domains(group_domains)
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
    Some(Json::Array(combined))
}

pub fn check_read_access(
    dynamic: &crate::registry::Dynamic,
    model: &str,
    uid: i32,
    groups: &std::collections::HashSet<i32>,
) -> Result<()> {
    let Some(rows) = dynamic.security.access.get(model) else {
        bail!("access denied: no ir.model.access read entry for {model}");
    };
    if rows
        .iter()
        .any(|g| g.is_none_or(|gid| groups.contains(&gid)))
    {
        Ok(())
    } else {
        bail!("access denied on {model} for uid {uid}")
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
                assert_eq!(chain, ["user", "env", "companies", "country_code"]);
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
                    ["user", "env", "companies", "country_id", "code", "ids"]
                );
            }
            other => panic!("expected a name chain, got {other:?}"),
        }
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
