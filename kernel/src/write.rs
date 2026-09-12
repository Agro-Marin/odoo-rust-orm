//! The write statements `odoo/orm/runtime/backend.py` composes, composed here.
//!
//! `PostgresBackend.update_rows` is the bottom of every `write()` in the ORM:
//! the cache flush hands it a column-group and a list of rows, and it renders
//! one `UPDATE`. What it renders is decided entirely by field METADATA --
//! the column's declared cast, whether the field is translated as a whole
//! value, whether it is company-dependent -- which is what the kernel's
//! registry holds. So the statement can be composed here and executed by the
//! caller's own cursor, with the same parameters Python would have passed.
//!
//! The contract is BYTE equality with Python's composition, not equivalence:
//! an equivalent statement that read differently in `--log-sql` would be a
//! second dialect to keep in step. `harness/write_sql_contract.json` holds
//! the text and two tests derive it independently -- `tests/pure.rs` from
//! here, `harness/test_shims.py` from the fork's own `PostgresBackend` -- so
//! neither side is checked against a copy of itself.

use anyhow::Result;

use crate::db::ident;
use crate::error::{refusal, refuse};
use crate::registry::{Field, Registry};

/// Which of the two statements `update_rows` chooses between.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpdateShape {
    /// Every row carries the same values: they are bound once and the ids go
    /// in an `ANY`. `_update_rows_uniform`.
    Uniform,
    /// The rows differ: they are bound as a `VALUES` join.
    /// `_update_rows_values`.
    Values,
}

/// Why a column-group cannot be composed here, in the words the refusal log
/// wants. Each one is a column the DELEGATE still writes correctly.
fn refuse_column(field: &Field) -> Option<String> {
    if !field.has_column || field.column_cast.is_none() {
        return Some(format!(
            "{} has no declared column cast in this registry",
            field.name
        ));
    }
    if field.company_dependent {
        // The assignment interpolates `ir.default`'s per-company fallbacks,
        // which are resolved by the ORM against `res_company` and the
        // defaults a USER can see. Composing it here would mean deciding
        // those from the kernel's own snapshot, and a stale fallback writes
        // the wrong jsonb rather than refusing.
        return Some(format!("{} is company-dependent", field.name));
    }
    None
}

fn assignment(table: &str, field: &Field, expr: &str) -> String {
    let column = ident(&field.name);
    if field.translate_whole {
        // Merge into the languages already stored, exactly as
        // `_update_assignments` does. The expression appears three times, so
        // its parameter is bound three times too.
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

/// How many parameters the value of one column contributes to the statement.
///
/// A whole-value translated column names its value three times in the merge
/// expression, so it binds three times; everything else binds once. Only the
/// uniform statement has a parameter there at all -- the `VALUES` join names
/// the temporary column instead -- and a group holding a translated column is
/// uniform only when its value is NULL on every row, because the update value
/// is a `PsycopgJson` wrapper and `_UNIFORM_UPDATE_TYPES` does not list it.
pub fn value_repeats(field: &Field) -> usize {
    if field.translate_whole { 3 } else { 1 }
}

/// The statement `update_rows` would run, with `%s` where Python's `SQL`
/// leaves a parameter, so the caller binds the same values in the same order.
///
/// `Values` takes the row tuples flattened, id first. `Uniform` takes each
/// column's value (repeated per [`value_repeats`]) and then the id list.
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
            let row = format!(
                "({})",
                vec!["%s"; fields.len() + 1].join(", ") // the id, then the columns
            );
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

/// The `INSERT` `PostgresBackend.create_rows` runs when it does not take the
/// `COPY` strategy, with `%s` where Python's `SQL` leaves a parameter.
///
/// The values are converted in Python (`convert_to_column_insert` decides a
/// translated or company-dependent column's jsonb), so nothing here depends on
/// what a column holds; what the kernel decides is that every column NAMED is
/// a column this registry knows the table to have. A column added by an
/// upgrade the kernel was not rebuilt for refuses instead of reaching
/// PostgreSQL as an error mid-create.
///
/// No columns is the shape Odoo gives a record created with nothing stored:
/// one `DEFAULT` per row in the id column.
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

/// A statement rendered with PostgreSQL's `$N` placeholders, rewritten into
/// the dialect Odoo's `SQL` object and cursor speak: `%s` in text order, one
/// parameter per occurrence, and every literal `%` doubled so the cursor's
/// printf-style substitution leaves it alone.
///
/// Placeholders are rewritten only outside single-quoted literals, where a
/// `$1` is text: the kernel writes jsonb paths such as `'$.*'` into its SQL,
/// and a rewrite there would change what the path matches.
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

/// A bound value as JSON the Python side revives into the object psycopg
/// would have been handed. Dates and datetimes are tagged, because as bare
/// strings they would reach PostgreSQL as text and compare as text.
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
