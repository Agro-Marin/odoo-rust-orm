use std::collections::{BTreeSet, HashMap};

use anyhow::{bail, Result};
use chrono::{NaiveDate, NaiveDateTime};
use sea_query::{Alias, Condition, Expr, ExprTrait, JoinType, PostgresQueryBuilder, Query};
use sea_query_postgres::PostgresBinder;
use serde_json::{json, Value as Json};

use crate::orm::{Env, Orm, Request};

use crate::registry::{Field, FieldType, Model};
use crate::security::{self, RuleSet};
use crate::sqlgen::{self, col, Compiler, ExprCtx, OrderItem};

pub(crate) enum ColKind {
    IntZero,
    Float,
    Bool,
    Str,
    Date,
    Datetime,
    M2o { comodel: String },
    JsonRaw,
}

pub(crate) fn col_kind(field: &Field) -> Result<ColKind> {
    Ok(match field.ttype {
        FieldType::Integer | FieldType::Many2oneReference => ColKind::IntZero,
        FieldType::Float | FieldType::Monetary => ColKind::Float,
        FieldType::Boolean => ColKind::Bool,
        FieldType::Char | FieldType::Text | FieldType::Html | FieldType::Selection => ColKind::Str,
        FieldType::Date => ColKind::Date,
        FieldType::Datetime => ColKind::Datetime,
        FieldType::Many2one => ColKind::M2o {
            comodel: field.relation.clone().unwrap_or_default(),
        },
        FieldType::Json => ColKind::JsonRaw,
        _ => bail!("field {} of unsupported type for read", field.name),
    })
}

pub(crate) fn decode(row: &tokio_postgres::Row, i: usize, kind: &ColKind) -> Result<Json> {
    Ok(match kind {
        ColKind::IntZero => match *row.columns()[i].type_() {
            tokio_postgres::types::Type::INT8 => {
                json!(row.try_get::<_, Option<i64>>(i)?.unwrap_or(0))
            }
            _ => json!(row.try_get::<_, Option<i32>>(i)?.unwrap_or(0)),
        },
        ColKind::Float => json!(row.try_get::<_, Option<f64>>(i)?.unwrap_or(0.0)),
        ColKind::Bool => json!(row.try_get::<_, Option<bool>>(i)?.unwrap_or(false)),
        ColKind::Str => match row.try_get::<_, Option<String>>(i)? {
            Some(s) => json!(s),
            None => json!(false),
        },
        ColKind::Date => match row.try_get::<_, Option<NaiveDate>>(i)? {
            Some(d) => json!(d.format("%Y-%m-%d").to_string()),
            None => json!(false),
        },

        ColKind::Datetime => match row.try_get::<_, Option<NaiveDateTime>>(i)? {
            Some(d) => json!(d
                .format(if d.and_utc().timestamp_subsec_micros() == 0 {
                    "%Y-%m-%d %H:%M:%S"
                } else {
                    "%Y-%m-%d %H:%M:%S%.6f"
                })
                .to_string()),
            None => json!(false),
        },
        ColKind::M2o { .. } => match row.try_get::<_, Option<i32>>(i)? {
            Some(id) => json!(id),
            None => json!(false),
        },
        ColKind::JsonRaw => match row.try_get::<_, Option<Json>>(i)? {
            Some(v) => v,
            None => json!(false),
        },
    })
}

