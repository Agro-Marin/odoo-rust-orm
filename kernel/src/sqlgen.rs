use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, NaiveDateTime};
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

pub struct ExprCtx<'a> {
    pub registry: &'a Registry,

    pub dynamic: std::sync::Arc<crate::registry::Dynamic>,
    pub lang: &'a str,
    pub company_id: i32,

    pub access: Option<(i32, std::sync::Arc<std::collections::HashSet<i32>>)>,

    pub active_test: bool,
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
        }
    }

    pub fn with_access(
        mut self,
        uid: i32,
        groups: std::sync::Arc<std::collections::HashSet<i32>>,
    ) -> Self {
        self.access = Some((uid, groups));
        self
    }

    pub fn lang_extract(&self, e: Expr) -> Expr {
        if self.lang == "en_US" {
            Expr::cust_with_exprs("$1 ->> 'en_US'", [e])
        } else {
            Expr::cust_with_exprs(
                format!("COALESCE($1 ->> '{}', $2 ->> 'en_US')", self.lang),
                [e.clone(), e],
            )
        }
    }

    pub fn field_expr(&self, model: &Model, f: &Field, alias: &str) -> Result<Expr> {
        if !f.has_column {
            bail!("field {}.{} has no column", model.name, f.name);
        }
        let raw = col(alias, &f.name);
        if f.company_dependent {
            let ty = pg_cast_type(f.ttype);

            let fallback = self
                .dynamic
                .defaults
                .get(model.name.as_str())
                .and_then(|m| m.get(f.name.as_str()))
                .and_then(|d| d.fallback(self.company_id))
                .cloned()
                .or_else(|| f.cd_fallback.clone().filter(|v| to_value(f, v).is_ok()));
            let coalesced = match fallback {
                Some(v) => {
                    let param = to_value(f, &v)?;
                    Expr::cust_with_exprs(
                        format!("COALESCE($1 -> '{}', to_jsonb($2::{ty}))", self.company_id),
                        [raw, Expr::val(param)],
                    )
                }
                None => Expr::cust_with_exprs(
                    format!(
                        "COALESCE($1 -> '{}', to_jsonb(NULL::{ty}))",
                        self.company_id
                    ),
                    [raw],
                ),
            };
            return Ok(match f.ttype {
                FieldType::Boolean
                | FieldType::Integer
                | FieldType::Float
                | FieldType::Monetary => Expr::cust_with_exprs(format!("($1)::{ty}"), [coalesced]),
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
            bail!("field {}.{} is not readable", model.name, f.name);
        }
        let e = self.field_expr(model, f, alias)?;
        if f.pg_type == "numeric" {
            Ok(e.cast_as("float8"))
        } else {
            Ok(e)
        }
    }

    pub fn normalize_path(&self, model: &Model, path: &[String]) -> Result<Vec<String>> {
        let mut out: Vec<String> = path.to_vec();
        let mut model_name = model.name.clone();
        let mut i = 0usize;
        let mut guard = 0;
        while i < out.len() {
            guard += 1;
            if guard > 64 {
                bail!("related expansion loop at {}.{:?}", model.name, path);
            }
            let m = self.registry.get(&model_name)?;
            let field = m
                .fields
                .get(&out[i])
                .ok_or_else(|| anyhow::anyhow!("unknown field {}.{}", model_name, out[i]))?;
            if !field.has_column {
                if let Some(rel) = &field.related {
                    let expansion: Vec<String> = rel.split('.').map(str::to_string).collect();
                    out.splice(i..=i, expansion);
                    continue;
                }
                if !matches!(field.ttype, FieldType::One2many | FieldType::Many2many) {
                    bail!("cannot traverse non-stored {}.{}", model_name, out[i]);
                }
            }
            if i + 1 < out.len() {
                model_name = field.relation.clone().ok_or_else(|| {
                    anyhow::anyhow!("{}.{} is not relational", model_name, out[i])
                })?;
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
                .ok_or_else(|| anyhow::anyhow!("unknown field {}.{seg}", m.name))?;
            m = self.registry.get(
                f.relation
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("{}.{seg} is not relational", m.name))?,
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
        let field = model
            .fields
            .get(&path[0])
            .ok_or_else(|| anyhow::anyhow!("unknown field {}.{}", model.name, path[0]))?;
        if path.len() == 1 {
            return self.read_expr(model, field, alias);
        }
        if field.ttype != FieldType::Many2one || !field.has_column {
            bail!(
                "related path hop {}.{} is not a stored many2one",
                model.name,
                field.name
            );
        }
        let co = self.registry.get(field.comodel()?)?;
        let sub_alias = format!("r{depth}_{}", co.table);
        let inner = self.related_expr(&sub_alias, co, &path[1..], depth + 1)?;
        let mut select = sea_query::Query::select();
        select
            .expr(inner)
            .from_as(Alias::new(&co.table), Alias::new(sub_alias.as_str()))
            .and_where(col(&sub_alias, "id").eq(col(alias, &field.name)));
        Ok(subquery(select))
    }
}

fn to_value(f: &Field, v: &Json) -> Result<Value> {
    let err = || anyhow::anyhow!("cannot convert {v} for {} field {}", f.pg_type, f.name);
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
            Value::from(NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(err)?)
        }
        FieldType::Datetime => {
            let s = v.as_str().ok_or_else(err)?;
            let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
                .or_else(|_| {
                    NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
                })
                .with_context(err)?;
            Value::from(dt)
        }
        _ => return Err(err()),
    })
}

