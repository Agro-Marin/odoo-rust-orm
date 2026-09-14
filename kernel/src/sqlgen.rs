use crate::error::{deny_access, refusal, refusal_at, refuse};
use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, NaiveDateTime};
use sea_query::{Alias, Cond, Condition, Expr, ExprTrait, Func, JoinType, Order, Value};
use serde_json::Value as Json;

use crate::domain::{Leaf, Node};
use crate::registry::{Field, FieldType, Model, Registry};

pub fn pg_cast_type(ttype: FieldType) -> &'static str {
    match ttype {
        FieldType::Boolean => "bool",
        FieldType::Integer | FieldType::Many2one | FieldType::Many2oneReference => "int4",
        FieldType::Float | FieldType::Monetary => "float8",
        FieldType::Text | FieldType::Html => "text",
        FieldType::Date => "date",
        FieldType::Datetime => "timestamp",
        _ => "varchar",
    }
}

pub fn col(table: &str, column: &str) -> Expr {
    Expr::col((Alias::new(table), Alias::new(column)))
}

fn subquery(select: sea_query::SelectStatement) -> Expr {
    Expr::SubQuery(None, Box::new(select.into()))
}

#[derive(Clone)]
pub struct ExprCtx<'a> {
    pub registry: &'a Registry,

    pub dynamic: std::sync::Arc<crate::registry::Dynamic>,
    pub lang: &'a str,
    pub company_id: i32,

    pub access: Option<(i32, std::sync::Arc<std::collections::HashSet<i32>>)>,

    pub active_test: bool,

    // the request's `tz`, already validated by the registry; None reads as UTC
    pub tz: Option<String>,

    /// Every (model, field) whose column the compiled SQL reads. Shared by
    /// every copy of this context -- rule compiles take an owned copy -- so
    /// the set a request ends with covers its sub-queries and its rules. The
    /// persistence port hands it to Odoo as the fragment's `to_flush`: the
    /// fields to write before the statement runs, exact because they are the
    /// columns in it.
    pub touched: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<(String, String)>>>,
}

impl<'a> ExprCtx<'a> {
    pub fn new(registry: &'a Registry, lang: &'a str, company_id: i32) -> Self {
        let dynamic = registry.dynamic();
        ExprCtx::pinned(registry, dynamic, lang, company_id, true)
    }

    pub fn pinned(
        registry: &'a Registry,
        dynamic: std::sync::Arc<crate::registry::Dynamic>,
        lang: &'a str,
        company_id: i32,
        active_test: bool,
    ) -> Self {
        ExprCtx {
            registry,
            dynamic,
            lang,
            company_id,
            active_test,
            access: None,
            tz: None,
            touched: Default::default(),
        }
    }

    pub fn touch(&self, model: &str, field: &str) {
        if let Ok(mut set) = self.touched.lock() {
            set.insert((model.to_string(), field.to_string()));
        }
    }

    pub fn with_tz(mut self, tz: Option<String>) -> Self {
        self.tz = tz;
        self
    }

    pub fn with_access(
        mut self,
        uid: i32,
        groups: std::sync::Arc<std::collections::HashSet<i32>>,
    ) -> Self {
        self.access = Some((uid, groups));
        self
    }

    pub fn for_rules(&self) -> ExprCtx<'a> {
        ExprCtx {
            access: None,
            active_test: false,
            ..self.clone()
        }
    }

    pub fn lang_extract(&self, e: Expr) -> Expr {
        Expr::cust_with_exprs(
            "COALESCE($1 ->> $2::text, $3 ->> 'en_US')",
            [e.clone(), Expr::val(self.lang.to_string()), e],
        )
    }

    fn company_key(&self) -> Expr {
        Expr::val(self.company_id.to_string())
    }

    pub fn cd_fallback_for(&self, model: &Model, f: &Field) -> Option<Json> {
        self.dynamic
            .defaults
            .get(model.name.as_str())
            .and_then(|m| m.get(f.name.as_str()))
            .and_then(|d| d.fallback(self.company_id))
            .cloned()
            .or_else(|| f.cd_fallback.clone().filter(|v| to_value(f, v).is_ok()))
    }

    pub fn field_expr(&self, model: &Model, f: &Field, alias: &str) -> Result<Expr> {
        if !f.has_column {
            refuse!("field {}.{} has no column", model.name, f.name);
        }
        self.touch(&model.name, &f.name);
        let raw = col(alias, &f.name);
        if f.company_dependent {
            let ty = pg_cast_type(f.ttype);

            let fallback = self.cd_fallback_for(model, f);
            // a company-dependent column is a jsonb keyed by company; the
            // fallback is what an unset company key reads as, and getting it
            // wrong changes rows rather than erroring
            tracing::trace!(
                target: "odoo_kernel::compile",
                model = %model.name,
                field = %f.name,
                company_id = self.company_id,
                cast = ty,
                fallback = ?fallback,
                "reading a company-dependent column out of its jsonb"
            );
            let coalesced = match fallback {
                Some(v) => {
                    let param = to_value(f, &v)?;
                    Expr::cust_with_exprs(
                        format!("COALESCE($1 -> $2::text, to_jsonb($3::{ty}))"),
                        [raw, self.company_key(), Expr::val(param)],
                    )
                }
                None => Expr::cust_with_exprs(
                    format!("COALESCE($1 -> $2::text, to_jsonb(NULL::{ty}))"),
                    [raw, self.company_key()],
                ),
            };
            return Ok(match f.ttype {
                FieldType::Boolean
                | FieldType::Integer
                | FieldType::Float
                | FieldType::Monetary => Expr::cust_with_exprs(format!("($1)::{ty}"), [coalesced]),
                // Many2one.to_sql: a company-dependent reference to a deleted
                // record reads NULL, through an existence subselect
                FieldType::Many2one => {
                    let co = self.registry.get(f.comodel()?)?;
                    Expr::cust_with_exprs(
                        format!(
                            "(SELECT e.id FROM {} e WHERE e.id = ($1 ->> 0)::{ty})",
                            crate::db::ident(&co.table)
                        ),
                        [coalesced],
                    )
                }
                _ => Expr::cust_with_exprs(format!("($1 ->> 0)::{ty}"), [coalesced]),
            });
        }
        if f.translated {
            return Ok(self.lang_extract(raw));
        }
        Ok(raw)
    }

    pub fn read_expr(&self, model: &Model, f: &Field, alias: &str) -> Result<Expr> {
        if !f.has_column {
            if f.related.is_some() {
                let path = self.normalize_path(model, std::slice::from_ref(&f.name))?;
                return self.related_expr(alias, model, &path, 0);
            }
            refuse!("field {}.{} is not readable", model.name, f.name);
        }
        let e = self.field_expr(model, f, alias)?;
        if f.pg_type == "numeric" {
            Ok(e.cast_as("float8"))
        } else {
            Ok(e)
        }
    }

    pub fn check_path_readable(&self, model: &Model, raw_path: &[String]) -> Result<()> {
        let Some((uid, groups)) = &self.access else {
            return Ok(());
        };
        let mut m = model;
        for (i, seg) in raw_path.iter().enumerate() {
            let Some(field) = m.fields.get(seg) else {
                return Ok(());
            };
            if !self.registry.field_readable(field, groups) {
                deny_access!(
                    "access denied: uid {uid} may not filter on {}.{}: the field is restricted to {:?}",
                    m.name,
                    field.name,
                    field.groups.as_deref().unwrap_or(".")
                );
            }
            if i + 1 < raw_path.len() {
                let Some(co) = field.relation.as_deref() else {
                    return Ok(());
                };
                let Ok(next) = self.registry.get(co) else {
                    return Ok(());
                };
                m = next;
            }
        }
        Ok(())
    }

    pub fn normalize_path(&self, model: &Model, path: &[String]) -> Result<Vec<String>> {
        self.normalize_path_inner(model, path)
            .map_err(|(site, why)| refusal_at!(site, "{why}"))
    }

    /// `normalize_path` as a QUESTION: a path that does not resolve is an
    /// answer, not a refusal.
    ///
    /// The reachability walk asks which comodels a domain's leaves traverse
    /// and skips a leaf it cannot resolve -- `display_name` is not a registry
    /// field, and a non-stored one cannot be traversed. Routing that through
    /// the demanding form filed a refusal per such leaf, 19% of a census over
    /// 8,254 sweep cases. Same reason `domain::parse_nested` exists.
    pub fn normalize_path_seen(&self, model: &Model, path: &[String]) -> Option<Vec<String>> {
        self.normalize_path_inner(model, path).ok()
    }

    /// The walk both forms share. It reports why it stopped as a plain
    /// message so the caller decides whether that is a refusal.
    fn normalize_path_inner(
        &self,
        model: &Model,
        path: &[String],
    ) -> std::result::Result<Vec<String>, (&'static str, String)> {
        let mut out: Vec<String> = path.to_vec();
        let mut model_name = model.name.clone();
        let mut i = 0usize;
        let mut guard = 0;
        while i < out.len() {
            guard += 1;
            if guard > 64 {
                return Err((
                    concat!(file!(), ":", line!()),
                    format!("related expansion loop at {}.{:?}", model.name, path),
                ));
            }
            let m = self.registry.lookup(&model_name).ok_or((
                concat!(file!(), ":", line!()),
                format!("unknown or table-less model {model_name}"),
            ))?;
            // display_name is no registry field: as the last segment it is the
            // comodel's own display-name search, compiled by the sub-compiler
            if i + 1 == out.len() && out[i] == "display_name" && i > 0 {
                break;
            }
            let field = m.fields.get(&out[i]).ok_or((
                concat!(file!(), ":", line!()),
                format!("unknown field {}.{}", model_name, out[i]),
            ))?;
            if !field.has_column {
                if let Some(rel) = &field.related {
                    let expansion: Vec<String> = rel.split('.').map(str::to_string).collect();
                    tracing::trace!(
                        target: "odoo_kernel::compile",
                        model = %model_name, field = %out[i], related = %rel,
                        "expanded a related field into its path"
                    );
                    out.splice(i..=i, expansion);
                    continue;
                }
                if !matches!(field.ttype, FieldType::One2many | FieldType::Many2many) {
                    return Err((
                        concat!(file!(), ":", line!()),
                        format!("cannot traverse non-stored {}.{}", model_name, out[i]),
                    ));
                }
            }
            if i + 1 < out.len() {
                model_name = field.relation.clone().ok_or((
                    concat!(file!(), ":", line!()),
                    format!("{}.{} is not relational", model_name, out[i]),
                ))?;
            }
            i += 1;
        }
        Ok(out)
    }

    pub fn path_target<'r>(&'r self, model: &'r Model, path: &[String]) -> Result<&'r Model> {
        let mut m = model;
        for seg in path {
            let f = m
                .fields
                .get(seg)
                .ok_or_else(|| refusal!("unknown field {}.{seg}", m.name))?;
            m = self.registry.get(
                f.relation
                    .as_ref()
                    .ok_or_else(|| refusal!("{}.{seg} is not relational", m.name))?,
            )?;
        }
        Ok(m)
    }

    pub fn related_expr(
        &self,
        alias: &str,
        model: &Model,
        path: &[String],
        depth: usize,
    ) -> Result<Expr> {
        self.related_expr_as(alias, model, path, depth, "r")
    }

    pub fn related_expr_as(
        &self,
        alias: &str,
        model: &Model,
        path: &[String],
        depth: usize,
        prefix: &str,
    ) -> Result<Expr> {
        let field = model
            .fields
            .get(&path[0])
            .ok_or_else(|| refusal!("unknown field {}.{}", model.name, path[0]))?;
        if path.len() == 1 {
            return self.read_expr(model, field, alias);
        }
        if field.ttype != FieldType::Many2one || !field.has_column {
            refuse!(
                "related path hop {}.{} is not a stored many2one",
                model.name,
                field.name
            );
        }
        let co = self.registry.get(field.comodel()?)?;
        let sub_alias = format!("{prefix}{depth}_{}", co.table);
        let inner = self.related_expr_as(&sub_alias, co, &path[1..], depth + 1, prefix)?;
        let mut select = sea_query::Query::select();
        select
            .expr(inner)
            .from_as(Alias::new(&co.table), Alias::new(sub_alias.as_str()))
            .and_where(col(&sub_alias, "id").eq(col(alias, &field.name)));
        self.touch(&model.name, &field.name);
        Ok(subquery(select))
    }
}

