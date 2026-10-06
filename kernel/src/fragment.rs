use crate::error::{refusal, refuse};
use anyhow::Result;
use sea_query::{ArrayType, Expr, Value};
use serde_json::Value as Json;

pub fn is_fragment(value: &Json) -> bool {
    value.as_object().is_some_and(|o| o.contains_key("$sql"))
}

pub fn subselect(value: &Json, nonce: Option<&str>) -> Result<Expr> {
    let object = value
        .as_object()
        .ok_or_else(|| refusal!("a SQL comparand must be an object"))?;
    let expected = nonce.ok_or_else(|| {
        refusal!("a SQL comparand reached a request that carries no nonce for it")
    })?;
    if object.get("$nonce").and_then(Json::as_str) != Some(expected) {
        refuse!("a SQL comparand does not carry this request's nonce");
    }
    let code = object
        .get("$sql")
        .and_then(Json::as_str)
        .ok_or_else(|| refusal!("a SQL comparand's code must be a string"))?;
    let params = match object.get("$params") {
        Some(Json::Array(items)) => items.as_slice(),
        None => &[],
        Some(other) => refuse!("a SQL comparand's params must be a list, got {other}"),
    };
    let values = params.iter().map(param).collect::<Result<Vec<_>>>()?;
    let casts: Vec<&str> = values.iter().map(cast).collect();
    let sql = rewrite(code, &casts)?;
    tracing::trace!(
        target: "odoo_kernel::compile",
        params = values.len(),
        %sql,
        "compiled a SQL comparand Python resolved"
    );
    Ok(Expr::cust_with_values(format!("({sql})"), values))
}

fn param(value: &Json) -> Result<Value> {
    Ok(match value {
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::from(i),
            None if n.is_u64() => refuse!("a SQL comparand's integer {n} is outside int8 range"),
            None => Value::from(
                n.as_f64()
                    .ok_or_else(|| refusal!("a SQL comparand's number {n} is out of range"))?,
            ),
        },
        Json::String(s) => Value::from(s.clone()),
        Json::Array(items) => {
            let values = items.iter().map(param).collect::<Result<Vec<_>>>()?;
            let kind = match values.first() {
                Some(Value::BigInt(_)) => ArrayType::BigInt,
                Some(Value::String(_)) => ArrayType::String,
                _ => refuse!(
                    "a SQL comparand's array parameter {value} is not a list of integers or strings"
                ),
            };
            if values.iter().any(|v| v.array_type() != kind) {
                refuse!("a SQL comparand's array parameter {value} mixes element types");
            }
            Value::Array(kind, Some(Box::new(values)))
        }
        other => refuse!("a SQL comparand's parameter {other} has no typed wire form"),
    })
}

fn cast(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "bool",
        Value::BigInt(_) => "int8",
        Value::Double(_) => "float8",
        Value::Array(ArrayType::BigInt, _) => "int8[]",
        Value::Array(_, _) => "text[]",
        _ => "text",
    }
}

