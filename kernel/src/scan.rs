use crate::error::{deny_access, refusal, refuse};
use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use chrono::{NaiveDate, NaiveDateTime};
use sea_query::{Alias, Condition, Expr, ExprTrait, JoinType, PostgresQueryBuilder, Query};
use sea_query_postgres::PostgresBinder;
use serde_json::{Value as Json, json};

use crate::orm::{Env, Orm, Request};

use crate::registry::{Field, FieldType, Model};
use crate::security::{self, RuleSet};
use crate::sqlgen::{self, Compiler, ExprCtx, OrderItem, col};

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
        _ => refuse!("field {} of unsupported type for read", field.name),
    })
}

// `_read_group_empty_value`: a NULL cell of a read_group is False for every
// aggregate but a count and for every non-relational group-by, where a read
// of the same column would give the field's falsy value (0, 0.0)
pub(crate) fn decode_group(row: &tokio_postgres::Row, i: usize, kind: &ColKind) -> Result<Json> {
    let null = match kind {
        ColKind::IntZero => match *row.columns()[i].type_() {
            tokio_postgres::types::Type::INT8 => row.try_get::<_, Option<i64>>(i)?.is_none(),
            _ => row.try_get::<_, Option<i32>>(i)?.is_none(),
        },
        ColKind::Float => row.try_get::<_, Option<f64>>(i)?.is_none(),
        ColKind::Bool => row.try_get::<_, Option<bool>>(i)?.is_none(),
        _ => false,
    };
    if null {
        return Ok(json!(false));
    }
    decode(row, i, kind)
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
            Some(d) => json!(
                d.format(if d.and_utc().timestamp_subsec_micros() == 0 {
                    "%Y-%m-%d %H:%M:%S"
                } else {
                    "%Y-%m-%d %H:%M:%S%.6f"
                })
                .to_string()
            ),
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
                // NOT a refusal: the reader built a row wider than the column
                // plan it announced, which is a defect here rather than a
                // capability the kernel lacks. `error` so it survives the
                // default `warn` filter.
                tracing::error!(
                    target: "odoo_kernel::scan",
                    cells = rec.len(),
                    names = names.len(),
                    row = r,
                    "kernel defect: a row carries more cells than the read named columns"
                );
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

// the expression a model's display_name is read from: the declared column(s)
// as an ordered coalesce of non-empty values, else the _rec_name field
pub(crate) fn display_name_expr(
    ctx: &ExprCtx<'_>,
    model: &Model,
    alias: &str,
    env: &Env,
    registry: &crate::registry::Registry,
) -> Result<Option<Expr>> {
    if !model.display_name_column.is_empty() {
        let mut parts = Vec::with_capacity(model.display_name_column.len());
        for name in &model.display_name_column {
            let f = model
                .fields
                .get(name)
                .ok_or_else(|| refusal!("unknown display-name column {}.{name}", model.name))?;
            if !env.su && !registry.field_readable(f, &env.groups) {
                deny_access!(
                    "access denied: uid {} may not read {}.{name}",
                    env.uid,
                    model.name
                );
            }
            parts.push(Expr::cust_with_exprs(
                "NULLIF($1, '')",
                [ctx.read_expr(model, f, alias)?],
            ));
        }
        return Ok(Some(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            let placeholders: Vec<String> = (1..=parts.len()).map(|i| format!("${i}")).collect();
            Expr::cust_with_exprs(format!("COALESCE({})", placeholders.join(", ")), parts)
        }));
    }
    let Some(f) = model.rec_name.as_ref().and_then(|n| model.fields.get(n)) else {
        return Ok(None);
    };
    if ctx.read_expr(model, f, alias).is_err() {
        return Ok(None);
    }
    if !env.su && !registry.field_readable(f, &env.groups) {
        deny_access!(
            "access denied: uid {} may not read {}.{}",
            env.uid,
            model.name,
            f.name
        );
    }
    Ok(Some(ctx.read_expr(model, f, alias)?))
}