// date.fromisoformat wants the padded form; chrono also takes '2026-9-1'
// and ' 2026-09-10', which Python rejects
fn strict_date(s: &str) -> Option<NaiveDate> {
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    (d.format("%Y-%m-%d").to_string() == s).then_some(d)
}

fn positive_operator(op: &str) -> Option<&'static str> {
    Some(match op {
        "not any" => "any",
        "not any!" => "any!",
        "not in" => "in",
        "not like" => "like",
        "not ilike" => "ilike",
        "not =like" => "=like",
        "not =ilike" => "=ilike",
        "!=" | "<>" => "=",
        _ => return None,
    })
}

fn to_value(f: &Field, v: &Json) -> Result<Value> {
    let err = || refusal!("cannot convert {v} for {} field {}", f.pg_type, f.name);
    Ok(match f.ttype {
        FieldType::Integer | FieldType::Many2one | FieldType::Many2oneReference => {
            let n = match v {
                Json::Number(_) => v.as_i64().ok_or_else(err)?,
                Json::Bool(b) => *b as i64,
                _ => return Err(err()),
            };

            if f.pg_type == "int8" {
                Value::from(n)
            } else {
                Value::from(i32::try_from(n).map_err(|_| err())?)
            }
        }
        FieldType::Float | FieldType::Monetary => Value::from(v.as_f64().ok_or_else(err)?),
        FieldType::Boolean => Value::from(v.as_bool().ok_or_else(err)?),
        FieldType::Char | FieldType::Text | FieldType::Html | FieldType::Selection => match v {
            Json::String(s) => Value::from(s.clone()),
            Json::Number(n) => Value::from(n.to_string()),
            _ => return Err(err()),
        },
        FieldType::Date => {
            let s = v.as_str().ok_or_else(err)?;
            Value::from(strict_date(s).ok_or_else(err)?)
        }
        FieldType::Datetime => {
            let s = v.as_str().ok_or_else(err)?;
            let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
                .ok()
                .or_else(|| strict_date(s).and_then(|d| d.and_hms_opt(0, 0, 0)))
                .ok_or_else(err)?;
            Value::from(dt)
        }
        _ => return Err(err()),
    })
}

const IN_TO_ANY_THRESHOLD: usize = 100;

pub fn id_membership(sql_field: Expr, ids: impl IntoIterator<Item = i32>) -> Expr {
    let vals: Vec<Value> = ids.into_iter().map(Value::from).collect();
    if vals.is_empty() {
        return Expr::cust("FALSE");
    }
    in_or_any(sql_field, "in", vals)
}

fn in_or_any(sql_field: Expr, op: &str, vals: Vec<Value>) -> Expr {
    // Past IN_TO_ANY_THRESHOLD the membership becomes one array parameter
    // instead of N placeholders, which is what keeps the statement text -- and
    // therefore the prepared-statement cache entry -- stable across calls.
    tracing::trace!(
        target: "odoo_kernel::compile",
        %op,
        values = vals.len(),
        shape = if vals.len() <= IN_TO_ANY_THRESHOLD { "IN (...)" } else { "= ANY($n)" },
        "compiled a membership"
    );
    if vals.len() <= IN_TO_ANY_THRESHOLD {
        return if op == "in" {
            sql_field.is_in(vals)
        } else {
            sql_field.is_not_in(vals)
        };
    }
    let array = Value::Array(vals[0].array_type(), Some(Box::new(vals)));
    let anyall = if op == "in" {
        "= ANY($2)"
    } else {
        "<> ALL($2)"
    };
    Expr::cust_with_exprs(format!("$1 {anyall}"), [sql_field, Expr::val(array)])
}

fn json_eq(a: &Json, b: &Json) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn is_null_like(v: &Json) -> bool {
    matches!(v, Json::Null | Json::Bool(false))
}

fn is_py_falsy(v: &Json) -> bool {
    match v {
        Json::Null | Json::Bool(false) => true,
        Json::Number(n) => n.as_f64() == Some(0.0),
        Json::String(s) => s.is_empty(),
        Json::Array(a) => a.is_empty(),
        Json::Object(o) => o.is_empty(),
        Json::Bool(true) => false,
    }
}

fn is_falsy_id(v: &Json) -> bool {
    matches!(v, Json::Number(n) if n.as_f64() == Some(0.0))
}

fn py_str2bool(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y" | "on" | "t"
    )
}

fn coerce_numeric(v: &Json, integer: bool) -> Option<Json> {
    let Json::String(s) = v else {
        return Some(v.clone());
    };
    if integer && let Ok(i) = s.trim().parse::<i64>() {
        return Some(Json::from(i));
    }
    s.trim().parse::<f64>().ok().map(Json::from)
}

fn leaf(field: &str, op: &str, value: Json) -> Node {
    Node::Leaf(Leaf {
        field: field.to_string(),
        op: op.to_string(),
        value,
    })
}

pub struct Compiler<'a> {
    // borrowed for the request's own compiler and every subquery; owned only
    // where the context changes (rules compile as sudo with active_test off)
    pub ctx: std::borrow::Cow<'a, ExprCtx<'a>>,
    pub model: &'a Model,

    pub alias: String,

    pub rules: &'a crate::security::RuleSet,
    pub su: bool,
    depth: usize,

    stack: Vec<String>,
}

impl<'a> Compiler<'a> {
    pub fn root(
        ctx: &'a ExprCtx<'a>,
        model: &'a Model,
        rules: &'a crate::security::RuleSet,
        su: bool,
    ) -> Self {
        Compiler {
            ctx: std::borrow::Cow::Borrowed(ctx),
            model,
            alias: model.table.clone(),
            rules,
            su,
            depth: 0,
            stack: Vec::new(),
        }
    }

    fn sub_compiler(&self, co: &'a Model, alias: String) -> Compiler<'a> {
        let mut stack = self.stack.clone();
        stack.push(self.model.name.clone());
        Compiler {
            ctx: match &self.ctx {
                std::borrow::Cow::Borrowed(c) => std::borrow::Cow::Borrowed(*c),
                std::borrow::Cow::Owned(c) => std::borrow::Cow::Owned(c.clone()),
            },
            model: co,
            alias,
            rules: self.rules,
            su: self.su,
            depth: self.depth + 1,
            stack,
        }
    }

    fn as_sudo(&self) -> Compiler<'a> {
        Compiler {
            ctx: std::borrow::Cow::Owned(self.ctx.for_rules()),
            model: self.model,
            alias: self.alias.clone(),
            rules: self.rules,
            su: true,
            depth: self.depth,
            stack: self.stack.clone(),
        }
    }

    pub fn compile_rules(&self, node: &Node) -> Result<Condition> {
        self.as_sudo().compile(node)
    }

    const MAX_DEPTH: usize = 32;

    pub fn compile(&self, node: &Node) -> Result<Condition> {
        tracing::trace!(
            target: "odoo_kernel::compile",
            model = %self.model.name, alias = %self.alias, depth = self.depth,
            node = ?std::mem::discriminant(node),
            "compiling a domain node"
        );
        if self.depth > Self::MAX_DEPTH {
            refuse!(
                "domain nesting exceeded {} levels at {}; refusing rather than \
                 recursing further",
                Self::MAX_DEPTH,
                self.model.name
            );
        }
        Ok(match node {
            Node::True => Cond::all(),
            Node::False => Cond::any(),
            Node::And(children) => {
                let mut c = Cond::all();
                for ch in children {
                    c = c.add(self.compile(ch)?);
                }
                c
            }
            Node::Or(children) => {
                let mut c = Cond::any();
                for ch in children {
                    c = c.add(self.compile(ch)?);
                }
                c
            }

            Node::Not(child) => self.compile(&self.negate(child)?)?,
            Node::Leaf(leaf) => Cond::all().add(self.compile_leaf(leaf)?),
        })
    }

    fn negate(&self, node: &Node) -> Result<Node> {
        Ok(match node {
            Node::True => Node::False,
            Node::False => Node::True,
            Node::Not(inner) => (**inner).clone(),
            Node::And(children) => Node::Or(
                children
                    .iter()
                    .map(|c| self.negate(c))
                    .collect::<Result<Vec<_>>>()?,
            ),
            Node::Or(children) => Node::And(
                children
                    .iter()
                    .map(|c| self.negate(c))
                    .collect::<Result<Vec<_>>>()?,
            ),
            Node::Leaf(leaf) => self.negate_leaf(leaf)?,
        })
    }