pub(crate) fn records_to_json(names: &[&str], cells: &[Vec<Json>]) -> Result<String> {
    let mut frags: Vec<String> = Vec::with_capacity(names.len());
    for (i, name) in names.iter().enumerate() {
        let key = serde_json::to_string(name)?;
        frags.push(format!("{}{}:", if i == 0 { "{" } else { "," }, key));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(cells.len() * 64 + 2);
    buf.push(b'[');
    for (r, rec) in cells.iter().enumerate() {
        if r > 0 {
            buf.push(b',');
        }
        if rec.is_empty() {
            buf.push(b'{');
        }
        for (i, cell) in rec.iter().enumerate() {
            let frag = frags.get(i).ok_or_else(|| {
                anyhow::anyhow!("row has {} cells for {} names", rec.len(), names.len())
            })?;
            buf.extend_from_slice(frag.as_bytes());
            serde_json::to_writer(&mut buf, cell)?;
        }
        buf.push(b'}');
    }
    buf.push(b']');
    Ok(String::from_utf8(buf)?)
}

pub(crate) fn final_field<'r>(
    ctx: &'r ExprCtx<'r>,
    model: &'r Model,
    field: &'r Field,
) -> Result<&'r Field> {
    if field.has_column || field.related.is_none() {
        return Ok(field);
    }
    let path = ctx.normalize_path(model, std::slice::from_ref(&field.name))?;
    let target_model = ctx.path_target(model, &path[..path.len() - 1])?;
    target_model
        .fields
        .get(&path[path.len() - 1])
        .ok_or_else(|| anyhow::anyhow!("bad related target for {}.{}", model.name, field.name))
}
impl<'a> Orm<'a> {
    fn apply_order(select: &mut sea_query::SelectStatement, items: Vec<OrderItem>) {
        let mut joined: BTreeSet<String> = BTreeSet::new();
        for item in items {
            for j in &item.joins {
                if joined.insert(j.alias.clone()) {
                    select.join_as(
                        JoinType::LeftJoin,
                        Alias::new(j.table.as_str()),
                        Alias::new(j.alias.as_str()),
                        col(&j.from_alias, &j.from_col)
                            .equals((Alias::new(j.alias.as_str()), Alias::new("id"))),
                    );
                }
            }
            match item.nulls {
                Some(n) => select.order_by_expr_with_nulls(item.expr, item.order, n),
                None => select.order_by_expr(item.expr, item.order),
            };
        }
    }

    fn check_field_access(&self, field: &Field, env: &Env) -> Result<()> {
        if env.su || self.registry.field_readable(field, &env.groups) {
            return Ok(());
        }
        bail!(
            "uid {} may not read {} ({}): the field is restricted to {:?}",
            env.uid,
            field.name,
            field.pg_type,
            field.groups.as_deref().unwrap_or(".")
        )
    }