pub(crate) fn display_name_cell(model: &str, id: i64, rec_name_value: Option<Json>) -> Json {
    match rec_name_value {
        None => json!(format!("{model},{id}")),
        Some(Json::String(s)) if !s.is_empty() => Json::String(s),
        Some(_) => json!(false),
    }
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
        .ok_or_else(|| refusal!("bad related target for {}.{}", model.name, field.name))
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
                        j.from
                            .clone()
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
            tracing::trace!(
                target: "odoo_kernel::access",
                uid = env.uid,
                field = %field.name,
                groups = field.groups.as_deref().unwrap_or("-"),
                "field is readable by this identity"
            );
            return Ok(());
        }
        deny_access!(
            "access denied: uid {} may not read {} ({}): the field is restricted to {:?}",
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
        let t_rules = std::time::Instant::now();
        let rules = self.uid_rules(req, env).await?;
        let rules_ms = t_rules.elapsed().as_secs_f64() * 1000.0;
        let t_cond = std::time::Instant::now();
        let cond = self
            .build_condition(model, domain_json, env, &rules, req.trusted_domain)
            .await?;
        let cond_ms = t_cond.elapsed().as_secs_f64() * 1000.0;
        let ctx = self.ctx(env);

        let mut sel_fields: Vec<&Field> = Vec::new();
        let mut x2many: Vec<&Field> = Vec::new();
        let mut display_name_from: Option<Option<Expr>> = None;
        for fname in fields {
            if fname == "id" {
                continue;
            }
            if fname == "display_name" {
                if !model.display_name_default {
                    refuse!(
                        "{model_name} computes display_name in Python; the kernel cannot \
                         render it from _rec_name"
                    );
                }
                let source = display_name_expr(&ctx, model, &model.table, env, self.registry)?;
                if source.is_none()
                    && model.rec_name.is_some()
                    && model.display_name_column.is_empty()
                {
                    refuse!("{model_name} has no readable _rec_name to render display_name from");
                }
                display_name_from = Some(source);
                continue;
            }
            let f = model
                .fields
                .get(fname)
                .ok_or_else(|| refusal!("unknown field {model_name}.{fname}"))?;
            self.check_field_access(f, env)?;
            match f.ttype {
                FieldType::One2many | FieldType::Many2many => x2many.push(f),
                _ if f.has_column || f.related.is_some() => sel_fields.push(f),
                _ => refuse!("field {model_name}.{fname} is not stored/readable"),
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
        let display_name_col = display_name_from.as_ref().map(|source| {
            select.expr(match source {
                Some(e) => e.clone(),
                None => Expr::cust("NULL::varchar"),
            });
            kinds.push(("display_name".into(), ColKind::Str));
            (kinds.len() - 1, source.is_some())
        });
        let guard_col = match (&display_name_from, model.display_name_guard.as_deref()) {
            (Some(_), Some(guard)) if env.lang != "en_US" => {
                select.expr(col(&model.table, guard));
                Some(kinds.len())
            }
            _ => None,
        };
        select.cond_where(cond);
        let order_items =
            sqlgen::parse_total_order(&ctx, model, &model.table, order.unwrap_or(&model.order))?;
        Self::apply_order(&mut select, order_items);
        if let Some(l) = limit {
            select.limit(l);
        }
        if offset > 0 {
            select.offset(offset);
        }

        // The column plan: how many columns come out of the main SELECT, and
        // how many fields need a round trip of their own afterwards. An x2many
        // is one extra query each and a many2one label one per comodel, so
        // this is the shape a slow search_read is explained by.
        tracing::debug!(
            target: "odoo_kernel::scan",
            model = %model_name,
            columns = kinds.len(),
            x2many = x2many.len(),
            display_name = display_name_from.is_some(),
            translation_guard = guard_col.is_some(),
            ?limit,
            offset,
            order = order.unwrap_or(&model.order),
            rules_ms,
            cond_ms,
            "search_read plan"
        );
        let t_main = std::time::Instant::now();
        let (sql, values) = select.build_postgres(PostgresQueryBuilder);
        let rows = self.db.query(&sql, &values.as_params()).await?;
        let main_ms = t_main.elapsed().as_secs_f64() * 1000.0;

        let t_decode = std::time::Instant::now();
        let mut cells: Vec<Vec<Json>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut rec = Vec::with_capacity(kinds.len() + x2many.len());
            for (i, (_, kind)) in kinds.iter().enumerate() {
                rec.push(decode(row, i, kind)?);
            }
            if let Some((ci, from_rec_name)) = display_name_col {
                let value = from_rec_name.then(|| rec[ci].take());
                rec[ci] = display_name_cell(&model.name, rec[0].as_i64().unwrap_or(0), value);
            }
            if let Some(gi) = guard_col
                && row
                    .try_get::<_, Option<String>>(gi)?
                    .filter(|s| !s.is_empty())
                    .is_none()
            {
                refuse!(
                    "{model_name} renders display_name from a translated placeholder \
                         for a record whose {} is empty; only en_US reads the stored column",
                    model.display_name_guard.as_deref().unwrap_or("guard")
                );
            }
            cells.push(rec);
        }
        let decode_ms = t_decode.elapsed().as_secs_f64() * 1000.0;

        let mut m2o_columns: std::collections::BTreeMap<String, Vec<usize>> =
            std::collections::BTreeMap::new();
        let mut keep_hidden_ids: BTreeSet<usize> = BTreeSet::new();
        for (fi, f) in sel_fields.iter().enumerate() {
            if req.raw_many2one.contains(&f.name) {
                continue;
            }
            if req.unredacted_many2one.contains(&f.name) {
                keep_hidden_ids.insert(fi + 1);
            }
            if let ColKind::M2o { comodel } = &kinds[fi + 1].1 {
                m2o_columns.entry(comodel.clone()).or_default().push(fi + 1);
            }
        }
        let t_labels = std::time::Instant::now();
        let comodels_labelled = m2o_columns.len();
        for (comodel, columns) in m2o_columns {
            let ids: BTreeSet<i64> = cells
                .iter()
                .flat_map(|r| columns.iter().filter_map(|ci| r[*ci].as_i64()))
                .collect();
            // one query per comodel, not per row: the distinct-id count is
            // what the label query actually costs
            tracing::trace!(
                target: "odoo_kernel::scan",
                %comodel, columns = columns.len(), distinct_ids = ids.len(),
                "resolving many2one labels"
            );
            let names = self.display_names(&comodel, &ids, env, &rules).await?;
            for rec in &mut cells {
                for ci in &columns {
                    if let Some(id) = rec[*ci].as_i64() {
                        rec[*ci] = match names.get(&id) {
                            Some(name) => json!([id, name]),
                            None if keep_hidden_ids.contains(ci) => json!(id),
                            None => json!(false),
                        };
                    }
                }
            }
        }

        let labels_ms = t_labels.elapsed().as_secs_f64() * 1000.0;

        let t_x2many = std::time::Instant::now();
        let parent_ids: Vec<i64> = cells.iter().filter_map(|r| r[0].as_i64()).collect();
        for f in &x2many {
            let by_parent = self.x2many_ids(model, f, &parent_ids, env, &rules).await?;
            for rec in &mut cells {
                let pid = rec[0].as_i64().unwrap();
                rec.push(json!(by_parent.get(&pid).cloned().unwrap_or_default()));
            }
        }

        let x2many_ms = t_x2many.elapsed().as_secs_f64() * 1000.0;

        let names: Vec<&str> = kinds
            .iter()
            .map(|k| k.0.as_str())
            .chain(x2many.iter().map(|f| f.name.as_str()))
            .collect();
        // The phases account for the whole read: main query, decoding its rows
        // into JSON, one label query per comodel, one query per x2many, then
        // the serialisation below. A phase missing from this line is a phase
        // nobody can attribute a slow read to.
        let t_serialise = std::time::Instant::now();
        let json = records_to_json(&names, &cells)?;
        tracing::debug!(
            target: "odoo_kernel::scan",
            model = %model_name,
            rows = cells.len(),
            bytes = json.len(),
            main_ms,
            decode_ms,
            labels_ms,
            comodels_labelled,
            x2many_ms,
            x2many_queries = x2many.len(),
            serialise_ms = t_serialise.elapsed().as_secs_f64() * 1000.0,
            "search_read read"
        );
        Ok(json)
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
        tracing::trace!(
            target: "odoo_kernel::scan",
            comodel = %comodel_name,
            ids = ids.len(),
            access_pure = comodel.display_name_access_pure,
            rec_name = ?comodel.rec_name,
            columns = ?comodel.display_name_column,
            "rendering display names"
        );
        if !comodel.display_name_default {
            refuse!(
                "{comodel_name} computes display_name in Python; the kernel \
                 cannot render it from _rec_name"
            );
        }
        if !env.su && !comodel.check_access_pure {
            refuse!(
                "{comodel_name} decides read access in Python (_check_access); \
                 the kernel cannot tell which of its records uid {} may see named",
                env.uid
            );
        }

        let widened = || {
            refusal!(
                "{comodel_name} widens display_name visibility in Python \
                 (_get_display_name_visible_ids) for records uid {} may not read",
                env.uid
            )
        };
        if !env.su
            && security::check_read_access(&env.dynamic, comodel_name, env.uid, &env.groups)
                .is_err()
        {
            if !comodel.display_name_access_pure {
                return Err(widened());
            }
            return Ok(out);
        }
        let ctx = self.ctx(env);
        let mut select = Query::select();
        select.from(Alias::new(&comodel.table));
        select.expr(col(&comodel.table, "id"));
        let rec_field = display_name_expr(&ctx, comodel, &comodel.table, env, self.registry)?;
        if let Some(e) = &rec_field {
            select.expr(e.clone());
        }
        let guard_col = match (&rec_field, comodel.display_name_guard.as_deref()) {
            (Some(_), Some(guard)) if env.lang != "en_US" => {
                select.expr(col(&comodel.table, guard));
                Some(2usize)
            }
            _ => None,
        };
        select.and_where(sqlgen::id_membership(
            col(&comodel.table, "id"),
            ids.iter().map(|i| *i as i32),
        ));

        if !env.su {
            rules.ensure_evaluated(comodel_name)?;
            if let Some(rule_node) = rules.get(comodel_name) {
                let compiler = Compiler::root(&ctx, comodel, rules, env.su);
                select.cond_where(compiler.compile_rules(rule_node)?);
            }
        }
        let (sql, values) = select.build_postgres(PostgresQueryBuilder);
        for row in self.db.query(&sql, &values.as_params()).await? {
            let id: i32 = row.get(0);
            if let Some(gi) = guard_col
                && row
                    .try_get::<_, Option<String>>(gi)?
                    .filter(|s| !s.is_empty())
                    .is_none()
            {
                refuse!(
                    "{comodel_name} renders display_name from a translated placeholder \
                         for record {id}; only en_US reads the stored column"
                );
            }
            let value = if rec_field.is_some() {
                Some(match row.try_get::<_, Option<String>>(1)? {
                    Some(s) => Json::String(s),
                    None => Json::Null,
                })
            } else {
                None
            };
            out.insert(id as i64, display_name_cell(comodel_name, id as i64, value));
        }
        if !env.su && !comodel.display_name_access_pure && out.len() < ids.len() {
            return Err(widened());
        }
        // fewer names than ids means the record rules hid some corecords;
        // whether Odoo would still show their names is what display_name_access_pure records
        tracing::trace!(
            target: "odoo_kernel::scan",
            comodel = %comodel_name, asked = ids.len(), rendered = out.len(),
            "display names read"
        );
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
            refuse!(
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
        if let Some(active_name) = comodel.active_name.as_deref()
            && field
                .context_active_test()?
                .unwrap_or(env.x2many_active_test)
        {
            field_cond = field_cond.add(col(&comodel.table, active_name).is_in([true]));
        }

        let rule_cond: Option<Condition> = match rules.get(&comodel.name) {
            Some(rule_node) if !env.su => Some(field_compiler.compile_rules(rule_node)?),
            _ => None,
        };

        let pids: Vec<i32> = parent_ids.iter().map(|i| *i as i32).collect();
        let mut select = Query::select();
        match field.ttype {
            FieldType::One2many => {
                let inverse = field.o2m_inverse_column(&owner.name, comodel)?;
                select
                    .from(Alias::new(&comodel.table))
                    .expr(col(&comodel.table, inverse))
                    .expr(col(&comodel.table, "id"))
                    .and_where(sqlgen::id_membership(
                        col(&comodel.table, inverse),
                        pids.iter().copied(),
                    ))
                    .cond_where(field_cond);
                if let Some(rc) = rule_cond {
                    select.cond_where(rc);
                }
                let order_items =
                    sqlgen::parse_total_order(&ctx, comodel, &comodel.table, &comodel.order)?;
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
                    .and_where(sqlgen::id_membership(col(rel, c1), pids.iter().copied()))
                    .cond_where(field_cond);
                if let Some(rc) = rule_cond {
                    select.cond_where(rc);
                }
                let order_items =
                    sqlgen::parse_total_order(&ctx, comodel, &comodel.table, &comodel.order)?;
                Self::apply_order(&mut select, order_items);
            }
            _ => unreachable!(),
        }
        let (sql, values) = select.build_postgres(PostgresQueryBuilder);
        let mut members = 0usize;
        for row in self.db.query(&sql, &values.as_params()).await? {
            let parent: Option<i32> = row.get(0);
            let id: i32 = row.get(1);
            if let Some(p) = parent {
                out.entry(p as i64).or_default().push(id as i64);
                members += 1;
            }
        }
        // one query for every parent at once, ordered by the comodel's _order:
        // the member count is what the field costs to answer
        tracing::debug!(
            target: "odoo_kernel::scan",
            model = %owner.name,
            field = %field.name,
            kind = ?field.ttype,
            comodel = %comodel.name,
            parents = parent_ids.len(),
            members,
            "read an x2many through its relation"
        );
        Ok(out)
    }

    pub async fn search_count(&self, req: &Request, env: &Env) -> Result<String> {
        let (model_name, domain_json, limit) = (&req.model, &req.domain, req.limit);
        let model = self.registry.get(model_name)?;
        let rules = self.uid_rules(req, env).await?;
        let cond = self
            .build_condition(model, domain_json, env, &rules, req.trusted_domain)
            .await?;
        // a limited count is COUNT(*) over a capped subquery, which stops the
        // scan early; an unlimited one counts every matching row
        tracing::debug!(
            target: "odoo_kernel::scan",
            model = %model_name, ?limit, capped = limit.is_some(),
            "search_count plan"
        );
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
                let rows = self.db.query(&sql, &values.as_params()).await?;
                Ok(rows[0].get::<_, i64>(0).to_string())
            }
            None => {
                select.expr(Expr::cust("COUNT(*)"));
                let (sql, values) = select.build_postgres(PostgresQueryBuilder);
                let rows = self.db.query(&sql, &values.as_params()).await?;
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
        if !model.read_group_pure {
            refuse!(
                "{model_name} customises _read_group in Python (a _read_group_* hook); \
                 the kernel cannot reproduce it from the columns"
            );
        }
        let rules = self.uid_rules(req, env).await?;
        let cond = self
            .build_condition(model, domain_json, env, &rules, req.trusted_domain)
            .await?;
        let ctx = self.ctx(env);
        tracing::debug!(
            target: "odoo_kernel::scan",
            model = %model_name,
            groupby = ?groupby,
            aggregates = ?aggregates,
            labels,
            ?limit,
            offset,
            order = ?req.order,
            "read_group plan"
        );

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
            let mut field = model
                .fields
                .get(fname)
                .ok_or_else(|| refusal!("unknown groupby field {model_name}.{fname}"))?;
            self.check_field_access(field, env)?;
            if let Some(stand_in) = field.group_by_field.as_deref() {
                field = model.fields.get(stand_in).ok_or_else(|| {
                    refusal!(
                        "{model_name}.{fname} groups through {stand_in}, which this \
                         registry does not read"
                    )
                })?;
                self.check_field_access(field, env)?;
            }
            gbs.push(GbSpec {
                field,
                granularity: gran,
            });
        }

        let mut select = Query::select();
        select.from(Alias::new(&model.table));
        select.cond_where(cond);

        let mut gb_exprs: Vec<Expr> = Vec::new();
        let mut gb_ordinals: Vec<usize> = Vec::new();
        let group_compiler = Compiler::root(&ctx, model, &rules, env.su);
        for gb in &gbs {
            let base = if gb.field.ttype == FieldType::Many2many {
                if gb.granularity.is_some() {
                    refuse!(
                        "a granularity on the many2many groupby {}.{}",
                        model.name,
                        gb.field.name
                    );
                }
                let (rel, rel_alias, on) = group_compiler.many2many_group_join(gb.field)?;
                select.join_as(
                    JoinType::LeftJoin,
                    Alias::new(rel),
                    Alias::new(rel_alias.as_str()),
                    on,
                );
                let (_, _, c2) = gb.field.m2m_columns()?;
                Expr::col((Alias::new(rel_alias.as_str()), Alias::new(c2)))
            } else {
                ctx.read_expr(model, gb.field, &model.table)?
            };
            let expr = match &gb.granularity {
                Some(g) => sqlgen::granularity_expr(
                    g,
                    base,
                    gb.field.ttype == FieldType::Date,
                    env.tz.as_deref(),
                    env.week_start,
                )?,
                None => {
                    if matches!(gb.field.ttype, FieldType::Date | FieldType::Datetime) {
                        refuse!(
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
            select.expr(expr.clone());

            gb_ordinals.push(gb_exprs.len() + 1);
            gb_exprs.push(expr);
        }

        enum AggKind {
            Count,
            SumInt,
            Float,
            Bool,
            IntArray,
            FloatArray,
            TextArray,
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
                .ok_or_else(|| refusal!("bad aggregate spec {agg}"))?;
            let f = model
                .fields
                .get(fname)
                .ok_or_else(|| refusal!("unknown aggregate field {fname}"))?;
            self.check_field_access(f, env)?;
            let numeric = f.has_column && f.pg_type == "numeric";
            let inner = if numeric {
                ctx.field_expr(model, f, &model.table)?
            } else {
                ctx.read_expr(model, f, &model.table)?
            };
            if numeric && matches!(func, "array_agg" | "array_agg_distinct") {
                refuse!(
                    "{func} over the numeric column {}.{} returns Decimals in Python; \
                     the kernel's float array would not compare equal",
                    model.name,
                    f.name
                );
            }
            let agg = sqlgen::agg_expr(func, inner, &model.table)?;
            let agg = if numeric && matches!(func, "sum" | "min" | "max") {
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
                "bool_and" | "bool_or" if f.ttype == FieldType::Boolean => AggKind::Bool,
                "array_agg" | "array_agg_distinct" => match f.ttype {
                    FieldType::Integer | FieldType::Many2one => AggKind::IntArray,
                    FieldType::Float | FieldType::Monetary => AggKind::FloatArray,
                    t if t.is_text() || t == FieldType::Selection => AggKind::TextArray,
                    other => refuse!(
                        "{func} over a {other:?} field ({}.{}) is not decoded by this kernel",
                        model.name,
                        f.name
                    ),
                },
                other => refuse!(
                    "unsupported aggregate function {other} on {}.{}",
                    model.name,
                    f.name
                ),
            });
        }

        for n in &gb_ordinals {
            select.add_group_by([Expr::cust(n.to_string())]);
        }

        let traverse_many2one = req.order.is_some();
        let explicit: Vec<(String, bool, Option<sea_query::NullOrdering>)> = match &req.order {
            Some(order) => order
                .split(',')
                .filter_map(|part| sqlgen::parse_order_term(part).transpose())
                .map(|t| t.map(|t| (t.field.to_string(), t.desc, t.nulls)))
                .collect::<Result<_>>()?,
            None => groupby
                .iter()
                .map(|spec| (spec.clone(), false, None))
                .collect(),
        };
        let mut order_items: Vec<sqlgen::OrderItem> = Vec::new();
        let mut hidden_columns = 0usize;
        for (spec, desc, nulls) in explicit {
            let direction = if desc {
                sea_query::Order::Desc
            } else {
                sea_query::Order::Asc
            };
            let Some(i) = groupby.iter().position(|g| *g == spec) else {
                if spec == "__count" && !aggregates.iter().any(|a| a == "__count") {
                    select.expr(Expr::cust("COUNT(*)"));
                    let ordinal = gbs.len() + agg_kinds.len() + hidden_columns + 1;
                    hidden_columns += 1;
                    order_items.push(sqlgen::OrderItem {
                        expr: Expr::cust(ordinal.to_string()),
                        order: direction,
                        nulls,
                        joins: Vec::new(),
                    });
                    continue;
                }
                let Some(j) = aggregates.iter().position(|a| *a == spec) else {
                    refuse!(
                        "read_group order term {spec:?} is neither a groupby nor one of \
                         the requested aggregates; Odoo would compute it, this kernel \
                         refuses rather than guess"
                    );
                };
                order_items.push(sqlgen::OrderItem {
                    expr: Expr::cust((gbs.len() + j + 1).to_string()),
                    order: direction,
                    nulls,
                    joins: Vec::new(),
                });
                continue;
            };
            let (gb, n) = (&gbs[i], gb_ordinals[i]);
            let comodel_ordered = gb.field.ttype == FieldType::Many2one
                && gb
                    .field
                    .comodel()
                    .ok()
                    .and_then(|c| self.registry.get(c).ok())
                    .is_some_and(|c| c.order.trim() != "id");
            if traverse_many2one && gb.granularity.is_none() && comodel_ordered {
                // ordering a many2one group by the COMODEL's _order rather
                // than by its id: the join it needs is wrapped in ANY_VALUE
                // so it survives the GROUP BY
                tracing::debug!(
                    target: "odoo_kernel::scan",
                    model = %model_name,
                    field = %gb.field.name,
                    "ordering a many2one groupby through its comodel's own order"
                );
                let mut term = gb.field.name.clone();
                if desc {
                    term.push_str(" desc");
                }
                match nulls {
                    Some(sea_query::NullOrdering::First) => term.push_str(" nulls first"),
                    Some(sea_query::NullOrdering::Last) => term.push_str(" nulls last"),
                    None => {}
                }
                let items = sqlgen::parse_order(&ctx, model, &model.table, &term)?;
                order_items.extend(items.into_iter().map(|item| sqlgen::OrderItem {
                    expr: Expr::cust_with_exprs("ANY_VALUE($1)", [item.expr]),
                    ..item
                }));
            } else if gb.granularity.as_deref() == Some("day_of_week") {
                let Some(ws) = env.week_start else {
                    refuse!(
                        "day_of_week groups are ordered from the language's first \
                         weekday, and the request names no language"
                    );
                };
                let expr = Expr::cust_with_exprs(
                    format!("mod(7 - {ws} + $1::int, 7)"),
                    [gb_exprs[n - 1].clone()],
                );
                select.expr(expr);
                let ordinal = gbs.len() + agg_kinds.len() + hidden_columns + 1;
                hidden_columns += 1;
                select.add_group_by([Expr::cust(ordinal.to_string())]);
                order_items.push(sqlgen::OrderItem {
                    expr: Expr::cust(ordinal.to_string()),
                    order: direction,
                    nulls,
                    joins: Vec::new(),
                });
            } else {
                order_items.push(sqlgen::OrderItem {
                    expr: Expr::cust(n.to_string()),
                    order: direction,
                    nulls,
                    joins: Vec::new(),
                });
            }
        }
        Self::apply_order(&mut select, order_items);
        if let Some(l) = limit.filter(|l| *l > 0) {
            select.limit(l);
        }
        if offset > 0 {
            select.offset(offset);
        }

        let t_main = std::time::Instant::now();
        let (sql, values) = select.build_postgres(PostgresQueryBuilder);
        let rows = self.db.query(&sql, &values.as_params()).await?;
        // `hidden_columns` are the ones only the ORDER BY needs -- a __count
        // nobody asked for, a day_of_week rotation -- and they are grouped by
        // as well, which is what keeps the group set the same as Odoo's
        tracing::debug!(
            target: "odoo_kernel::scan",
            model = %model_name,
            groups = rows.len(),
            group_columns = gbs.len(),
            aggregate_columns = agg_kinds.len(),
            hidden_columns,
            ms = t_main.elapsed().as_secs_f64() * 1000.0,
            "read_group aggregated"
        );

        let t_decode = std::time::Instant::now();
        let mut result: Vec<Vec<Json>> = Vec::new();
        for row in &rows {
            let mut item = Vec::new();
            for (i, gb) in gbs.iter().enumerate() {
                let kind = if let Some(g) = &gb.granularity {
                    if sqlgen::is_number_granularity(g) {
                        ColKind::Float
                    } else {
                        match gb.field.ttype {
                            FieldType::Date => ColKind::Date,
                            _ => ColKind::Datetime,
                        }
                    }
                } else if gb.field.ttype == FieldType::Many2many {
                    ColKind::M2o {
                        comodel: gb.field.relation.clone().unwrap_or_default(),
                    }
                } else {
                    col_kind(final_field(&ctx, model, gb.field)?)?
                };
                item.push(decode_group(row, i, &kind)?);
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
                    // _read_group_empty_value: a NULL aggregate is False, or []
                    AggKind::Bool => json!(row.try_get::<_, Option<bool>>(i)?.unwrap_or(false)),
                    AggKind::IntArray => {
                        json!(
                            row.try_get::<_, Option<Vec<Option<i32>>>>(i)?
                                .unwrap_or_default()
                        )
                    }
                    AggKind::FloatArray => {
                        json!(
                            row.try_get::<_, Option<Vec<Option<f64>>>>(i)?
                                .unwrap_or_default()
                        )
                    }
                    AggKind::TextArray => {
                        json!(
                            row.try_get::<_, Option<Vec<Option<String>>>>(i)?
                                .unwrap_or_default()
                        )
                    }
                    AggKind::ByField(kind) => decode_group(row, i, kind)?,
                };
                item.push(v);
            }
            result.push(item);
        }
        tracing::debug!(
            target: "odoo_kernel::scan",
            model = %model_name,
            groups = result.len(),
            ms = t_decode.elapsed().as_secs_f64() * 1000.0,
            "decoded the group keys and aggregates"
        );

        if labels {
            let t_labels = std::time::Instant::now();
            let mut labelled = 0usize;
            for (i, gb) in gbs.iter().enumerate() {
                let relational = (gb.field.ttype == FieldType::Many2one && gb.field.has_column)
                    || gb.field.ttype == FieldType::Many2many;
                if !relational {
                    continue;
                }
                labelled += 1;
                let comodel = gb.field.comodel()?;
                let pure = self
                    .registry
                    .get(comodel)?
                    .overridden_for("labels")
                    .is_none();
                let ids: BTreeSet<i64> = result.iter().filter_map(|r| r[i].as_i64()).collect();
                if req.groupby_hidden_labels_empty && !pure {
                    refuse!(
                        "{comodel} defines its read path in Python, so the kernel cannot \
                         tell which group labels uid {} may see",
                        env.uid
                    );
                }
                let names = self.display_names(comodel, &ids, env, &rules).await?;
                for r in &mut result {
                    let Some(id) = r[i].as_i64() else { continue };
                    if req.groupby_hidden_labels_empty && !names.contains_key(&id) {
                        r[i] = json!([id, ""]);
                        continue;
                    }
                    let Some(name) = names.get(&id) else {
                        let tail = if pure {
                            "; Python raises AccessError rendering its display name".to_string()
                        } else {
                            format!(
                                ", and {comodel} defines its read path in Python, which may \
                                 widen or narrow that"
                            )
                        };
                        refuse!(
                            "grouping {}.{} by {comodel} {id}, which uid {} may not read{tail}",
                            model.name,
                            gb.field.name,
                            env.uid,
                        );
                    };
                    r[i] = json!([id, name]);
                }
            }
            if labelled > 0 {
                tracing::debug!(
                    target: "odoo_kernel::scan",
                    model = %model_name,
                    many2one_groupbys = labelled,
                    ms = t_labels.elapsed().as_secs_f64() * 1000.0,
                    "rendered the display name of each many2one group key"
                );
            }
        }

        Ok(serde_json::to_string(&result)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{display_name_cell, records_to_json};
    use serde_json::json;

    #[test]
    fn a_falsy_rec_name_renders_display_name_false_as_odoo_does() {
        assert_eq!(
            display_name_cell("m", 7, Some(json!("Acme"))),
            json!("Acme")
        );
        assert_eq!(display_name_cell("m", 7, Some(json!(null))), json!(false));
        assert_eq!(display_name_cell("m", 7, Some(json!(""))), json!(false));
    }

    #[test]
    fn a_model_without_rec_name_renders_its_name_and_id() {
        assert_eq!(
            display_name_cell("res.users.log", 7, None),
            json!("res.users.log,7")
        );
    }

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