    fn negate_leaf(&self, leaf: &Leaf) -> Result<Node> {
        // DomainNot._optimize_step optimises the child BEFORE negating it, so
        // a leaf that collapses (`write_date <= False` is FALSE) negates to
        // its collapsed form; negating the raw leaf first would invert the
        // comparison and collapse that to FALSE as well.
        if let Some(rewritten) = self.optimize_leaf(leaf)? {
            return self.negate(&rewritten);
        }
        // Odoo optimises a dotted condition into `head any [rest op v]` before
        // negating it, so `!` on a path becomes `head not any [...]`: a row
        // whose head is unset (or has no corecord) satisfies the negation.
        // Flipping the comparison instead keeps `any`, which drops those rows.
        if let Some((head, rest)) = leaf.field.split_once('.') {
            return Ok(Node::Leaf(Leaf {
                field: head.to_string(),
                op: "not any".into(),
                value: serde_json::json!([[rest, leaf.op, leaf.value]]),
            }));
        }
        let flipped = |op: &str| -> Option<&'static str> {
            Some(match op {
                "any" => "not any",
                "not any" => "any",
                "any!" => "not any!",
                "not any!" => "any!",
                "in" => "not in",
                "not in" => "in",
                "like" => "not like",
                "not like" => "like",
                "ilike" => "not ilike",
                "not ilike" => "ilike",
                "=like" => "not =like",
                "not =like" => "=like",
                "=ilike" => "not =ilike",
                "not =ilike" => "=ilike",
                "=" | "==" => "!=",
                "!=" | "<>" => "=",
                _ => return None,
            })
        };
        if let Some(op) = flipped(&leaf.op) {
            return Ok(Node::Leaf(Leaf {
                field: leaf.field.clone(),
                op: op.to_string(),
                value: leaf.value.clone(),
            }));
        }
        let inverse_inequality = match leaf.op.as_str() {
            "<" => ">=",
            ">" => "<=",
            ">=" => "<",
            "<=" => ">",
            other => refuse!("cannot negate the operator {other:?} on {}", leaf.field),
        };
        let inverted = Node::Leaf(Leaf {
            field: leaf.field.clone(),
            op: inverse_inequality.to_string(),
            value: leaf.value.clone(),
        });