    pub async fn search_read(&self, req: &Request, env: &Env) -> Result<String> {
        let (model_name, domain_json, fields) = (&req.model, &req.domain, &req.fields[..]);
        let (offset, limit, order) = (req.offset.unwrap_or(0), req.limit, req.order.as_deref());
        let model = self.registry.get(model_name)?;
        let rules = self.uid_rules(req, env).await?;
        let cond = self
            .build_condition(model, domain_json, env, &rules)
            .await?;
        let ctx = self.ctx(env);

        let mut sel_fields: Vec<&Field> = Vec::new();
        let mut x2many: Vec<&Field> = Vec::new();
        let mut display_name_from: Option<&Field> = None;
        for fname in fields {
            if fname == "id" {
                continue;
            }
            if fname == "display_name" {
                if !model.display_name_default {
                    bail!(
                        "{model_name} computes display_name in Python; the kernel cannot \
                         render it from _rec_name"
                    );
                }
                let rec_field = model
                    .rec_name
                    .as_ref()
                    .and_then(|n| model.fields.get(n))
                    .filter(|f| ctx.read_expr(model, f, &model.table).is_ok())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "{model_name} has no readable _rec_name to render display_name from"
                        )
                    })?;
                self.check_field_access(rec_field, env)?;
                display_name_from = Some(rec_field);
                continue;
            }
            let f = model
                .fields
                .get(fname)
                .ok_or_else(|| anyhow::anyhow!("unknown field {model_name}.{fname}"))?;
            self.check_field_access(f, env)?;
            match f.ttype {
                FieldType::One2many | FieldType::Many2many => x2many.push(f),
                _ if f.has_column || f.related.is_some() => sel_fields.push(f),
                _ => bail!("field {model_name}.{fname} is not stored/readable"),
            }
        }

        let mut select = Query::select();
        select.from(Alias::new(&model.table));
        select.expr(col(&model.table, "id"));
        let mut kinds: Vec<(String, ColKind)> = vec![("id".into(), ColKind::IntZero)];
        for f in &sel_fields {
            select.expr(ctx.read_expr(model, f, &model.table)?);
            kinds.push((f.name.clone(), col_kind(final_field(&ctx, model, f)?)?));
        }
        let display_name_col = display_name_from.map(|f| {
            select.expr(
                ctx.read_expr(model, f, &model.table)
                    .expect("checked above"),
            );
            kinds.push(("display_name".into(), ColKind::Str));
            kinds.len() - 1
        });
        select.cond_where(cond);
        let order_items =
            sqlgen::parse_order(&ctx, model, &model.table, order.unwrap_or(&model.order))?;
        Self::apply_order(&mut select, order_items);
        if let Some(l) = limit {
            select.limit(l);
        }
        if offset > 0 {
            select.offset(offset);
        }

        let (sql, values) = select.build_postgres(PostgresQueryBuilder);
        let rows = self.query_cached(&sql, &values.as_params()).await?;

        let mut cells: Vec<Vec<Json>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut rec = Vec::with_capacity(kinds.len() + x2many.len());
            for (i, (_, kind)) in kinds.iter().enumerate() {
                rec.push(decode(row, i, kind)?);
            }
            if let Some(ci) = display_name_col {
                if !rec[ci].is_string() {
                    rec[ci] = json!(format!("{},{}", model.name, rec[0].as_i64().unwrap_or(0)));
                }
            }
            cells.push(rec);
        }

        for (fi, _f) in sel_fields.iter().enumerate() {
            if let ColKind::M2o { comodel } = &kinds[fi + 1].1 {
                let comodel = comodel.clone();
                let ci = fi + 1;
                let ids: BTreeSet<i64> = cells.iter().filter_map(|r| r[ci].as_i64()).collect();
                let names = self.display_names(&comodel, &ids, env, &rules).await?;
                for rec in &mut cells {
                    if let Some(id) = rec[ci].as_i64() {
                        rec[ci] = match names.get(&id) {
                            Some(name) => json!([id, name]),
                            None => json!(false),
                        };
                    }
                }
            }
        }

        let parent_ids: Vec<i64> = cells.iter().filter_map(|r| r[0].as_i64()).collect();
        for f in &x2many {
            let by_parent = self.x2many_ids(model, f, &parent_ids, env, &rules).await?;
            for rec in &mut cells {
                let pid = rec[0].as_i64().unwrap();
                rec.push(json!(by_parent.get(&pid).cloned().unwrap_or_default()));
            }
        }

        let names: Vec<&str> = kinds
            .iter()
            .map(|k| k.0.as_str())
            .chain(x2many.iter().map(|f| f.name.as_str()))
            .collect();
        records_to_json(&names, &cells)
    }

    async fn display_names(
        &self,
        comodel_name: &str,
        ids: &BTreeSet<i64>,
        env: &Env,
        rules: &RuleSet,
    ) -> Result<HashMap<i64, Json>> {
        let mut out = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let comodel = self.registry.get(comodel_name)?;
        if !comodel.display_name_default {
            bail!(
                "{comodel_name} computes display_name in Python; the kernel \
                 cannot render it from _rec_name"
            );
        }

        if !env.su
            && security::check_read_access(&env.dynamic, comodel_name, env.uid, &env.groups)
                .is_err()
        {
            return Ok(out);
        }
        let ctx = self.ctx(env);
        let mut select = Query::select();
        select.from(Alias::new(&comodel.table));
        select.expr(col(&comodel.table, "id"));
        let rec_field = comodel
            .rec_name
            .as_ref()
            .and_then(|n| comodel.fields.get(n))
            .filter(|f| ctx.read_expr(comodel, f, &comodel.table).is_ok());
        if let Some(f) = rec_field {
            select.expr(ctx.read_expr(comodel, f, &comodel.table)?);
        }
        select.and_where(col(&comodel.table, "id").is_in(ids.iter().map(|i| *i as i32)));

        if !env.su {
            rules.ensure_evaluated(comodel_name)?;
            if let Some(rule_node) = rules.get(comodel_name) {
                let compiler = Compiler::root(&ctx, comodel, rules, env.su);
                select.cond_where(compiler.compile(rule_node)?);
            }
        }
        let (sql, values) = select.build_postgres(PostgresQueryBuilder);
        for row in self.query_cached(&sql, &values.as_params()).await? {
            let id: i32 = row.get(0);
            let name: Json = if rec_field.is_some() {
                match row.try_get::<_, Option<String>>(1)? {
                    Some(s) => json!(s),
                    None => json!(format!("{},{}", comodel_name, id)),
                }
            } else {
                json!(format!("{},{}", comodel_name, id))
            };
            out.insert(id as i64, name);
        }
        Ok(out)
    }

    async fn x2many_ids(
        &self,
        owner: &Model,
        field: &Field,
        parent_ids: &[i64],
        env: &Env,
        rules: &RuleSet,
    ) -> Result<HashMap<i64, Vec<i64>>> {
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        if parent_ids.is_empty() {
            return Ok(out);
        }
        let comodel = self.registry.get(field.comodel()?)?;

        if !comodel.search_pure {
            bail!(
                "{}.{} reads through {}, which defines `_search` in Python; the \
                 relation table is a wider answer",
                owner.name,
                field.name,
                comodel.name
            );
        }
        let ctx = self.ctx(env);
        if !env.su {
            security::check_read_access(&env.dynamic, &comodel.name, env.uid, &env.groups)?;
            rules.ensure_evaluated(&comodel.name)?;
        }

        let field_compiler = Compiler::root(&ctx, comodel, rules, env.su);
        let mut field_cond = Compiler::field_domain_cond(&field_compiler, field, comodel, owner)?;
        if let Some(active_name) = comodel.active_name.as_deref() {
            if field.context_active_test()?.unwrap_or(true) {
                field_cond = field_cond.add(col(&comodel.table, active_name).is_in([true]));
            }
        }

        let rule_cond: Option<Condition> = match rules.get(&comodel.name) {
            Some(rule_node) if !env.su => Some(field_compiler.compile(rule_node)?),
            _ => None,
        };

        let pids: Vec<i32> = parent_ids.iter().map(|i| *i as i32).collect();
        let mut select = Query::select();
        match field.ttype {
            FieldType::One2many => {
                let inverse = field.inverse_column()?;
                select
                    .from(Alias::new(&comodel.table))
                    .expr(col(&comodel.table, inverse))
                    .expr(col(&comodel.table, "id"))
                    .and_where(col(&comodel.table, inverse).is_in(pids))
                    .cond_where(field_cond);
                if let Some(rc) = rule_cond {
                    select.cond_where(rc);
                }
                let order_items =
                    sqlgen::parse_order(&ctx, comodel, &comodel.table, &comodel.order)?;
                Self::apply_order(&mut select, order_items);
            }
            FieldType::Many2many => {
                let (rel, c1, c2) = field.m2m_columns()?;
                select
                    .from(Alias::new(rel))
                    .expr(col(rel, c1))
                    .expr(col(rel, c2))
                    .join(
                        JoinType::InnerJoin,
                        Alias::new(&comodel.table),
                        col(rel, c2).equals((Alias::new(&comodel.table), Alias::new("id"))),
                    )
                    .and_where(col(rel, c1).is_in(pids))
                    .cond_where(field_cond);
                if let Some(rc) = rule_cond {
                    select.cond_where(rc);
                }
                let order_items =
                    sqlgen::parse_order(&ctx, comodel, &comodel.table, &comodel.order)?;
                Self::apply_order(&mut select, order_items);
            }
            _ => unreachable!(),
        }
        let (sql, values) = select.build_postgres(PostgresQueryBuilder);
        for row in self.query_cached(&sql, &values.as_params()).await? {
            let parent: Option<i32> = row.get(0);
            let id: i32 = row.get(1);
            if let Some(p) = parent {
                out.entry(p as i64).or_default().push(id as i64);
            }
        }
        Ok(out)
    }

    pub async fn search_count(&self, req: &Request, env: &Env) -> Result<String> {
        let (model_name, domain_json, limit) = (&req.model, &req.domain, req.limit);
        let model = self.registry.get(model_name)?;
        let rules = self.uid_rules(req, env).await?;
        let cond = self
            .build_condition(model, domain_json, env, &rules)
            .await?;
        let mut select = Query::select();
        select.from(Alias::new(&model.table)).cond_where(cond);
        match limit {
            Some(l) => {
                select.expr(Expr::cust("1")).limit(l);
                let mut outer = Query::select();
                outer
                    .expr(Expr::cust("COUNT(*)"))
                    .from_subquery(select, Alias::new("capped"));
                let (sql, values) = outer.build_postgres(PostgresQueryBuilder);
                let rows = self.query_cached(&sql, &values.as_params()).await?;
                Ok(rows[0].get::<_, i64>(0).to_string())
            }
            None => {
                select.expr(Expr::cust("COUNT(*)"));
                let (sql, values) = select.build_postgres(PostgresQueryBuilder);
                let rows = self.query_cached(&sql, &values.as_params()).await?;
                Ok(rows[0].get::<_, i64>(0).to_string())
            }
        }
    }

    pub async fn read_group(&self, req: &Request, env: &Env) -> Result<String> {
        let (model_name, domain_json) = (&req.model, &req.domain);
        let (offset, limit) = (req.offset.unwrap_or(0), req.limit);
        let labels = req.groupby_labels.unwrap_or(true);
        let groupby = &req.groupby_names()?[..];
        let aggregates = &req.aggregates[..];
        let model = self.registry.get(model_name)?;
        let rules = self.uid_rules(req, env).await?;
        let cond = self
            .build_condition(model, domain_json, env, &rules)
            .await?;
        let ctx = self.ctx(env);

        struct GbSpec<'f> {
            field: &'f Field,
            granularity: Option<String>,
        }
        let mut gbs: Vec<GbSpec> = Vec::new();
        for spec in groupby {
            let (fname, gran) = match spec.split_once(':') {
                Some((f, g)) => (f, Some(g.to_string())),
                None => (spec.as_str(), None),
            };
            let field = model
                .fields
                .get(fname)
                .ok_or_else(|| anyhow::anyhow!("unknown groupby field {model_name}.{fname}"))?;
            self.check_field_access(field, env)?;
            gbs.push(GbSpec {
                field,
                granularity: gran,
            });
        }

        let mut select = Query::select();
        select.from(Alias::new(&model.table));
        select.cond_where(cond);

        let mut gb_exprs: Vec<()> = Vec::new();
        let mut gb_ordinals: Vec<usize> = Vec::new();
        for gb in &gbs {
            let base = ctx.read_expr(model, gb.field, &model.table)?;
            let expr = match &gb.granularity {
                Some(g) => sqlgen::granularity_expr(g, base, gb.field.ttype == FieldType::Date)?,
                None => {
                    if matches!(gb.field.ttype, FieldType::Date | FieldType::Datetime) {
                        bail!(
                            "granularity not set on the date(time) groupby \
                             {}.{}; Odoo requires `{}:day|month|quarter|year`",
                            model.name,
                            gb.field.name,
                            gb.field.name
                        );
                    }

                    if gb.field.ttype.is_text() {
                        Expr::cust_with_exprs("NULLIF($1, '')", [base])
                    } else if gb.field.ttype == FieldType::Boolean {
                        Expr::cust_with_exprs("COALESCE($1, FALSE)", [base])
                    } else {
                        base
                    }
                }
            };
            select.expr(expr);

            gb_ordinals.push(gb_exprs.len() + 1);
            gb_exprs.push(());
        }

        enum AggKind {
            Count,
            SumInt,
            Float,
            ByField(ColKind),
        }
        let mut agg_kinds: Vec<AggKind> = Vec::new();
        for agg in aggregates {
            if agg == "__count" {
                select.expr(Expr::cust("COUNT(*)"));
                agg_kinds.push(AggKind::Count);
                continue;
            }
            let (fname, func) = agg
                .rsplit_once(':')
                .ok_or_else(|| anyhow::anyhow!("bad aggregate spec {agg}"))?;
            let f = model
                .fields
                .get(fname)
                .ok_or_else(|| anyhow::anyhow!("unknown aggregate field {fname}"))?;
            self.check_field_access(f, env)?;
            let numeric = f.has_column && f.pg_type == "numeric";
            let inner = if numeric {
                ctx.field_expr(model, f, &model.table)?
            } else {
                ctx.read_expr(model, f, &model.table)?
            };
            let agg = sqlgen::agg_expr(func, inner)?;
            let agg = if numeric && !matches!(func, "count" | "count_distinct" | "avg") {
                Expr::cust_with_exprs("($1)::float8", [agg])
            } else {
                agg
            };
            select.expr(agg);
            agg_kinds.push(match func {
                "count" | "count_distinct" => AggKind::Count,
                "sum" if f.ttype == FieldType::Integer => AggKind::SumInt,
                "avg" | "sum" => AggKind::Float,
                "min" | "max" => AggKind::ByField(col_kind(final_field(&ctx, model, f)?)?),
                other => bail!("unsupported aggregate function {other}"),
            });
        }

        for n in &gb_ordinals {
            select.add_group_by([Expr::cust(n.to_string())]);
        }

        let traverse_many2one = req.order.is_some();
        let mut order_items: Vec<sqlgen::OrderItem> = Vec::new();
        for (gb, n) in gbs.iter().zip(&gb_ordinals) {
            let comodel_ordered = gb.field.ttype == FieldType::Many2one
                && gb
                    .field
                    .comodel()
                    .ok()
                    .and_then(|c| self.registry.get(c).ok())
                    .is_some_and(|c| c.order.trim() != "id");
            if traverse_many2one && gb.granularity.is_none() && comodel_ordered {
                let items = sqlgen::parse_order(&ctx, model, &model.table, &gb.field.name)?;
                for item in &items {
                    select.add_group_by([item.expr.clone()]);
                }
                order_items.extend(items);
            } else {
                order_items.push(sqlgen::OrderItem {
                    expr: Expr::cust(n.to_string()),
                    order: sea_query::Order::Asc,
                    nulls: None,
                    joins: Vec::new(),
                });
            }
        }
        Self::apply_order(&mut select, order_items);
        if let Some(l) = limit {
            select.limit(l);
        }
        if offset > 0 {
            select.offset(offset);
        }

        let (sql, values) = select.build_postgres(PostgresQueryBuilder);
        let rows = self.query_cached(&sql, &values.as_params()).await?;

        let mut result: Vec<Vec<Json>> = Vec::new();
        for row in &rows {
            let mut item = Vec::new();
            for (i, gb) in gbs.iter().enumerate() {
                let kind = if gb.granularity.is_some() {
                    match gb.field.ttype {
                        FieldType::Date => ColKind::Date,
                        _ => ColKind::Datetime,
                    }
                } else {
                    col_kind(final_field(&ctx, model, gb.field)?)?
                };
                item.push(decode(row, i, &kind)?);
            }
            for (k, ak) in agg_kinds.iter().enumerate() {
                let i = gbs.len() + k;
                let v = match ak {
                    AggKind::Count => json!(row.try_get::<_, Option<i64>>(i)?.unwrap_or(0)),
                    AggKind::SumInt => match row.try_get::<_, Option<i64>>(i)? {
                        Some(x) => json!(x),
                        None => json!(false),
                    },
                    AggKind::Float => match row.try_get::<_, Option<f64>>(i)? {
                        Some(x) => json!(x),
                        None => json!(false),
                    },
                    AggKind::ByField(kind) => decode(row, i, kind)?,
                };
                item.push(v);
            }
            result.push(item);
        }

        if labels {
            for (i, gb) in gbs.iter().enumerate() {
                if gb.field.ttype != FieldType::Many2one || !gb.field.has_column {
                    continue;
                }
                let comodel = gb.field.comodel()?;
                let pure = self.registry.get(comodel)?.read_path_pure;
                let ids: BTreeSet<i64> = result.iter().filter_map(|r| r[i].as_i64()).collect();
                let names = self.display_names(comodel, &ids, env, &rules).await?;
                for r in &mut result {
                    let Some(id) = r[i].as_i64() else { continue };
                    let Some(name) = names.get(&id) else {
                        bail!(
                            "grouping {}.{} by {comodel} {id}, which uid {} may not read{}",
                            model.name,
                            gb.field.name,
                            env.uid,
                            if pure {
                                "; Python raises AccessError rendering its display name"
                            } else {
                                ", and {comodel} defines its read path in Python, which \
                                 may widen or narrow that"
                            }
                        );
                    };
                    r[i] = json!([id, name]);
                }
            }
        }

        Ok(serde_json::to_string(&result)?)
    }
}