const IN_TO_ANY_THRESHOLD: usize = 100;

fn in_or_any(sql_field: Expr, op: &str, vals: Vec<Value>) -> Expr {
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

pub struct Compiler<'a> {
    pub ctx: &'a ExprCtx<'a>,
    pub model: &'a Model,

    pub alias: String,

    pub rules: &'a crate::security::RuleSet,
    pub su: bool,
    pub depth: usize,

    pub stack: Vec<String>,
}

impl<'a> Compiler<'a> {
    pub fn root(
        ctx: &'a ExprCtx<'a>,
        model: &'a Model,
        rules: &'a crate::security::RuleSet,
        su: bool,
    ) -> Self {
        Compiler {
            ctx,
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
            ctx: self.ctx,
            model: co,
            alias,
            rules: self.rules,
            su: self.su,
            depth: self.depth + 1,
            stack,
        }
    }

    const MAX_DEPTH: usize = 32;

    pub fn compile(&self, node: &Node) -> Result<Condition> {
        if self.depth > Self::MAX_DEPTH {
            bail!(
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
            other => bail!("cannot negate the operator {other:?} on {}", leaf.field),
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
            .ok_or_else(|| anyhow::anyhow!("unknown field {}", leaf.field))?;
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

    fn compile_leaf(&self, leaf: &Leaf) -> Result<Expr> {
        let raw_path: Vec<String> = leaf.field.split('.').map(str::to_string).collect();

        if raw_path.len() == 1 && raw_path[0] == "display_name" {
            return self.display_name_condition(self.model, &leaf.op, &leaf.value);
        }

        let path = self.ctx.normalize_path(self.model, &raw_path)?;

        if path.len() > 1 {
            return self.dotted(&path, &leaf.op, &leaf.value);
        }
        let field = self
            .model
            .fields
            .get(&path[0])
            .ok_or_else(|| anyhow::anyhow!("unknown field {}.{}", self.model.name, path[0]))?;

        if field.custom_search {
            bail!(
                "{}.{} defines a custom search method; its domain cannot be \
                 compiled from the column",
                self.model.name,
                field.name
            );
        }

        if field.ttype == FieldType::Many2one && leaf.op.contains("like") {
            return self.m2o_name_search(field, &leaf.op, &leaf.value);
        }

        if field.ttype == FieldType::Boolean && matches!(leaf.op.as_str(), "<" | "<=" | ">" | ">=")
        {
            bail!(
                "operator {:?} is not supported on the boolean field {}.{}",
                leaf.op,
                self.model.name,
                field.name
            );
        }

        if let Some((uid, groups)) = &self.ctx.access
            && !self.ctx.registry.field_readable(field, groups)
        {
            bail!(
                "uid {uid} may not filter on {}.{}: the field is restricted to {:?}",
                self.model.name,
                field.name,
                field.groups.as_deref().unwrap_or(".")
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
                return self.m2o_any(
                    field,
                    &sub,
                    !leaf.op.starts_with("not"),
                    field.bypass_search_access,
                );
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
                    bail!("'{}' operator expects a list, got {}", leaf.op, leaf.value);
                };
                (leaf.op.as_str(), items.clone())
            }
            "child_of" | "parent_of" => {
                bail!("hierarchy operator {} must be pre-resolved", leaf.op)
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
            _ if field.ttype == FieldType::Boolean => bail!(
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
            other => bail!("unsupported operator {other:?}"),
        }?;
        let cond = match self.trigram_accelerator_for_value(field, &values) {
            Some(accelerator) if op == "in" => accelerator.and(cond),
            _ => cond,
        };
        Ok(self.company_dependent_guard(field, op, &values, cond))
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
        let Some(fallback) = field.cd_fallback.as_ref() else {
            return col(&self.alias, &field.name).is_not_null().and(cond);
        };
        let satisfied = match op {
            "in" => values.iter().any(|v| json_eq(v, fallback)),
            "not in" => !values.iter().any(|v| json_eq(v, fallback)),
            "<" | ">" | "<=" | ">=" => values
                .first()
                .and_then(|v| json_cmp_op(fallback, v, op))
                .unwrap_or(true),

            _ => true,
        };
        if satisfied {
            cond
        } else {
            col(&self.alias, &field.name).is_not_null().and(cond)
        }
    }

    fn dotted(&self, path: &[String], op: &str, value: &Json) -> Result<Expr> {
        let head = self
            .model
            .fields
            .get(&path[0])
            .ok_or_else(|| anyhow::anyhow!("unknown field {}.{}", self.model.name, path[0]))?;
        let sub_leaf = Node::Leaf(Leaf {
            field: path[1..].join("."),
            op: op.to_string(),
            value: value.clone(),
        });
        match head.ttype {
            FieldType::Many2one => self.m2o_any(head, &sub_leaf, true, head.bypass_search_access),
            FieldType::One2many | FieldType::Many2many => {
                self.x2many_subselect(head, Some(&sub_leaf), true, true, head.bypass_search_access)
            }
            _ => bail!(
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
            bail!(
                "x2many {}.{} cannot be read from an ir_model bootstrap registry: \
                 field-level domains are not recorded there. Build the registry \
                 from a live-registry export.",
                owner.name,
                field.name
            );
        }
        if field.domain_callable {
            bail!(
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
            let inverse = field.inverse_column()?;
            if let Some(inv) = co.fields.get(inverse)
                && inv.ttype == FieldType::Many2oneReference
            {
                let model_field = inv.model_field.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}.{} is a many2one_reference with no model_field; a \
                             one2many over it would read other models' rows",
                        co.name,
                        inverse
                    )
                })?;
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
            return Ok(Cond::all());
        }
        Ok(Cond::all().add(col(&self.alias, active_name).is_in([true])))
    }

    /// The comodel's record rules to AND into a subquery, if any apply.
    ///
    /// `bypass` is the field's `bypass_search_access`, and Odoo's answer to
    /// it is total: `_optimize_any_with_rights` rewrites the condition to
    /// `any!`, and `_search(bypass_access=True)` skips the comodel's ACL
    /// check AND its record rules. The `_search` purity check stays either
    /// way -- a bypass does not make a Python `_search` reproducible.
    fn comodel_rules(&self, co: &'a Model, bypass: Option<bool>) -> Result<Option<&'a Node>> {
        if !co.search_pure {
            bail!(
                "the subquery traverses {}, which defines `_search` in Python",
                co.name
            );
        }
        if bypass == Some(true) {
            return Ok(None);
        }
        if self.su {
            return Ok(None);
        }

        if self.stack.contains(&co.name) {
            bail!(
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
        if bypass.is_none() && rules.is_some() {
            bail!(
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
        if !op.contains("like") {
            bail!(
                "display_name `{op}` on {}: only the like family is compiled from \
                 the name columns",
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
            bail!(
                "{} searches `display_name` to answer `display_name`; Odoo breaks \
                 that loop in Python and this kernel refuses it",
                co.name
            );
        }
        let Some(fnames) = co.name_search_fields.as_deref() else {
            bail!(
                "{} defines its display name in Python (_search_display_name, \
                 _rec_names_search or a relational _rec_name); the columns \
                 cannot express a search on it",
                co.name
            );
        };
        let negative = op.starts_with("not ");

        let empty = matches!(value, Json::String(s) if s.is_empty())
            || matches!(value, Json::Bool(false) | Json::Null);
        if empty && !op.contains('=') {
            return Ok(if negative { Node::False } else { Node::True });
        }
        let mut terms = Vec::with_capacity(fnames.len());
        for fname in fnames {
            terms.push(Node::Leaf(Leaf {
                field: fname.clone(),
                op: op.to_string(),
                value: value.clone(),
            }));
        }
        Ok(match (terms.len(), negative) {
            (1, _) => terms.pop().unwrap(),
            (_, true) => Node::And(terms),
            (_, false) => Node::Or(terms),
        })
    }

    fn m2o_any(
        &self,
        field: &Field,
        sub_node: &Node,
        positive: bool,
        bypass: Option<bool>,
    ) -> Result<Expr> {
        let co = self.ctx.registry.get(field.comodel()?)?;
        let sub_alias = format!("s{}_{}", self.depth, co.table);
        let sub_compiler = self.sub_compiler(co, sub_alias.clone());
        let mut cond = Cond::all().add(sub_compiler.compile(sub_node)?);
        if let Some(rules) = self.comodel_rules(co, bypass)? {
            cond = cond.add(sub_compiler.compile(rules)?);
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
                self.x2many_subselect(
                    field,
                    Some(&sub),
                    !op.starts_with("not"),
                    true,
                    field.bypass_search_access,
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
                        bail!(
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
                        field.bypass_search_access,
                    )?)
                };
                let empty = match match_empty {
                    true => Some(self.x2many_subselect(
                        field,
                        None,
                        false,
                        false,
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
            _ => bail!("unsupported x2many operator {op}"),
        }
    }

    fn x2many_subselect(
        &self,
        field: &Field,
        sub_node: Option<&Node>,
        positive: bool,
        apply_active: bool,
        bypass: Option<bool>,
    ) -> Result<Expr> {
        let co = self.ctx.registry.get(field.comodel()?)?;
        let sub_alias = format!("s{}_{}", self.depth, co.table);
        let sub_compiler = self.sub_compiler(co, sub_alias.clone());
        let field_cond = Self::field_domain_cond(&sub_compiler, field, co, self.model)?;
        let mut cond = Cond::all().add(field_cond);
        if let Some(node) = sub_node {
            cond = cond.add(sub_compiler.compile(node)?);

            if apply_active {
                cond = cond.add(sub_compiler.active_filter(field, co, Some(node))?);
            }
        }
        if let Some(rules) = self.comodel_rules(co, bypass)? {
            cond = cond.add(sub_compiler.compile(rules)?);
        }

        let mut select = sea_query::Query::select();
        match field.ttype {
            FieldType::One2many => {
                let inverse = field.o2m_inverse_column(&self.model.name, co)?;
                select
                    .expr(col(&sub_alias, inverse))
                    .from_as(Alias::new(&co.table), Alias::new(sub_alias.as_str()))
                    .and_where(col(&sub_alias, inverse).is_not_null())
                    .cond_where(cond);
            }
            FieldType::Many2many => {
                let (rel, c1, c2) = field.m2m_columns()?;
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
        sql.ok_or_else(|| anyhow::anyhow!("missing sql for {op} {values:?}"))
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
            [col(&self.alias, &field.name)],
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
            Json::Number(n) => n.to_string(),
            other => bail!("invalid value for {op}: {other}"),
        };
        // Odoo's `_optimize_like_str`: an EMPTY pattern is not a LIKE at all.
        // `like ''` is every row, NULLs included, where `col LIKE '%%'` drops
        // them; `not like ''` is no row, where `col NOT LIKE '%%' OR col IS
        // NULL` is every NULL one -- a filter that fails OPEN. The `=`-forms
        // and relational fields turn into a set-ness test on the column
        // instead, and a pattern that is only `%` is the positive case of the
        // same two answers.
        let negative = op.starts_with("not ");
        let eq_like = op.contains('=');
        let relational = matches!(
            field.ttype,
            FieldType::Many2one | FieldType::One2many | FieldType::Many2many
        );
        if raw.is_empty() {
            let result = negative == eq_like;
            if relational || eq_like {
                return self.compile_leaf(&Leaf {
                    field: field.name.clone(),
                    op: (if result { "!=" } else { "=" }).to_string(),
                    value: Json::Bool(false),
                });
            }
            return Ok(Expr::cust(if result { "TRUE" } else { "FALSE" }));
        }
        if raw.chars().all(|c| c == '%') {
            let result = !negative;
            if relational {
                return self.compile_leaf(&Leaf {
                    field: field.name.clone(),
                    op: (if result { "!=" } else { "=" }).to_string(),
                    value: Json::Bool(false),
                });
            }
            return Ok(Expr::cust(if result { "TRUE" } else { "FALSE" }));
        }

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
        let falsy = field.falsy_json();
        if is_null_like(value) && falsy.is_none() {
            // `_optimize_inequality_against_null`: with nothing to stand in
            // for the unset value there is nothing to order against, and
            // Odoo folds the condition to FALSE rather than comparing.
            return Ok(Expr::cust("FALSE"));
        }
        let mut value = value.clone();
        let mut accept_null = false;
        if let Some(falsy) = falsy {
            if is_null_like(&value) {
                value = falsy.clone();
            }
            accept_null = can_be_null && json_cmp_op(&falsy, &value, op).unwrap_or(false);
        }
        let v = to_value(field, &value)?;
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

pub fn agg_expr(func: &str, inner: Expr) -> Result<Expr> {
    Ok(match func {
        "sum" => Func::sum(inner).into(),
        "min" => Func::min(inner).into(),
        "max" => Func::max(inner).into(),
        "count" => Func::count(inner).into(),
        "avg" => Expr::cust_with_exprs("AVG($1)::float8", [inner]),
        "count_distinct" => Expr::cust_with_exprs("COUNT(DISTINCT $1)", [inner]),
        other => bail!("unsupported aggregate function {other}"),
    })
}

pub fn granularity_expr(gran: &str, inner: Expr, is_date: bool) -> Result<Expr> {
    if !matches!(gran, "day" | "month" | "quarter" | "year") {
        bail!("unsupported granularity {gran}");
    }
    let e = Expr::cust_with_exprs(format!("date_trunc('{gran}', $1::timestamp)"), [inner]);
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
    pub from_alias: String,
    pub from_col: String,
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
    Ok(out)
}

struct OrderTerm<'t> {
    field: &'t str,
    desc: bool,
    nulls: Option<sea_query::NullOrdering>,
}

fn parse_order_term(part: &str) -> Result<Option<OrderTerm<'_>>> {
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
        other => bail!("cannot parse order term {part:?} (trailing {other:?})"),
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
            .ok_or_else(|| anyhow::anyhow!("unknown order field {}.{fname}", model.name))?;

        if !field.has_column {
            if field.related.is_none() {
                bail!("cannot order {} by non-stored {fname}", model.name);
            }
            let path = ctx
                .normalize_path(model, std::slice::from_ref(&field.name))
                .with_context(|| format!("cannot resolve order field {}.{fname}", model.name))?;
            let expr = ctx
                .related_expr(alias, model, &path, 16)
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
        let fk = col(alias, &field.name);

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

        seen.push(key);
        let join_alias = format!("ord_{}_{}", joins.len(), fname);
        let mut chain = joins.to_vec();
        chain.push(OrderJoin {
            table: comodel.table.clone(),
            alias: join_alias.clone(),
            from_alias: alias.to_string(),
            from_col: field.name.clone(),
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
    }
    Ok(())
}