        let raw: Vec<String> = leaf.field.split('.').map(str::to_string).collect();
        let path = self.ctx.normalize_path(self.model, &raw)?;
        let target = self.ctx.path_target(self.model, &path[..path.len() - 1])?;
        let field = target
            .fields
            .get(&path[path.len() - 1])
            .ok_or_else(|| refusal!("unknown field {}", leaf.field))?;
        if field.falsy_json().is_some() {
            return Ok(inverted);
        }
        Ok(Node::Or(vec![
            Node::Leaf(Leaf {
                field: leaf.field.clone(),
                op: "in".into(),
                value: serde_json::json!([false]),
            }),
            inverted,
        ]))
    }

    // `Datetime._optimize_datetime_comparand`: a value that is a bare date
    // names a whole local day, so `>`/`<=` move to the end of that day and a
    // membership becomes one `>= start AND < end` range per day. The day
    // boundaries depend on the request's timezone; the kernel computes them
    // only for UTC and refuses elsewhere rather than comparing at midnight
    fn datetime_day_comparand(&self, fname: &str, op: &str, v: &Json) -> Result<Option<Node>> {
        fn bare_date(v: &Json) -> Option<NaiveDate> {
            strict_date(v.as_str()?)
        }
        let values: Vec<&Json> = match v {
            Json::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        if !values.iter().any(|x| bare_date(x).is_some()) {
            return Ok(None);
        }
        tracing::trace!(
            target: "odoo_kernel::domain",
            field = %fname, %op, value = %v, tz = ?self.ctx.tz,
            "a bare date comparand names a whole day in the request's timezone"
        );
        let tz: Option<chrono_tz::Tz> = match self.ctx.tz.as_deref() {
            None | Some("UTC") => None,
            Some(name) => Some(name.parse().map_err(|_| {
                refusal!(
                    "{fname} {op} names a whole day in timezone {name:?}, which the \
                     kernel's zone table does not know"
                )
            })?),
        };
        let start_of = |d: NaiveDate| -> Result<Json> {
            let midnight = d.and_hms_opt(0, 0, 0).unwrap();
            let utc = match tz {
                Some(z) if !matches!(d.year(), 1 | 9999) => {
                    use chrono::{LocalResult, TimeZone};
                    match z.from_local_datetime(&midnight) {
                        LocalResult::Single(t) => t.naive_utc(),
                        _ => refuse!("midnight of {d} is ambiguous or missing in {z}"),
                    }
                }
                _ => midnight,
            };
            Ok(Json::String(utc.format("%Y-%m-%d %H:%M:%S").to_string()))
        };
        let next = |d: NaiveDate| -> Result<NaiveDate> {
            d.succ_opt()
                .ok_or_else(|| refusal!("no day after {d} for {fname} {op}"))
        };
        if matches!(op, ">" | "<" | ">=" | "<=") {
            let Some(day) = bare_date(v) else {
                return Ok(None);
            };
            return Ok(Some(match op {
                ">" => leaf(fname, ">=", start_of(next(day)?)?),
                "<=" => leaf(fname, "<", start_of(next(day)?)?),
                _ => leaf(fname, op, start_of(day)?),
            }));
        }
        let mut days: Vec<Node> = Vec::new();
        let mut exact: Vec<Json> = Vec::new();
        for x in values {
            match bare_date(x) {
                Some(day) => days.push(Node::And(vec![
                    leaf(fname, ">=", start_of(day)?),
                    leaf(fname, "<", start_of(next(day)?)?),
                ])),
                None => exact.push(x.clone()),
            }
        }
        if !exact.is_empty() {
            days.push(leaf(fname, "in", Json::Array(exact)));
        }
        let node = if days.len() == 1 {
            days.pop().unwrap()
        } else {
            Node::Or(days)
        };
        Ok(Some(if op == "not in" {
            Node::Not(Box::new(node))
        } else {
            node
        }))
    }

    // Odoo optimises a domain leaf before compiling it, and the rewrites are
    // where the semantics live: `=` becomes `in`, a bare date names a whole
    // local day, a string among a relational membership becomes a display-name
    // lookup. Every past wrong answer in this kernel was a rewrite that did
    // not happen or happened differently, so each one announces itself.
    fn optimize_leaf(&self, l: &Leaf) -> Result<Option<Node>> {
        let out = self.optimize_leaf_inner(l)?;
        if let Some(rewritten) = &out {
            tracing::trace!(
                target: "odoo_kernel::domain",
                model = %self.model.name,
                field = %l.field,
                op = %l.op,
                value = %l.value,
                into = ?rewritten,
                "rewrote a leaf"
            );
        }
        Ok(out)
    }

    fn optimize_leaf_inner(&self, l: &Leaf) -> Result<Option<Node>> {
        let head = l.field.split('.').next().unwrap_or(&l.field);
        let dotted = head != l.field;
        let field = self.model.fields.get(head);
        let ttype = field.map(|f| f.ttype);
        let relational = dotted
            || matches!(
                ttype,
                Some(
                    FieldType::Many2one
                        | FieldType::Many2oneReference
                        | FieldType::One2many
                        | FieldType::Many2many
                )
            );
        let op = l.op.as_str();
        let v = &l.value;

        if op == "=?" {
            // `DomainCondition._optimize_step` splits a relational dotted path
            // into `head any [rest op value]` at BASIC, BEFORE any operator
            // optimisation runs, so `=?` is decided inside the sub-domain.
            // Collapsing the whole leaf first turned `('currency_id.decimal_
            // places', '=?', 0)` into TRUE -- every row -- where Odoo reads
            // "currency_id is set"; the fuzzer's seed 3 found it as 232 rows
            // against 0.
            let head_relational = matches!(
                ttype,
                Some(
                    FieldType::Many2one
                        | FieldType::Many2oneReference
                        | FieldType::One2many
                        | FieldType::Many2many
                )
            );
            if dotted && head_relational {
                let rest = &l.field[head.len() + 1..];
                return Ok(Some(leaf(
                    head,
                    "any",
                    serde_json::json!([[rest, "=?", v.clone()]]),
                )));
            }
            return Ok(Some(if is_py_falsy(v) {
                Node::True
            } else {
                leaf(&l.field, "=", v.clone())
            }));
        }
        if (dotted || head != "display_name") && matches!(op, "=" | "==" | "!=" | "<>") {
            let set_op = if matches!(op, "=" | "==") {
                "in"
            } else {
                "not in"
            };
            let items = match v {
                Json::Array(items) if items.is_empty() => vec![Json::Bool(false)],
                Json::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            return Ok(Some(leaf(&l.field, set_op, Json::Array(items))));
        }
        if matches!(op, "in" | "not in") && !v.is_array() {
            if dotted {
                return Ok(None);
            }
            if is_py_falsy(v) {
                return Ok(Some(if op == "in" { Node::False } else { Node::True }));
            }
            return Ok(Some(leaf(&l.field, op, Json::Array(vec![v.clone()]))));
        }

        if op.ends_with("like") {
            let negative = op.starts_with("not ");
            let equal_form = op.contains('=');
            let empty = is_py_falsy(v);
            let only_wildcards =
                matches!(v, Json::String(s) if !s.is_empty() && s.chars().all(|c| c == '%'));
            if (empty || only_wildcards) && dotted {
                return Ok(None);
            }
            if empty || only_wildcards {
                let result = if empty {
                    negative == equal_form
                } else {
                    !negative
                };
                return Ok(Some(if relational || (empty && equal_form) {
                    leaf(&l.field, if result { "!=" } else { "=" }, Json::Bool(false))
                } else if result {
                    Node::True
                } else {
                    Node::False
                }));
            }
            if let Json::Number(n) = v {
                if equal_form {
                    refuse!("the pattern for {op} on {} must be a string", l.field);
                }
                return Ok(Some(leaf(&l.field, op, Json::String(n.to_string()))));
            }
        }

        if !dotted
            && ttype == Some(FieldType::Datetime)
            && matches!(op, "in" | "not in" | ">" | "<" | ">=" | "<=")
            && let Some(node) = self.datetime_day_comparand(&l.field, op, v)?
        {
            return Ok(Some(node));
        }

        let m2o_ref_inequality =
            ttype == Some(FieldType::Many2oneReference) && !matches!(op, "in" | "not in");
        if !dotted
            && relational
            && !m2o_ref_inequality
            && matches!(op, "in" | "not in" | ">" | "<" | ">=" | "<=")
        {
            let m2o = matches!(
                ttype,
                Some(FieldType::Many2one | FieldType::Many2oneReference)
            );
            match v {
                Json::Array(items) if items.iter().any(is_falsy_id) => {
                    let mapped: Vec<Json> = items
                        .iter()
                        .map(|i| {
                            if is_falsy_id(i) {
                                Json::Bool(false)
                            } else {
                                i.clone()
                            }
                        })
                        .collect();
                    return Ok(Some(leaf(&l.field, op, Json::Array(mapped))));
                }
                _ if is_falsy_id(v) && (m2o || matches!(op, "in" | "not in")) => {
                    return Ok(Some(leaf(&l.field, op, Json::Bool(false))));
                }
                _ => {}
            }
            // _Relational._optimize_condition: a string among the members is a
            // display_name lookup on the comodel, OR-ed (AND-ed for `not in`)
            // with the membership of the remaining ids; the sub-domain always
            // carries the POSITIVE operator, `not any` supplies the negation
            if let (Json::Array(items), true) = (v, matches!(op, "in" | "not in"))
                && items.iter().any(Json::is_string)
            {
                let (strs, others): (Vec<Json>, Vec<Json>) =
                    items.iter().cloned().partition(Json::is_string);
                let positive = op == "in";
                let by_name = leaf(
                    &l.field,
                    if positive { "any" } else { "not any" },
                    serde_json::json!([["display_name", "in", strs]]),
                );
                if others.is_empty() {
                    return Ok(Some(by_name));
                }
                let by_id = leaf(&l.field, op, Json::Array(others));
                return Ok(Some(if positive {
                    Node::Or(vec![by_name, by_id])
                } else {
                    Node::And(vec![by_name, by_id])
                }));
            }
        }

        if !dotted
            && matches!(
                ttype,
                Some(FieldType::Integer | FieldType::Float | FieldType::Monetary)
            )
            && matches!(op, "in" | "not in" | ">" | "<" | ">=" | "<=")
        {
            let integer = ttype == Some(FieldType::Integer);
            match v {
                Json::Array(items) if items.iter().any(|i| i.is_string()) => {
                    let coerced: Vec<Json> = items
                        .iter()
                        .filter_map(|i| coerce_numeric(i, integer))
                        .collect();
                    return Ok(Some(leaf(&l.field, op, Json::Array(coerced))));
                }
                Json::String(_) => {
                    return Ok(Some(match coerce_numeric(v, integer) {
                        Some(n) => leaf(&l.field, op, n),
                        None if op == "in" => Node::False,
                        None if op == "not in" => Node::True,
                        None => refuse!("cannot compare the numeric field {} with {v}", l.field),
                    }));
                }
                _ => {}
            }
        }

        if !dotted
            && ttype == Some(FieldType::Boolean)
            && matches!(op, "in" | "not in")
            && let Json::Array(items) = v
            && items.iter().any(|i| !i.is_boolean())
        {
            let coerced: Vec<Json> = items
                .iter()
                .map(|i| match i {
                    Json::String(s) => Json::Bool(py_str2bool(s)),
                    other => Json::Bool(!is_py_falsy(other)),
                })
                .collect();
            return Ok(Some(leaf(&l.field, op, Json::Array(coerced))));
        }

        if !dotted
            && matches!(op, ">" | "<" | ">=" | "<=")
            && is_null_like(v)
            && let Some(f) = field
        {
            return Ok(Some(match f.inequality_falsy_json() {
                Some(falsy) if is_null_like(&falsy) => return Ok(None),
                Some(falsy) => leaf(&l.field, op, falsy),
                None => Node::False,
            }));
        }
        Ok(None)
    }

    fn compile_leaf(&self, leaf: &Leaf) -> Result<Expr> {
        if let Some(rewritten) = self.optimize_leaf(leaf)? {
            return Ok(Expr::expr(self.compile(&rewritten)?));
        }
        let raw_path: Vec<String> = leaf.field.split('.').map(str::to_string).collect();

        if raw_path.len() == 1 && raw_path[0] == "display_name" {
            return self.display_name_condition(self.model, &leaf.op, &leaf.value);
        }
        self.ctx.check_path_readable(self.model, &raw_path)?;
        if raw_path.len() == 1
            && let Some(f) = self.model.fields.get(&raw_path[0])
            && let (false, Some(rel)) = (f.has_column, f.related.as_deref())
        {
            return self.related_search(f, rel, &leaf.op, &leaf.value);
        }

        let path = self.ctx.normalize_path(self.model, &raw_path)?;

        if path.len() > 1 {
            return self.dotted(&path, &leaf.op, &leaf.value);
        }
        let field = self
            .model
            .fields
            .get(&path[0])
            .ok_or_else(|| refusal!("unknown field {}.{}", self.model.name, path[0]))?;

        if field.search_kind.as_deref() == Some("mail_followers_partner") {
            return self.followed_by_partners(field, &leaf.op, &leaf.value);
        }
        if field.custom_search {
            refuse!(
                "{}.{} defines a custom search method; its domain cannot be \
                 compiled from the column",
                self.model.name,
                field.name
            );
        }
        if field.ttype == FieldType::Properties {
            refuse!(
                "{}.{} is a properties field; Odoo searches it by property name \
                 with JSON semantics the kernel does not compile",
                self.model.name,
                field.name
            );
        }

        if field.ttype == FieldType::Many2one && leaf.op.contains("like") {
            return self.m2o_name_search(field, &leaf.op, &leaf.value);
        }

        if field.ttype == FieldType::Boolean
            && (matches!(leaf.op.as_str(), "<" | "<=" | ">" | ">=")
                || leaf.op.contains("like")
                || leaf.op.contains("any"))
        {
            refuse!(
                "operator {:?} is not supported on the boolean field {}.{}",
                leaf.op,
                self.model.name,
                field.name
            );
        }

        match field.ttype {
            FieldType::One2many | FieldType::Many2many if !field.has_column => {
                return self.x2many_condition(field, &leaf.op, &leaf.value);
            }
            FieldType::Many2one
                if matches!(leaf.op.as_str(), "any" | "not any" | "any!" | "not any!") =>
            {
                let sub = crate::domain::parse(&leaf.value)?;
                let bypass = if leaf.op.ends_with('!') {
                    Some(true)
                } else {
                    field.bypass_search_access
                };
                return self.m2o_any(field, &sub, !leaf.op.starts_with("not"), bypass);
            }
            _ => {}
        }

        let (op, values): (&str, Vec<Json>) = match leaf.op.as_str() {
            "=" | "==" => ("in", vec![leaf.value.clone()]),
            "!=" | "<>" => ("not in", vec![leaf.value.clone()]),
            "=?" => {
                if is_null_like(&leaf.value) {
                    return Ok(Expr::cust("TRUE"));
                }
                ("in", vec![leaf.value.clone()])
            }
            "in" | "not in" => {
                let Json::Array(items) = &leaf.value else {
                    refuse!("'{}' operator expects a list, got {}", leaf.op, leaf.value);
                };
                (leaf.op.as_str(), items.clone())
            }
            "child_of" | "parent_of" => {
                refuse!("hierarchy operator {} must be pre-resolved", leaf.op)
            }
            other => (other, vec![leaf.value.clone()]),
        };

        let mut sql_field = self.ctx.field_expr(self.model, field, &self.alias)?;

        if field.pg_type == "numeric" {
            sql_field = sql_field.cast_as("float8");
        }
        let can_be_null = !field.not_null;

        let cond = match op {
            "in" | "not in" => self.in_condition(field, sql_field, op, &values, can_be_null),
            _ if field.ttype == FieldType::Boolean => refuse!(
                "operator {op:?} is not supported on the boolean field {}.{}",
                self.model.name,
                field.name
            ),
            _ if op.ends_with("like") => {
                self.like_condition(field, sql_field, op, &values[0], can_be_null)
            }
            "<" | ">" | "<=" | ">=" => {
                self.inequality_condition(field, sql_field, op, &values[0], can_be_null)
            }
            other => refuse!("unsupported operator {other:?}"),
        }?;
        let cond = match self.trigram_accelerator_for_value(field, &values) {
            Some(accelerator) if op == "in" => accelerator.and(cond),
            _ => cond,
        };
        Ok(self.company_dependent_guard(field, op, &values, cond))
    }

    fn related_search(&self, f: &Field, rel: &str, op: &str, value: &Json) -> Result<Expr> {
        let falsy = f.falsy_json();
        let nullish = |v: &Json| is_null_like(v) || falsy.as_ref().is_some_and(|fv| json_eq(fv, v));
        let value_is_null = match value {
            Json::Array(items) => items.iter().any(nullish),
            other => nullish(other),
        };
        if let Some(positive) = positive_operator(op)
            && !value_is_null
        {
            let node = self.related_domain(f, rel, positive, value, false)?;
            return Ok(Expr::expr(self.compile(&Node::Not(Box::new(node)))?));
        }
        let can_be_null = positive_operator(op).is_none() == value_is_null;
        let node = self.related_domain(f, rel, op, value, can_be_null)?;
        Ok(Expr::expr(self.compile(&node)?))
    }

    fn related_domain(
        &self,
        f: &Field,
        rel: &str,
        op: &str,
        value: &Json,
        can_be_null: bool,
    ) -> Result<Node> {
        let steps: Vec<&str> = rel.split('.').collect();
        let mut hops: Vec<&Field> = Vec::with_capacity(steps.len());
        let mut m = self.model;
        for step in &steps[..steps.len() - 1] {
            let hop = m.fields.get(*step).ok_or_else(|| {
                refusal!(
                    "related {}.{} names unknown {step}",
                    self.model.name,
                    f.name
                )
            })?;
            hops.push(hop);
            m = self.ctx.registry.get(hop.comodel()?)?;
        }
        let any_op = if f.compute_sudo { "any!" } else { "any" };
        let mut domain = serde_json::json!([[steps[steps.len() - 1], op, value]]);
        for (i, step) in steps[..steps.len() - 1].iter().enumerate().rev() {
            let hop = hops[i];
            domain = if can_be_null && hop.ttype == FieldType::Many2one && !hop.required {
                serde_json::json!(["|", [step, any_op, domain], [step, "=", false]])
            } else {
                serde_json::json!([[step, any_op, domain]])
            };
        }
        crate::domain::parse(&domain)
    }

    fn company_dependent_guard(
        &self,
        field: &Field,
        op: &str,
        values: &[Json],
        cond: Expr,
    ) -> Expr {
        if !field.company_dependent || field.index.as_deref() != Some("btree_not_null") {
            return cond;
        }
        // `_evaluate_condition_with_fallback` runs the condition on a record
        // holding the LIVE fallback (ir.default for the company, else the
        // field's own) and guards with IS NOT NULL only when that is False
        let fallback = self
            .ctx
            .cd_fallback_for(self.model, field)
            .or_else(|| field.falsy_json())
            .unwrap_or(Json::Null);
        // filtered_domain sees False, None and the field's falsy value as one
        // unset value: `barcode = False` on a Char fallback of '' is True
        let falsy = field.falsy_json();
        let unset = |v: &Json| is_null_like(v) || falsy.as_ref().is_some_and(|f| json_eq(f, v));
        let matches_fallback = |v: &Json| (unset(v) && unset(&fallback)) || json_eq(v, &fallback);
        let like_matches = |pattern: &Json| -> bool {
            let text = match &fallback {
                Json::String(s) => s.clone(),
                Json::Null | Json::Bool(false) => String::new(),
                other => other.to_string(),
            };
            let Some(pat) = pattern.as_str() else {
                return false;
            };
            let ci = op.contains("ilike");
            let full = if op.contains('=') {
                pat.to_string()
            } else {
                format!("%{pat}%")
            };
            sql_like(&text, &full, ci)
        };
        let satisfied = match op {
            "in" => values.iter().any(matches_fallback),
            "not in" => !values.iter().any(matches_fallback),
            "<" | ">" | "<=" | ">=" => values
                .first()
                .and_then(|v| json_cmp_op(&fallback, v, op))
                .unwrap_or(true),
            _ if op.ends_with("like") => {
                let hit = values.first().is_some_and(like_matches);
                if op.starts_with("not ") { !hit } else { hit }
            }
            _ => true,
        };
        // Odoo evaluates the condition against a record holding the live
        // fallback; when that record would NOT satisfy it, rows with no
        // company key must be excluded, and the IS NOT NULL guard is how
        tracing::trace!(
            target: "odoo_kernel::compile",
            model = %self.model.name,
            field = %field.name,
            %op,
            fallback = %fallback,
            satisfied,
            guarded = !satisfied,
            "guarded a company-dependent condition against its fallback"
        );
        if satisfied {
            cond
        } else {
            self.ctx.touch(&self.model.name, &field.name);
            col(&self.alias, &field.name).is_not_null().and(cond)
        }
    }

    fn dotted(&self, path: &[String], op: &str, value: &Json) -> Result<Expr> {
        let head = self
            .model
            .fields
            .get(&path[0])
            .ok_or_else(|| refusal!("unknown field {}.{}", self.model.name, path[0]))?;
        let sub_leaf = Node::Leaf(Leaf {
            field: path[1..].join("."),
            op: op.to_string(),
            value: value.clone(),
        });
        match head.ttype {
            FieldType::Many2one => self.m2o_any(head, &sub_leaf, true, head.bypass_search_access),
            FieldType::One2many | FieldType::Many2many => self.x2many_subselect(
                head,
                Some(&sub_leaf),
                true,
                true,
                false,
                head.bypass_search_access,
            ),
            _ => refuse!(
                "cannot traverse non-relational {}.{}",
                self.model.name,
                path[0]
            ),
        }
    }

    pub fn field_domain_cond(
        compiler: &Compiler<'_>,
        field: &Field,
        co: &Model,
        owner: &Model,
    ) -> Result<Condition> {
        if compiler.ctx.registry.source == crate::registry::Source::Bootstrap {
            refuse!(
                "x2many {}.{} cannot be read from an ir_model bootstrap registry: \
                 field-level domains are not recorded there. Build the registry \
                 from a live-registry export.",
                owner.name,
                field.name
            );
        }
        if field.domain_callable {
            refuse!(
                "{}.{} carries a domain computed in Python; which {} rows count as its \
                 members is decided per record, and the relation table is a wider answer",
                owner.name,
                field.name,
                co.name
            );
        }
        let mut cond = Cond::all();
        if let Some(dom) = &field.domain {
            let node = crate::domain::parse(dom)
                .with_context(|| format!("field-level domain on {}: {dom}", field.name))?;
            cond = cond.add(compiler.compile(&node)?);
        }
        if field.ttype == FieldType::One2many {
            let inverse = field.o2m_inverse()?;
            if let Some(inv) = co.fields.get(inverse)
                && inv.ttype == FieldType::Many2oneReference
            {
                let model_field = inv.model_field.as_deref().ok_or_else(|| {
                    refusal!(
                        "{}.{} is a many2one_reference with no model_field; a \
                             one2many over it would read other models' rows",
                        co.name,
                        inverse
                    )
                })?;
                compiler.ctx.touch(&co.name, model_field);
                cond = cond.add(col(&compiler.alias, model_field).eq(owner.name.as_str()));
            }
        }
        Ok(cond)
    }

    fn active_filter(
        &self,
        field: &Field,
        co: &Model,
        sub_node: Option<&Node>,
    ) -> Result<Condition> {
        let Some(active_name) = co.active_name.as_deref() else {
            return Ok(Cond::all());
        };
        if !field.context_active_test()?.unwrap_or(self.ctx.active_test) {
            return Ok(Cond::all());
        }
        let mut referenced = Vec::new();
        if let Some(node) = sub_node {
            crate::domain::referenced_fields(node, &mut referenced);
        }
        if let Some(dom) = &field.domain {
            crate::domain::referenced_fields(&crate::domain::parse(dom)?, &mut referenced);
        }
        if referenced.iter().any(|f| f == active_name) {
            tracing::trace!(
                target: "odoo_kernel::compile",
                comodel = %co.name, %active_name,
                "the sub-domain names the active field itself, so no implicit filter is added"
            );
            return Ok(Cond::all());
        }
        tracing::trace!(
            target: "odoo_kernel::compile",
            comodel = %co.name, %active_name, "added the implicit active filter to the subquery"
        );
        self.ctx.touch(&co.name, active_name);
        Ok(Cond::all().add(col(&self.alias, active_name).is_in([true])))
    }

    fn comodel_rules(
        &self,
        co: &'a Model,
        bypass_access: Option<bool>,
    ) -> Result<Option<&'a Node>> {
        if !co.search_pure {
            refuse!(
                "the subquery traverses {}, which defines `_search` in Python",
                co.name
            );
        }
        if self.su || bypass_access == Some(true) {
            tracing::trace!(
                target: "odoo_kernel::rules",
                comodel = %co.name, su = self.su, ?bypass_access,
                "the subquery bypasses the comodel's record rules, as Odoo does"
            );
            return Ok(None);
        }

        if self.stack.contains(&co.name) {
            refuse!(
                "record rules on {} recurse through {}; refusing to compile the \
                 subquery without them",
                co.name,
                self.model.name
            );
        }

        if let Some((uid, groups)) = &self.ctx.access {
            crate::security::check_read_access(&self.ctx.dynamic, &co.name, *uid, groups)?;
        }
        self.rules.ensure_evaluated(&co.name)?;

        let rules = self.rules.get(&co.name);
        tracing::trace!(
            target: "odoo_kernel::rules",
            comodel = %co.name, restricted = rules.is_some(), ?bypass_access,
            "the subquery applies the comodel's record rules"
        );
        if bypass_access.is_none() && rules.is_some() {
            refuse!(
                "whether {} bypasses {}'s record rules is not recorded in \
                 `ir_model_fields`, and applying them when Odoo would not \
                 answers with fewer rows; build the registry from a \
                 live-registry export",
                self.model.name,
                co.name
            );
        }
        Ok(rules)
    }

    fn m2o_name_search(&self, field: &Field, op: &str, value: &Json) -> Result<Expr> {
        let co = self.ctx.registry.get(field.comodel()?)?;
        let positive = !op.starts_with("not ");
        let pos_op = op.strip_prefix("not ").unwrap_or(op);
        let sub = Self::display_name_node(co, pos_op, value)?;
        self.m2o_any(field, &sub, positive, field.bypass_search_access)
    }

    fn display_name_condition(&self, model: &Model, op: &str, value: &Json) -> Result<Expr> {
        if !op.contains("like") && !matches!(op, "=" | "!=" | "in" | "not in") {
            refuse!(
                "display_name `{op}` on {}: only the like family and equality are \
                 compiled from the name columns",
                model.name
            );
        }
        let node = Self::display_name_node(model, op, value)?;
        Ok(Expr::expr(self.compile(&node)?))
    }

    fn display_name_node(co: &Model, op: &str, value: &Json) -> Result<Node> {
        if co
            .name_search_fields
            .as_deref()
            .is_some_and(|f| f.iter().any(|n| n == "display_name"))
        {
            refuse!(
                "{} searches `display_name` to answer `display_name`; Odoo breaks \
                 that loop in Python and this kernel refuses it",
                co.name
            );
        }
        let Some(fnames) = co.name_search_fields.as_deref() else {
            refuse!(
                "{} defines its display name in Python (_search_display_name, \
                 _rec_names_search or a relational _rec_name); the columns \
                 cannot express a search on it",
                co.name
            );
        };
        let negative = matches!(op, "!=" | "not in") || op.starts_with("not ");
        let empty = matches!(value, Json::String(s) if s.is_empty())
            || matches!(value, Json::Bool(false) | Json::Null);
        if op.ends_with("like") && empty && !op.contains('=') {
            return Ok(if negative { Node::False } else { Node::True });
        }
        if !co.display_name_search_exact.is_empty() && matches!(op, "in" | "ilike") && !empty {
            refuse!(
                "{} lets an exact match on {} take precedence over its display name;                  the shim resolves that before the kernel and this leaf was not resolved",
                co.name,
                co.display_name_search_exact.join(", ")
            );
        }
        // an ordering comparison uses the first name field only
        let fnames: &[String] = if matches!(op, "<" | "<=" | ">" | ">=") {
            &fnames[..1]
        } else {
            fnames
        };
        let relational = |fname: &str| co.fields.get(fname).is_some_and(|f| f.relation.is_some());
        let aggregate = |mut terms: Vec<Node>| match (terms.len(), negative) {
            (1, _) => terms.pop().unwrap(),
            (_, true) => Node::And(terms),
            (_, false) => Node::Or(terms),
        };
        // _search_display_name_match: a relational name field is searched
        // through its comodel's display_name, on the path, so the negation
        // stays inside the traversal and an unset head does not match
        let matching = |op: &str, value: &Json| {
            aggregate(
                fnames
                    .iter()
                    .map(|fname| {
                        if relational(fname) {
                            leaf(&format!("{fname}.display_name"), op, value.clone())
                        } else {
                            leaf(fname, op, value.clone())
                        }
                    })
                    .collect(),
            )
        };

        if matches!(op, "=" | "!=" | "in" | "not in") {
            let unset = |v: &Json| {
                matches!(v, Json::Null | Json::Bool(false))
                    || matches!(v, Json::String(s) if s.is_empty())
            };
            let values: Vec<Json> = match value {
                Json::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            let present: Vec<Json> = values.iter().filter(|v| !unset(v)).cloned().collect();
            if present.len() == values.len() {
                return Ok(matching(op, value));
            }
            // _search_display_name_unset: every name field unset; a relational
            // one is unset when the field is, or its target's display_name is
            let unset_all = Node::And(
                fnames
                    .iter()
                    .map(|f| {
                        if relational(f) {
                            Node::Or(vec![
                                leaf(f, "=", Json::Bool(false)),
                                leaf(&format!("{f}.display_name"), "=", Json::Bool(false)),
                            ])
                        } else {
                            leaf(f, "=", Json::Bool(false))
                        }
                    })
                    .collect(),
            );
            let unset_node = if negative {
                Node::Not(Box::new(unset_all))
            } else {
                unset_all
            };
            let mut parts = Vec::new();
            if !present.is_empty() {
                parts.push(matching(op, &Json::Array(present)));
            }
            parts.push(unset_node);
            return Ok(aggregate(parts));
        }
        Ok(matching(op, value))
    }

    fn followed_by_partners(&self, field: &Field, op: &str, value: &Json) -> Result<Expr> {
        if !self.su {
            refuse!(
                "{}.{} searches followers as the requesting user, where \
                 _search_message_partner_ids checks portal partners in Python",
                self.model.name,
                field.name
            );
        }
        let positive = match op {
            "in" | "=" => true,
            "not in" | "!=" => false,
            other => refuse!(
                "{}.{} {other}: _search_message_partner_ids is compiled for in and = only",
                self.model.name,
                field.name
            ),
        };
        let items: Vec<&Json> = match value {
            Json::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        let partner_ids = items
            .into_iter()
            .map(|v| {
                v.as_i64().ok_or_else(|| {
                    refusal!(
                        "{}.{} {op} {v}: a follower search by anything but partner ids \
                         is left to Python",
                        self.model.name,
                        field.name
                    )
                })
            })
            .collect::<Result<Vec<i64>>>()?;
        if partner_ids.is_empty() {
            return Ok(Expr::cust(if positive { "FALSE" } else { "TRUE" }));
        }
        let followers = self.ctx.registry.get("mail.followers")?;
        for column in ["res_model", "res_id", "partner_id"] {
            if !followers.fields.get(column).is_some_and(|f| f.has_column) {
                refuse!("mail.followers.{column} is not a column this registry reads");
            }
        }
        let alias = format!("s{}_{}", self.depth, followers.table);
        let mut select = sea_query::Query::select();
        select
            .expr(col(&alias, "res_id"))
            .from_as(Alias::new(&followers.table), Alias::new(alias.as_str()))
            .and_where(col(&alias, "res_model").eq(self.model.name.as_str()))
            .and_where(id_membership(
                col(&alias, "partner_id"),
                partner_ids.iter().map(|id| *id as i32),
            ));
        let followed = col(&self.alias, "id").in_subquery(select);
        Ok(if positive { followed } else { followed.not() })
    }

    fn m2o_any(
        &self,
        field: &Field,
        sub_node: &Node,
        positive: bool,
        bypass_access: Option<bool>,
    ) -> Result<Expr> {
        let co = self.ctx.registry.get(field.comodel()?)?;
        let sub_alias = format!("s{}_{}", self.depth, co.table);
        tracing::debug!(
            target: "odoo_kernel::compile",
            model = %self.model.name,
            field = %field.name,
            comodel = %co.name,
            alias = %sub_alias,
            positive,
            depth = self.depth,
            "traversing a many2one as an id subquery"
        );
        let sub_compiler = self.sub_compiler(co, sub_alias.clone());
        let mut cond = Cond::all().add(sub_compiler.compile(sub_node)?);
        if let Some(rules) = self.comodel_rules(co, bypass_access)? {
            cond = cond.add(sub_compiler.compile_rules(rules)?);
        }
        let mut select = sea_query::Query::select();
        select
            .expr(col(&sub_alias, "id"))
            .from_as(Alias::new(&co.table), Alias::new(sub_alias.as_str()))
            .cond_where(cond);
        let sql_field = self.ctx.field_expr(self.model, field, &self.alias)?;
        let can_be_null = !field.not_null;
        Ok(if positive {
            sql_field.in_subquery(select)
        } else if can_be_null {
            sql_field
                .clone()
                .is_null()
                .or(sql_field.not_in_subquery(select))
        } else {
            sql_field.not_in_subquery(select)
        })
    }

    fn x2many_condition(&self, field: &Field, op: &str, value: &Json) -> Result<Expr> {
        match op {
            "any" | "not any" | "any!" | "not any!" => {
                let sub = crate::domain::parse(value)?;
                // `any!` skips the comodel's access on EVERY relational field
                // (`_base.py`: `bypass_access = self.bypass_search_access or
                // operator in ("any!", "not any!")`); the many2one path
                // honours it below and this one used to drop the `!`
                let bypass = if op.ends_with('!') {
                    Some(true)
                } else {
                    field.bypass_search_access
                };
                self.x2many_subselect(
                    field,
                    Some(&sub),
                    !op.starts_with("not"),
                    true,
                    false,
                    bypass,
                )
            }
            "in" | "=" | "not in" | "!=" => {
                let positive = matches!(op, "in" | "=");
                let items: Vec<&Json> = match value {
                    Json::Array(items) => items.iter().collect(),
                    other => vec![other],
                };

                let mut ids: Vec<i64> = Vec::new();
                let mut match_empty = false;
                for v in items {
                    if is_null_like(v) {
                        match_empty = true;
                    } else if let Some(id) = v.as_i64() {
                        ids.push(id);
                    } else {
                        refuse!(
                            "unsupported value {v} for {}.{} {op}",
                            self.model.name,
                            field.name
                        );
                    }
                }
                let matching = if ids.is_empty() {
                    None
                } else {
                    let sub = Node::Leaf(Leaf {
                        field: "id".into(),
                        op: "in".into(),
                        value: serde_json::json!(ids),
                    });
                    Some(self.x2many_subselect(
                        field,
                        Some(&sub),
                        true,
                        false,
                        true,
                        field.bypass_search_access,
                    )?)
                };
                let empty = match match_empty {
                    true => Some(self.x2many_subselect(
                        field,
                        None,
                        false,
                        false,
                        true,
                        field.bypass_search_access,
                    )?),
                    false => None,
                };
                let combined = match (matching, empty) {
                    (Some(m), Some(e)) => e.or(m),
                    (Some(m), None) => m,
                    (None, Some(e)) => e,

                    (None, None) => Expr::cust("FALSE"),
                };
                Ok(if positive { combined } else { combined.not() })
            }
            _ => refuse!("unsupported x2many operator {op}"),
        }
    }

    fn x2many_subselect(
        &self,
        field: &Field,
        sub_node: Option<&Node>,
        positive: bool,
        apply_active: bool,
        sudo: bool,
        bypass_access: Option<bool>,
    ) -> Result<Expr> {
        let co = self.ctx.registry.get(field.comodel()?)?;
        let sub_alias = format!("s{}_{}", self.depth, co.table);
        tracing::debug!(
            target: "odoo_kernel::compile",
            model = %self.model.name,
            field = %field.name,
            kind = ?field.ttype,
            comodel = %co.name,
            alias = %sub_alias,
            positive,
            apply_active,
            sudo,
            depth = self.depth,
            "traversing an x2many as a parent-id subquery"
        );
        let mut sub_compiler = self.sub_compiler(co, sub_alias.clone());
        if sudo {
            sub_compiler = sub_compiler.as_sudo();
        }
        let field_cond = Self::field_domain_cond(&sub_compiler, field, co, self.model)?;
        let mut cond = Cond::all().add(field_cond);
        if let Some(node) = sub_node {
            cond = cond.add(sub_compiler.compile(node)?);

            if apply_active {
                cond = cond.add(sub_compiler.active_filter(field, co, Some(node))?);
            }
        }
        if let Some(rules) = sub_compiler.comodel_rules(co, bypass_access)? {
            cond = cond.add(sub_compiler.compile_rules(rules)?);
        }

        let mut select = sea_query::Query::select();
        match field.ttype {
            FieldType::One2many => {
                let inverse = field.o2m_inverse_column(&self.model.name, co)?;
                self.ctx.touch(&co.name, inverse);
                select
                    .expr(col(&sub_alias, inverse))
                    .from_as(Alias::new(&co.table), Alias::new(sub_alias.as_str()))
                    .and_where(col(&sub_alias, inverse).is_not_null())
                    .cond_where(cond);
            }
            FieldType::Many2many => {
                let (rel, c1, c2) = field.m2m_columns()?;
                // the relation table has no fields of its own; what Odoo
                // flushes for it is the many2many field
                self.ctx.touch(&self.model.name, &field.name);
                select
                    .expr(col(rel, c1))
                    .from(Alias::new(rel))
                    .join_as(
                        JoinType::InnerJoin,
                        Alias::new(&co.table),
                        Alias::new(sub_alias.as_str()),
                        col(rel, c2).equals((Alias::new(sub_alias.as_str()), Alias::new("id"))),
                    )
                    .cond_where(cond);
            }
            _ => unreachable!(),
        }
        let id_col = col(&self.alias, "id");
        Ok(if positive {
            id_col.in_subquery(select)
        } else {
            id_col.in_subquery(select).not()
        })
    }

    fn in_condition(
        &self,
        field: &Field,
        sql_field: Expr,
        op: &str,
        values: &[Json],
        can_be_null: bool,
    ) -> Result<Expr> {
        if values.is_empty() {
            return Ok(Expr::cust(if op == "in" { "FALSE" } else { "TRUE" }));
        }
        let mut params: Vec<Json> = values
            .iter()
            .filter(|v| !is_null_like(v))
            .cloned()
            .collect();
        let mut null_in_condition = params.len() < values.len();
        if let Some(falsy) = field.falsy_json() {
            if field.ttype == FieldType::Boolean {
                if null_in_condition {
                    params.push(falsy);
                }
            } else if params.iter().any(|p| json_eq(p, &falsy)) {
                null_in_condition = true;
            } else if null_in_condition {
                params.push(falsy);
            }
        }

        // Odoo reads NULL and the field's falsy value as one unset value, so a
        // membership that contains either widens to `IS NULL` as well; this is
        // the line that says whether a row with no value answers the leaf
        tracing::trace!(
            target: "odoo_kernel::compile",
            model = %self.model.name,
            field = %field.name,
            %op,
            given = values.len(),
            bound = params.len(),
            null_in_condition,
            can_be_null,
            falsy = ?field.falsy_json(),
            "compiled a membership on a column"
        );
        let sql = if params.is_empty() {
            None
        } else {
            let vals = params
                .iter()
                .map(|p| to_value(field, p))
                .collect::<Result<Vec<_>>>()?;
            Some(in_or_any(sql_field.clone(), op, vals))
        };

        if (op == "in") == null_in_condition {
            if !can_be_null {
                return Ok(sql.unwrap_or_else(|| Expr::cust("FALSE")));
            }
            let sql_null = sql_field.is_null();
            return Ok(match sql {
                Some(s) => s.or(sql_null),
                None => sql_null,
            });
        }
        if op == "not in" && null_in_condition && sql.is_none() {
            return Ok(if can_be_null {
                sql_field.is_not_null()
            } else {
                Expr::cust("TRUE")
            });
        }
        // NOT a refusal: reaching this means the branches above disagreed with
        // each other, which is a defect in this function rather than a
        // capability the kernel lacks. It carries no `odoo_kernel::refusal`
        // line for that reason, and logs at `error` so it is visible with
        // `RUSTORM_LOG` unset -- the default filter is `warn`.
        sql.ok_or_else(|| {
            tracing::error!(
                target: "odoo_kernel::compile",
                model = %self.model.name,
                field = %field.name,
                %op,
                values = values.len(),
                null_in_condition,
                can_be_null,
                "kernel defect: in_condition produced no SQL for a membership it accepted"
            );
            anyhow::anyhow!("missing sql for {op} {values:?}")
        })
    }

    /// Odoo's trigram accelerator: a conjunct on the ONE expression the GIN
    /// index is declared over, which the base condition already implies.
    ///
    /// Without it a `like` over a translated `index="trigram"` field is a
    /// sequential scan, because the index is on
    /// `unaccent(jsonb_path_query_array(col, '$.*')::text)` and nothing else
    /// can use it -- so the kernel was slower than the Python it replaces on
    /// exactly the path a product autocomplete takes. See `crate::trigram`.
    ///
    /// Only the POSITIVE operators, as Odoo does: `not like` gets no
    /// conjunct, because "does not contain" is not implied by the prefilter.
    fn trigram_indexed(&self, field: &Field) -> bool {
        self.ctx.registry.has_trigram
            && field.translated
            && field.index.as_deref() == Some("trigram")
    }

    /// The conjunct itself, over the RAW jsonb column rather than the
    /// language extraction the base condition compares: the index is over
    /// every translation, and only that expression can use it.
    fn trigram_conjunct(&self, field: &Field, pattern: String, insensitive: bool) -> Expr {
        let left = Expr::cust_with_exprs(
            "jsonb_path_query_array($1, '$.*')::text",
            [{
                self.ctx.touch(&self.model.name, &field.name);
                col(&self.alias, &field.name)
            }],
        );
        let keyword = if insensitive { "ILIKE" } else { "LIKE" };
        if self.ctx.registry.has_unaccent {
            Expr::cust_with_exprs(
                format!("unaccent($1) {keyword} unaccent($2)"),
                [left, Expr::val(pattern)],
            )
        } else {
            Expr::cust_with_exprs(format!("$1 {keyword} $2"), [left, Expr::val(pattern)])
        }
    }

    fn trigram_accelerator(&self, field: &Field, op: &str, raw: &str) -> Option<Expr> {
        if !self.trigram_indexed(field) || op.starts_with("not ") || raw.is_empty() {
            return None;
        }
        let pattern = crate::trigram::pattern_to_pattern(raw);
        // Without a conjunct the LIKE is a sequential scan on a large table,
        // which is the one shape where this kernel was slower than the Python
        // it replaces; `%` means the pattern has no run long enough to
        // constrain a trigram index, so no conjunct is implied.
        tracing::debug!(
            target: "odoo_kernel::compile",
            model = %self.model.name,
            field = %field.name,
            %op,
            accelerated = pattern != "%",
            %pattern,
            "trigram accelerator for a like on a translated indexed field"
        );
        if pattern == "%" {
            return None;
        }
        Some(self.trigram_conjunct(field, pattern, op.ends_with("ilike")))
    }

    /// The same accelerator for a single-valued `in` -- which is what an `=`
    /// on a translated field becomes. Odoo compares it with LIKE rather than
    /// ILIKE, because the equality it accompanies is case-sensitive too.
    fn trigram_accelerator_for_value(&self, field: &Field, values: &[Json]) -> Option<Expr> {
        if !self.trigram_indexed(field) {
            return None;
        }
        let [Json::String(one)] = values else {
            return None;
        };
        let pattern = crate::trigram::value_to_pattern(one);
        tracing::debug!(
            target: "odoo_kernel::compile",
            model = %self.model.name,
            field = %field.name,
            accelerated = pattern != "%",
            %pattern,
            "trigram accelerator for a single-valued equality on a translated indexed field"
        );
        if pattern == "%" {
            return None;
        }
        Some(self.trigram_conjunct(field, pattern, false))
    }

    fn like_condition(
        &self,
        field: &Field,
        sql_field: Expr,
        op: &str,
        value: &Json,
        can_be_null: bool,
    ) -> Result<Expr> {
        let raw = match value {
            Json::String(s) => s.clone(),
            other => refuse!("invalid value for {op}: {other}"),
        };
        let sql_left = if field.ttype.is_text() {
            sql_field.clone()
        } else {
            sql_field.clone().cast_as("text")
        };
        let need_wildcard = !op.contains('=');
        let pattern = if need_wildcard {
            format!("%{raw}%")
        } else {
            raw.clone()
        };
        let insensitive = op.ends_with("ilike");
        let negative = op.starts_with("not ");
        // unaccent() on both sides is what Odoo emits when the function
        // exists; without it an ilike compares accented text literally, so
        // the same domain answers differently on a database that lacks it
        tracing::trace!(
            target: "odoo_kernel::compile",
            model = %self.model.name,
            field = %field.name,
            %op,
            unaccent = insensitive && self.ctx.registry.has_unaccent,
            wildcarded = need_wildcard,
            can_be_null,
            "compiled a like"
        );

        let mut sql = if insensitive && self.ctx.registry.has_unaccent {
            let keyword = if negative { "NOT ILIKE" } else { "ILIKE" };
            Expr::cust_with_exprs(
                format!("unaccent($1) {keyword} unaccent($2)"),
                [sql_left, Expr::val(pattern)],
            )
        } else {
            use sea_query::extension::postgres::PgExpr;
            match (insensitive, negative) {
                (true, false) => sql_left.ilike(pattern),
                (true, true) => sql_left.not_ilike(pattern),
                (false, false) => sql_left.like(pattern),
                (false, true) => sql_left.not_like(pattern),
            }
        };
        if negative && can_be_null {
            sql = sql.or(sql_field.is_null());
        }
        if let Some(accelerator) = self.trigram_accelerator(field, op, &raw) {
            sql = accelerator.and(sql);
        }
        Ok(sql)
    }

    fn inequality_condition(
        &self,
        field: &Field,
        sql_field: Expr,
        op: &str,
        value: &Json,
        can_be_null: bool,
    ) -> Result<Expr> {
        // an unset comparand never reaches here: optimize_leaf rewrote it to
        // the field's falsy value or to FALSE
        if field.ttype == FieldType::Html {
            refuse!(
                "{} {op} compares against the sanitized value, which Python computes",
                field.name
            );
        }
        let textual = matches!(
            field.ttype,
            FieldType::Char | FieldType::Text | FieldType::Selection
        );
        let comparand = match value {
            Json::Number(n) if textual => Json::String(n.to_string()),
            other => other.clone(),
        };
        let accept_null = field.falsy_json().is_some_and(|falsy| {
            can_be_null && json_cmp_op(&falsy, &comparand, op).unwrap_or(false)
        });
        let v = to_value(field, value)?;
        let mut sql = match op {
            "<" => sql_field.clone().lt(v),
            ">" => sql_field.clone().gt(v),
            "<=" => sql_field.clone().lte(v),
            ">=" => sql_field.clone().gte(v),
            _ => unreachable!(),
        };
        if accept_null {
            sql = sql.or(sql_field.is_null());
        }
        Ok(sql)
    }
}

fn sql_like(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let (t, p): (Vec<char>, Vec<char>) = if case_insensitive {
        (
            text.to_lowercase().chars().collect(),
            pattern.to_lowercase().chars().collect(),
        )
    } else {
        (text.chars().collect(), pattern.chars().collect())
    };
    fn go(t: &[char], p: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('%') => (0..=t.len()).any(|i| go(&t[i..], &p[1..])),
            Some('_') => !t.is_empty() && go(&t[1..], &p[1..]),
            Some(c) => t.first() == Some(c) && go(&t[1..], &p[1..]),
        }
    }
    go(&t, &p)
}

fn json_cmp_op(a: &Json, b: &Json, op: &str) -> Option<bool> {
    let ord = match (a, b) {
        (Json::String(x), Json::String(y)) => x.partial_cmp(y)?,
        _ => a.as_f64()?.partial_cmp(&b.as_f64()?)?,
    };
    Some(match op {
        "<" => ord.is_lt(),
        ">" => ord.is_gt(),
        "<=" => ord.is_le(),
        ">=" => ord.is_ge(),
        _ => return None,
    })
}

pub fn agg_expr(func: &str, inner: Expr, table: &str) -> Result<Expr> {
    Ok(match func {
        "sum" => Func::sum(inner).into(),
        "min" => Func::min(inner).into(),
        "max" => Func::max(inner).into(),
        "count" => Func::count(inner).into(),
        "avg" => Expr::cust_with_exprs("AVG($1)::float8", [inner]),
        "count_distinct" => Expr::cust_with_exprs("COUNT(DISTINCT $1)", [inner]),
        "bool_and" => Expr::cust_with_exprs("BOOL_AND($1)", [inner]),
        "bool_or" => Expr::cust_with_exprs("BOOL_OR($1)", [inner]),
        // READ_GROUP_AGGREGATE orders the members by the row id, and the
        // distinct form sorts the distinct values
        "array_agg" => Expr::cust_with_exprs(
            format!("ARRAY_AGG($1 ORDER BY {}.\"id\")", crate::db::ident(table)),
            [inner],
        ),
        "array_agg_distinct" => Expr::cust_with_exprs(
            "(SELECT array_agg(v ORDER BY v) FROM (SELECT DISTINCT unnest(array_agg($1)) AS v) sub)",
            [inner],
        ),
        other => refuse!("unsupported aggregate function {other}"),
    })
}

const NUMBER_GRANULARITIES: [(&str, &str); 10] = [
    ("year_number", "year"),
    ("quarter_number", "quarter"),
    ("month_number", "month"),
    ("iso_week_number", "week"),
    ("day_of_year", "doy"),
    ("day_of_month", "day"),
    ("day_of_week", "dow"),
    ("hour_number", "hour"),
    ("minute_number", "minute"),
    ("second_number", "second"),
];

pub fn is_number_granularity(gran: &str) -> bool {
    NUMBER_GRANULARITIES.iter().any(|(n, _)| *n == gran)
}

pub fn granularity_expr(
    gran: &str,
    inner: Expr,
    is_date: bool,
    tz: Option<&str>,
    week_start: Option<i32>,
) -> Result<Expr> {
    // A datetime groupby buckets in the CONTEXT timezone, not the user's, and
    // a week bucket starts on the language's first weekday: both move rows
    // between groups rather than erroring when they are wrong.
    tracing::debug!(
        target: "odoo_kernel::compile",
        granularity = gran,
        is_date,
        ?tz,
        ?week_start,
        numeric = is_number_granularity(gran),
        "compiling a read_group granularity"
    );
    let inner = match (is_date, tz) {
        (false, Some(tz)) => Expr::cust_with_exprs(
            "timezone($2::text, timezone('UTC', $1))",
            [inner, Expr::val(tz.to_string())],
        ),
        _ => inner,
    };
    if let Some((_, part)) = NUMBER_GRANULARITIES.iter().find(|(n, _)| *n == gran) {
        return Ok(Expr::cust_with_exprs(
            format!("date_part('{part}', $1)"),
            [inner],
        ));
    }
    let e = match gran {
        "hour" | "day" | "month" | "quarter" | "year" => {
            Expr::cust_with_exprs(format!("date_trunc('{gran}', $1::timestamp)"), [inner])
        }
        "week" => {
            let Some(ws) = week_start else {
                refuse!(
                    "a week groupby starts on the language's first weekday, and the \
                     request names no language"
                );
            };
            let first = ws - 1;
            let days = if first == 0 { 0 } else { 7 - first };
            Expr::cust_with_exprs(
                format!(
                    "(date_trunc('week', $1::timestamp - INTERVAL '-{days} DAY') + \
                     INTERVAL '-{days} DAY')"
                ),
                [inner],
            )
        }
        other => refuse!("unsupported granularity {other}"),
    };
    Ok(if is_date {
        Expr::cust_with_exprs("($1)::date", [e])
    } else {
        e
    })
}

fn order_expr(ctx: &ExprCtx, model: &Model, f: &Field, alias: &str) -> Result<Expr> {
    let e = ctx.field_expr(model, f, alias)?;
    Ok(if f.ttype == FieldType::Boolean && !f.not_null {
        Expr::cust_with_exprs("COALESCE($1, FALSE)", [e])
    } else {
        e
    })
}

#[derive(Clone, Debug)]
pub struct OrderJoin {
    pub table: String,
    pub alias: String,
    /// The left side of the join, as an EXPRESSION rather than a column name.
    ///
    /// A column name cannot express a company-dependent many2one: the column is
    /// a `jsonb` keyed by company and the id lives inside it, so joining the
    /// comodel on the raw column asks PostgreSQL for `jsonb = integer` and the
    /// statement dies before it runs. `ExprCtx::field_expr` is what every other
    /// reader of that field already uses.
    pub from: Expr,
}

pub struct OrderItem {
    pub expr: Expr,
    pub order: Order,
    pub nulls: Option<sea_query::NullOrdering>,
    pub joins: Vec<OrderJoin>,
}

pub fn parse_order(
    ctx: &ExprCtx,
    model: &Model,
    alias: &str,
    order: &str,
) -> Result<Vec<OrderItem>> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    order_terms(ctx, model, alias, order, false, &mut seen, &[], &mut out)?;
    tracing::debug!(
        target: "odoo_kernel::compile",
        model = %model.name,
        %order,
        items = out.len(),
        joins = out.iter().map(|i| i.joins.len()).max().unwrap_or(0),
        "parsed the ORDER BY"
    );
    Ok(out)
}