fn rewrite(code: &str, casts: &[&str]) -> Result<String> {
    use crate::sqllex::{Kind, opaque};
    let mut out = String::with_capacity(code.len() + casts.len() * 8);
    let mut used = 0;
    let mut i = 0;
    while i < code.len() {
        if let Some(token) = opaque(code.as_bytes(), i) {
            if !token.terminated {
                refuse!("a SQL comparand has an unterminated quoted text or comment");
            }
            if token.kind == Kind::Comment {
                out.push(' ');
            } else {
                if token.kind == Kind::DollarString {
                    refuse!("a SQL comparand uses dollar quoting");
                }
                let mut chars = code[i..token.end].chars();
                while let Some(c) = chars.next() {
                    match c {
                        '%' => match chars.next() {
                            Some('%') => out.push('%'),
                            _ => refuse!("a SQL comparand has a placeholder inside a quoted text"),
                        },
                        '$' => refuse!("a SQL comparand quotes a dollar sign"),
                        other => out.push(other),
                    }
                }
            }
            i = token.end;
            continue;
        }
        let c = code[i..].chars().next().expect("i is within code");
        i += c.len_utf8();
        match c {
            '%' => {
                match code.as_bytes().get(i) {
                    Some(b'%') => out.push('%'),
                    Some(b's') => {
                        let Some(kind) = casts.get(used) else {
                            refuse!(
                                "a SQL comparand has more placeholders than its {} parameter(s)",
                                casts.len()
                            );
                        };
                        used += 1;
                        out.push_str(&format!("${used}::{kind}"));
                    }
                    _ => refuse!("a SQL comparand has a % that is neither %s nor %%"),
                }
                i += 1;
            }
            '$' => refuse!("a SQL comparand uses a dollar sign outside a placeholder"),
            ';' => refuse!("a SQL comparand holds more than one statement"),
            other => out.push(other),
        }
    }
    if used != casts.len() {
        refuse!(
            "a SQL comparand has {used} placeholder(s) for {} parameter(s)",
            casts.len()
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(value: &Json, nonce: Option<&str>) -> Result<(String, usize)> {
        let expr = subselect(value, nonce)?;
        let (sql, values) = sea_query::Query::select()
            .expr(expr)
            .to_owned()
            .build(sea_query::PostgresQueryBuilder);
        Ok((sql, values.0.len()))
    }

    #[test]
    fn placeholders_become_typed_bound_parameters_and_comments_are_dropped() {
        let value = json!({
            "$sql": "SELECT a.id -- the partner's rows\nFROM t AS a /* it's */ WHERE a.p = %s AND a.name LIKE 'x%%' AND a.id = ANY(%s) AND a.k = %s",
            "$params": [7, [1, 2], "read"],
            "$nonce": "n",
        });
        let (sql, n) = render(&value, Some("n")).unwrap();
        assert_eq!(
            sql,
            "SELECT (SELECT a.id  FROM t AS a   WHERE a.p = $1::int8 AND a.name LIKE 'x%' AND a.id = ANY($2::int8[]) AND a.k = $3::text)"
        );
        assert_eq!(n, 3);
    }

    #[test]
    fn integer_parameters_are_never_silently_rounded_to_float() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_test_writer()
            .try_init();
        for integer in [i64::MIN, -1, 0, 9_007_199_254_740_993, i64::MAX] {
            let value = param(&json!(integer)).unwrap();
            tracing::debug!(integer, ?value, "exact SQL fragment integer");
            assert_eq!(value, Value::BigInt(Some(integer)));
        }
        for integer in [i64::MAX as u64 + 1, i64::MAX as u64 + 2, u64::MAX] {
            let value = json!({"$sql": "SELECT %s::numeric", "$params": [integer], "$nonce": "n"});
            let result = render(&value, Some("n"));
            tracing::debug!(integer, ?result, "out-of-range SQL fragment integer");
            assert!(
                result.is_err(),
                "an integer outside int8 must delegate without rounding"
            );
        }
        assert_eq!(param(&json!(1.5)).unwrap(), Value::Double(Some(1.5)));
    }

    #[test]
    fn nested_comments_cannot_introduce_parameters_or_statements() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_test_writer()
            .try_init();
        let value = json!({"$sql": "SELECT /* outer /* inner */ %s; $1 */ %s", "$params": [7], "$nonce": "n"});
        let (sql, n) = render(&value, Some("n")).unwrap();
        tracing::debug!(%sql, n, "nested comment fragment");
        assert_eq!(sql, "SELECT (SELECT   $1::int8)");
        assert_eq!(n, 1);
        let unclosed =
            json!({"$sql": "SELECT /* outer /* inner */ %s", "$params": [], "$nonce": "n"});
        assert!(render(&unclosed, Some("n")).is_err());
        assert_eq!(
            rewrite(r"SELECT E'it\'s %%' WHERE a=%s", &["int8"]).unwrap(),
            r"SELECT E'it\'s %' WHERE a=$1::int8"
        );
    }

    #[test]
    fn a_fragment_without_this_requests_nonce_is_refused() {
        let value = json!({"$sql": "SELECT 1", "$params": [], "$nonce": "forged"});
        assert!(render(&value, Some("n")).is_err());
        assert!(render(&value, None).is_err());
        assert!(render(&json!({"$sql": "SELECT 1"}), Some("n")).is_err());
    }

    #[test]
    fn shapes_the_rewrite_cannot_bind_exactly_are_refused() {
        for (code, params) in [
            ("SELECT %s", json!([])),
            ("SELECT 1", json!([1])),
            ("SELECT $1", json!([])),
            ("SELECT $$x$$", json!([])),
            ("SELECT 1; DROP TABLE t", json!([])),
            ("SELECT '%s'", json!([1])),
            ("SELECT %d", json!([1])),
            ("SELECT 'open", json!([])),
            ("SELECT /* open", json!([])),
            ("SELECT %s", json!([null])),
            ("SELECT %s", json!([[1, "a"]])),
            ("SELECT %s", json!([{"k": 1}])),
        ] {
            let value = json!({"$sql": code, "$params": params, "$nonce": "n"});
            assert!(render(&value, Some("n")).is_err(), "{code} {params}");
        }
    }
}