#[cfg(test)]
mod tests {
    use super::records_to_json;
    use serde_json::json;

    #[test]
    fn a_row_is_an_object_and_the_set_is_an_array() {
        let out = records_to_json(
            &["id", "name"],
            &[vec![json!(1), json!("a")], vec![json!(2), json!(false)]],
        )
        .unwrap();
        assert_eq!(out, r#"[{"id":1,"name":"a"},{"id":2,"name":false}]"#);
    }

    #[test]
    fn no_rows_is_an_empty_array_not_an_empty_string() {
        assert_eq!(records_to_json(&["id"], &[]).unwrap(), "[]");
    }

    #[test]
    fn a_row_with_no_columns_is_still_an_object() {
        assert_eq!(records_to_json(&[], &[vec![]]).unwrap(), "[{}]");
    }

    #[test]
    fn a_field_name_needing_escapes_is_escaped_once_not_per_row() {
        let out = records_to_json(&["a\"b"], &[vec![json!(1)], vec![json!(2)]]).unwrap();
        assert_eq!(out, r#"[{"a\"b":1},{"a\"b":2}]"#);
    }

    #[test]
    fn the_many2one_and_x2many_shapes_survive_the_buffer() {
        let out = records_to_json(
            &["id", "partner_id", "child_ids"],
            &[vec![json!(1), json!([7, "Acme \"Ltd\""]), json!([3, 4])]],
        )
        .unwrap();
        let back: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back[0]["partner_id"][1], json!("Acme \"Ltd\""));
        assert_eq!(back[0]["child_ids"], json!([3, 4]));
    }

    #[test]
    fn more_cells_than_names_is_an_error_not_a_panic() {
        assert!(records_to_json(&["id"], &[vec![json!(1), json!(2)]]).is_err());
    }
}
