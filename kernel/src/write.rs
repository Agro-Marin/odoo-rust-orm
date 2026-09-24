use anyhow::Result;

use crate::db::ident;
use crate::error::{refusal, refuse};
use crate::registry::{Field, Registry};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpdateShape {
    Uniform,
    Values,
}

fn refuse_column(field: &Field) -> Option<String> {
    if !field.has_column || field.column_cast.is_none() {
        return Some(format!(
            "{} has no declared column cast in this registry",
            field.name
        ));
    }
    if field.company_dependent {
        return Some(format!("{} is company-dependent", field.name));
    }
    None
}

fn assignment(table: &str, field: &Field, expr: &str) -> String {
    let column = ident(&field.name);
    if field.translate_whole {
        format!(
            "{column} = CASE WHEN {expr} IS NULL THEN NULL ELSE
                        COALESCE({table}.{column}, jsonb_build_object(
                            'en_US', jsonb_path_query_first({expr}, '$.*')
                        )) || {expr}
                    END"
        )
    } else {
        format!("{column} = {expr}")
    }
}

pub fn value_repeats(field: &Field) -> usize {
    if field.translate_whole { 3 } else { 1 }
}

pub fn update_rows_sql(
    registry: &Registry,
    model_name: &str,
    fnames: &[String],
    shape: UpdateShape,
    row_count: usize,
) -> Result<String> {
    let model = registry.get(model_name)?;
    if fnames.is_empty() {
        refuse!("update_rows: no columns");
    }
    if row_count == 0 {
        refuse!("update_rows: no rows");
    }
    let table = ident(&model.table);

    let mut fields: Vec<&Field> = Vec::with_capacity(fnames.len());
    for fname in fnames {
        let field = model.fields.get(fname).ok_or_else(|| {
            refusal!("update_rows: {model_name} has no field {fname} in this registry")
        })?;
        if let Some(why) = refuse_column(field) {
            refuse!("update_rows: {why}");
        }
        fields.push(field);
    }

    match shape {
        UpdateShape::Values => {
            let assignments = fields
                .iter()
                .map(|f| {
                    let expr = format!(
                        "\"__tmp\".{}::{}",
                        ident(&f.name),
                        f.column_cast.as_deref().unwrap_or_default()
                    );
                    assignment(&table, f, &expr)
                })
                .collect::<Vec<_>>()
                .join(", ");
            let columns = fields
                .iter()
                .map(|f| ident(&f.name))
                .collect::<Vec<_>>()
                .join(", ");
            let row = format!("({})", vec!["%s"; fields.len() + 1].join(", "));
            let values = vec![row; row_count].join(", ");
            Ok(format!(
                " UPDATE {table}
                SET {assignments}
                FROM (VALUES {values}) AS \"__tmp\"(\"id\", {columns})
                WHERE {table}.\"id\" = \"__tmp\".\"id\"
            "
            ))
        }
        UpdateShape::Uniform => {
            let assignments = fields
                .iter()
                .map(|f| {
                    let expr = format!("%s::{}", f.column_cast.as_deref().unwrap_or_default());
                    assignment(&table, f, &expr)
                })
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!(
                "UPDATE {table} SET {assignments} WHERE \"id\" = ANY(%s)"
            ))
        }
    }
}

pub fn insert_rows_sql(
    registry: &Registry,
    model_name: &str,
    columns: &[String],
    row_count: usize,
) -> Result<String> {
    let model = registry.get(model_name)?;
    if row_count == 0 {
        refuse!("create_rows: no rows");
    }
    let table = ident(&model.table);
    if columns.is_empty() {
        let values = vec!["(DEFAULT)"; row_count].join(", ");
        return Ok(format!(
            "INSERT INTO {table} (\"id\") VALUES {values} RETURNING \"id\""
        ));
    }
    for column in columns {
        let field = model.fields.get(column).ok_or_else(|| {
            refusal!("create_rows: {model_name} has no field {column} in this registry")
        })?;
        if !field.has_column || field.column_cast.is_none() {
            refuse!(
                "create_rows: {} has no declared column cast in this registry",
                field.name
            );
        }
    }
    let names = columns
        .iter()
        .map(|c| ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let row = format!("({})", vec!["%s"; columns.len()].join(", "));
    let values = vec![row; row_count].join(", ");
    Ok(format!(
        "INSERT INTO {table} ({names}) VALUES {values} RETURNING \"id\""
    ))
}

pub fn to_odoo_dialect(
    sql: &str,
    values: &[sea_query::Value],
) -> Result<(String, Vec<serde_json::Value>)> {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut params = Vec::new();
    let mut chars = sql.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = !quoted;
                out.push(c);
            }
            '%' => out.push_str("%%"),
            '$' if !quoted && chars.peek().is_some_and(char::is_ascii_digit) => {
                let mut n = String::new();
                while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
                    n.push(d);
                    chars.next();
                }
                let index: usize = n.parse()?;
                let value = values.get(index.wrapping_sub(1)).ok_or_else(|| {
                    refusal!("placeholder ${index} has no value among {}", values.len())
                })?;
                params.push(value_to_json(value)?);
                out.push_str("%s");
            }
            _ => out.push(c),
        }
    }
    if quoted {
        refuse!("unterminated literal in rendered SQL: {sql}");
    }
    Ok((out, params))
}

fn value_to_json(value: &sea_query::Value) -> Result<serde_json::Value> {
    use sea_query::Value as V;
    use serde_json::{Value as J, json};
    Ok(match value {
        V::Bool(v) => v.map_or(J::Null, J::from),
        V::TinyInt(v) => v.map_or(J::Null, J::from),
        V::SmallInt(v) => v.map_or(J::Null, J::from),
        V::Int(v) => v.map_or(J::Null, J::from),
        V::BigInt(v) => v.map_or(J::Null, J::from),
        V::Double(v) => v.map_or(J::Null, J::from),
        V::Float(v) => v.map_or(J::Null, |f| J::from(f64::from(f))),
        V::String(v) => v.as_ref().map_or(J::Null, |s| J::from(s.as_str())),
        V::ChronoDate(v) => v.map_or(
            J::Null,
            |d| json!({"__date__": d.format("%Y-%m-%d").to_string()}),
        ),
        V::ChronoDateTime(v) => v.map_or(
            J::Null,
            |d| json!({"__datetime__": d.format("%Y-%m-%d %H:%M:%S%.f").to_string()}),
        ),
        V::Array(_, Some(items)) => {
            J::Array(items.iter().map(value_to_json).collect::<Result<_>>()?)
        }
        V::Array(_, None) => J::Null,
        other => refuse!("a bound value of kind {other:?} has no wire form for the port"),
    })
}