pub fn parse_total_order(
    ctx: &ExprCtx,
    model: &Model,
    alias: &str,
    order: &str,
) -> Result<Vec<OrderItem>> {
    let mut items = parse_order(ctx, model, alias, order)?;
    let names_id = order
        .split(',')
        .any(|part| part.split_whitespace().next() == Some("id"));
    if !names_id {
        items.push(OrderItem {
            expr: col(alias, "id"),
            order: Order::Asc,
            nulls: None,
            joins: Vec::new(),
        });
    }
    Ok(items)
}

pub struct OrderTerm<'t> {
    pub field: &'t str,
    pub desc: bool,
    pub nulls: Option<sea_query::NullOrdering>,
}

pub fn parse_order_term(part: &str) -> Result<Option<OrderTerm<'_>>> {
    let mut words = part.split_whitespace();
    let Some(field) = words.next() else {
        return Ok(None);
    };
    let mut desc = false;
    let mut nulls = None;
    let rest: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
    let mut i = 0;
    if let Some(w) = rest.first() {
        match w.as_str() {
            "desc" => {
                desc = true;
                i = 1;
            }
            "asc" => i = 1,
            _ => {}
        }
    }
    match rest[i..]
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => {}
        ["nulls", "first"] => nulls = Some(sea_query::NullOrdering::First),
        ["nulls", "last"] => nulls = Some(sea_query::NullOrdering::Last),
        other => refuse!("cannot parse order term {part:?} (trailing {other:?})"),
    }
    Ok(Some(OrderTerm { field, desc, nulls }))
}

