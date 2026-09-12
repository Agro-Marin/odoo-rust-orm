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
//! second dialect to keep in step. `harness/update_sql_contract.json` holds
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