#[allow(clippy::too_many_arguments)]
fn order_terms(
    ctx: &ExprCtx,
    model: &Model,
    alias: &str,
    order: &str,
    reverse: bool,
    seen: &mut Vec<(String, String)>,
    joins: &[OrderJoin],
    out: &mut Vec<OrderItem>,
) -> Result<()> {
    if !model.order_pure {
        refuse!(
            "{} orders its rows in Python (_order_to_sql / _order_field_to_sql); \
             the kernel cannot reproduce that from `_order`",
            model.name
        );
    }
    for part in order.split(',') {
        let Some(term) = parse_order_term(part)? else {
            continue;
        };
        let desc = term.desc ^ reverse;
        let nulls = match (term.nulls, reverse) {
            (Some(sea_query::NullOrdering::First), true) => Some(sea_query::NullOrdering::Last),
            (Some(sea_query::NullOrdering::Last), true) => Some(sea_query::NullOrdering::First),
            (n, _) => n,
        };
        let dir = if desc { Order::Desc } else { Order::Asc };
        let fname = term.field;

        let field = model
            .fields
            .get(fname)
            .ok_or_else(|| refusal!("unknown order field {}.{fname}", model.name))?;

        if let Some((uid, groups)) = &ctx.access
            && !ctx.registry.field_readable(field, groups)
        {
            tracing::debug!(
                target: "odoo_kernel::sql",
                uid, model = %model.name, field = %fname,
                "ignoring ORDER BY: not readable by user"
            );
            continue;
        }

        if let Some(stand_in) = field.order_by_field.as_deref() {
            let mut stand_in_term = stand_in.to_string();
            if term.desc {
                stand_in_term.push_str(" desc");
            }
            match term.nulls {
                Some(sea_query::NullOrdering::First) => stand_in_term.push_str(" nulls first"),
                Some(sea_query::NullOrdering::Last) => stand_in_term.push_str(" nulls last"),
                None => {}
            }
            order_terms(ctx, model, alias, &stand_in_term, reverse, seen, joins, out)?;
            continue;
        }

        if !field.has_column {
            if field.related.is_none() {
                refuse!("cannot order {} by non-stored {fname}", model.name);
            }
            let path = ctx
                .normalize_path(model, std::slice::from_ref(&field.name))
                .with_context(|| format!("cannot resolve order field {}.{fname}", model.name))?;
            let expr = ctx
                .related_expr_as(alias, model, &path, 0, "ord")
                .with_context(|| format!("cannot order {} by related {fname}", model.name))?;
            out.push(OrderItem {
                expr,
                order: dir,
                nulls,
                joins: joins.to_vec(),
            });
            continue;
        }

        if field.ttype != FieldType::Many2one {
            out.push(OrderItem {
                expr: order_expr(ctx, model, field, alias)?,
                order: dir,
                nulls,
                joins: joins.to_vec(),
            });
            continue;
        }

        let key = (model.name.clone(), fname.to_string());
        if seen.contains(&key) {
            continue;
        }
        let comodel = ctx.registry.get(field.comodel()?)?;
        // NOT the raw column: a company-dependent many2one keeps its id inside
        // a jsonb, and both the ORDER BY term and the join below have to read
        // it the way the rest of the compiler does
        let fk = order_expr(ctx, model, field, alias)?;

        if comodel.order.trim() == "id" {
            out.push(OrderItem {
                expr: fk,
                order: dir,
                nulls,
                joins: joins.to_vec(),
            });
            continue;
        }

        if let Some(n) = nulls {
            let null_first = matches!(n, sea_query::NullOrdering::First);
            let e = if null_first {
                fk.clone().is_not_null()
            } else {
                fk.clone().is_null()
            };
            out.push(OrderItem {
                expr: e,
                order: Order::Asc,
                nulls: None,
                joins: joins.to_vec(),
            });
        }

        // __m2o_order_seen is scoped to the recursion path: the same field
        // reached through two chains sorts once per chain
        seen.push(key);
        // ordering by a many2one whose comodel is not ordered by id means a
        // LEFT JOIN per hop, and the comodel's own _order recurses -- this is
        // where an order clause stops being free
        tracing::debug!(
            target: "odoo_kernel::compile",
            model = %model.name,
            field = %fname,
            comodel = %comodel.name,
            comodel_order = %comodel.order,
            depth = joins.len(),
            "ordering through a many2one; joining the comodel"
        );
        // Postgres truncates identifiers at 63 bytes, where two long chains
        // would collide; a long alias keeps its field and hashes its parent
        let join_alias = {
            let full = format!("{alias}__{fname}");
            if full.len() <= 56 {
                full
            } else {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                alias.hash(&mut h);
                format!(
                    "o{:08x}__{}",
                    h.finish() as u32,
                    &fname[..fname.len().min(40)]
                )
            }
        };
        let mut chain = joins.to_vec();
        chain.push(OrderJoin {
            table: comodel.table.clone(),
            alias: join_alias.clone(),
            from: fk.clone(),
        });
        order_terms(
            ctx,
            comodel,
            &join_alias,
            &comodel.order,
            desc,
            seen,
            &chain,
            out,
        )
        .with_context(|| {
            format!(
                "cannot order {} by {} through {fname}",
                model.name, comodel.name
            )
        })?;
        seen.pop();
    }
    Ok(())
}

#[cfg(test)]
mod like_tests {
    use super::sql_like;

    #[test]
    fn like_matches_the_way_postgres_does() {
        assert!(sql_like("abc", "%b%", false));
        assert!(!sql_like("abc", "%B%", false));
        assert!(sql_like("abc", "%B%", true));
        assert!(sql_like("abc", "a_c", false));
        assert!(!sql_like("abc", "a_", false));
        assert!(sql_like("", "%", false));
        assert!(!sql_like("", "_", false));
        assert!(sql_like("héllo", "h_llo", false));
    }
}
