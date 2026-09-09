use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{NaiveDate, NaiveDateTime};
use pyo3::IntoPyObjectExt;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};
use tokio::runtime::Handle;
use tokio_postgres::Client;
use tokio_postgres::types::{ToSql, Type};

fn rerr(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(format!("{e}"))
}

fn db_err(e: tokio_postgres::Error) -> PyErr {
    let code = e
        .as_db_error()
        .map(|d| d.code().code().to_string())
        .unwrap_or_default();
    let msg = e
        .as_db_error()
        .map(|d| d.message().to_string())
        .unwrap_or_else(|| {
            let mut msg = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(&format!(" <- {s}"));
                src = std::error::Error::source(s);
            }
            msg
        });
    let mut out = format!("SQLSTATE:{code}|{msg}");
    if let Some(d) = e.as_db_error() {
        for (key, value) in [
            ("severity", Some(d.severity())),
            ("message_detail", d.detail()),
            ("message_hint", d.hint()),
            ("context", d.where_()),
            ("schema_name", d.schema()),
            ("table_name", d.table()),
            ("column_name", d.column()),
            ("datatype_name", d.datatype()),
            ("constraint_name", d.constraint()),
        ] {
            if let Some(v) = value {
                out.push('\u{1f}');
                out.push_str(key);
                out.push('\u{1e}');
                out.push_str(v);
            }
        }
    }
    PyRuntimeError::new_err(out)
}

fn translate_placeholders(sql: &str) -> (String, Vec<String>) {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    let mut in_squote = false;
    let mut in_dquote = false;
    let mut in_line_comment = false;
    let mut block_comment_depth = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_line_comment {
            out.push(c);
            if c == '\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if block_comment_depth > 0 {
            if c == '*' && bytes.get(i + 1) == Some(&b'/') {
                block_comment_depth -= 1;
                out.push_str("*/");
                i += 2;
                continue;
            }
            if c == '/' && bytes.get(i + 1) == Some(&b'*') {
                block_comment_depth += 1;
                out.push_str("/*");
                i += 2;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if in_squote {
            out.push(c);
            if c == '\'' {
                in_squote = false;
            }
            i += 1;
            continue;
        }
        if in_dquote {
            out.push(c);
            if c == '"' {
                in_dquote = false;
            }
            i += 1;
            continue;
        }
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            in_line_comment = true;
            out.push_str("--");
            i += 2;
            continue;
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            block_comment_depth = 1;
            out.push_str("/*");
            i += 2;
            continue;
        }
        match c {
            '\'' => {
                in_squote = true;
                out.push(c);
                i += 1;
            }
            '"' => {
                in_dquote = true;
                out.push(c);
                i += 1;
            }
            '%' if i + 1 < bytes.len() && bytes[i + 1] == b'%' => {
                out.push('%');
                i += 2;
            }
            '%' if i + 1 < bytes.len() && bytes[i + 1] == b's' => {
                names.push(String::new());
                out.push_str(&format!("${}", names.len()));
                i += 2;
            }
            '%' if i + 1 < bytes.len() && bytes[i + 1] == b'(' => {
                let end = sql[i..].find(")s").map(|p| i + p);
                match end {
                    Some(e) => {
                        names.push(sql[i + 2..e].to_string());
                        out.push_str(&format!("${}", names.len()));
                        i = e + 2;
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, names)
}

fn json_wrapper_value(v: &Bound<'_, PyAny>) -> PyResult<Option<serde_json::Value>> {
    let Ok(cls) = v.get_type().name() else {
        return Ok(None);
    };
    if cls != "Json" && cls != "Jsonb" {
        return Ok(None);
    }
    let Ok(inner) = v.getattr("obj") else {
        return Ok(None);
    };
    if let Ok(dumps) = v.getattr("dumps")
        && !dumps.is_none()
        && let Ok(text) = dumps.call1((inner.clone(),))
        && let Ok(text) = text.extract::<String>()
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text)
    {
        return Ok(Some(parsed));
    }
    Ok(Some(py_to_json(&inner)?))
}

fn conv_numeric(v: &Bound<'_, PyAny>) -> PyResult<rust_decimal::Decimal> {
    let s: String = v.str()?.extract()?;
    rust_decimal::Decimal::from_str(&s).map_err(rerr)
}

fn conv_date(v: &Bound<'_, PyAny>) -> PyResult<NaiveDate> {
    let s: String = v.str()?.extract()?;
    NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(rerr)
}

fn conv_time(v: &Bound<'_, PyAny>) -> PyResult<chrono::NaiveTime> {
    let s: String = v.str()?.extract()?;
    chrono::NaiveTime::parse_from_str(&s, "%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveTime::parse_from_str(&s, "%H:%M:%S"))
        .map_err(rerr)
}

fn conv_timestamp(v: &Bound<'_, PyAny>) -> PyResult<NaiveDateTime> {
    let s: String = v.str()?.extract()?;
    NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| {
            NaiveDate::parse_from_str(&s, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        })
        .map_err(rerr)
}

fn conv_timestamptz(v: &Bound<'_, PyAny>) -> PyResult<chrono::DateTime<chrono::Utc>> {
    let s: String = v.str()?.extract()?;
    parse_tstz(&s)
}

fn conv_json(v: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    match v.extract::<String>() {
        Ok(s) => {
            Ok(serde_json::from_str::<serde_json::Value>(&s)
                .unwrap_or(serde_json::Value::String(s)))
        }
        Err(_) => py_to_json(v),
    }
}

fn conv_ip(v: &Bound<'_, PyAny>) -> PyResult<std::net::IpAddr> {
    let s: String = v.str()?.extract()?;
    // A cidr value carries a prefix length; the address is what tokio-postgres
    // encodes, and Postgres supplies the /32 or /128 back.
    let addr = s.split('/').next().unwrap_or(&s);
    addr.parse::<std::net::IpAddr>().map_err(rerr)
}

fn extract_opt_vec<T>(
    v: &Bound<'_, PyAny>,
    conv: impl Fn(&Bound<'_, PyAny>) -> PyResult<T>,
) -> PyResult<Vec<Option<T>>> {
    let mut out = Vec::new();
    for item in v.try_iter()? {
        let item = item?;
        out.push(if item.is_none() {
            None
        } else {
            Some(conv(&item)?)
        });
    }
    Ok(out)
}

fn py_to_sql(
    py: Python<'_>,
    v: &Bound<'_, PyAny>,
    ty: &Type,
) -> PyResult<Box<dyn ToSql + Sync + Send>> {
    if let Some(jv) = json_wrapper_value(v)? {
        return Ok(Box::new(jv));
    }
    if v.is_none() {
        return Ok(match *ty {
            Type::BOOL => Box::new(None::<bool>),
            Type::INT2 => Box::new(None::<i16>),
            Type::INT4 => Box::new(None::<i32>),
            Type::INT8 => Box::new(None::<i64>),
            Type::FLOAT4 => Box::new(None::<f32>),
            Type::FLOAT8 => Box::new(None::<f64>),
            Type::NUMERIC => Box::new(None::<rust_decimal::Decimal>),
            Type::DATE => Box::new(None::<NaiveDate>),
            Type::TIMESTAMP => Box::new(None::<NaiveDateTime>),
            Type::TIMESTAMPTZ => Box::new(None::<chrono::DateTime<chrono::Utc>>),
            Type::JSON | Type::JSONB => Box::new(None::<serde_json::Value>),
            Type::BYTEA => Box::new(None::<Vec<u8>>),

            _ if is_vector(ty) => Box::new(None::<PgVector>),
            _ => match ty.kind() {
                tokio_postgres::types::Kind::Array(elem) => match *elem {
                    Type::BOOL => Box::new(None::<Vec<Option<bool>>>),
                    Type::INT2 => Box::new(None::<Vec<Option<i16>>>),
                    Type::INT4 => Box::new(None::<Vec<Option<i32>>>),
                    Type::INT8 => Box::new(None::<Vec<Option<i64>>>),
                    Type::FLOAT4 => Box::new(None::<Vec<Option<f32>>>),
                    Type::FLOAT8 => Box::new(None::<Vec<Option<f64>>>),
                    Type::OID => Box::new(None::<Vec<Option<u32>>>),
                    Type::BYTEA => Box::new(None::<Vec<Option<Vec<u8>>>>),
                    Type::NUMERIC => Box::new(None::<Vec<Option<rust_decimal::Decimal>>>),
                    Type::DATE => Box::new(None::<Vec<Option<NaiveDate>>>),
                    Type::TIME => Box::new(None::<Vec<Option<chrono::NaiveTime>>>),
                    Type::TIMESTAMP => Box::new(None::<Vec<Option<NaiveDateTime>>>),
                    Type::TIMESTAMPTZ => {
                        Box::new(None::<Vec<Option<chrono::DateTime<chrono::Utc>>>>)
                    }
                    Type::JSON | Type::JSONB => Box::new(None::<Vec<Option<serde_json::Value>>>),
                    Type::INET | Type::CIDR => Box::new(None::<Vec<Option<std::net::IpAddr>>>),
                    _ => Box::new(None::<Vec<Option<String>>>),
                },
                _ => Box::new(None::<String>),
            },
        });
    }
    Ok(match *ty {
        Type::BOOL => Box::new(v.extract::<bool>()?),
        Type::INT2 => Box::new(v.extract::<i16>()?),
        Type::INT4 => Box::new(v.extract::<i32>()?),
        Type::INT8 => Box::new(v.extract::<i64>()?),
        Type::FLOAT4 => Box::new(v.extract::<f32>()?),
        Type::FLOAT8 => Box::new(v.extract::<f64>()?),
        Type::NUMERIC => Box::new(conv_numeric(v)?),
        Type::DATE => Box::new(conv_date(v)?),
        Type::TIMESTAMP => Box::new(conv_timestamp(v)?),
        Type::TIMESTAMPTZ => Box::new(conv_timestamptz(v)?),
        Type::JSON | Type::JSONB => Box::new(conv_json(v)?),
        Type::BYTEA => Box::new(v.extract::<Vec<u8>>()?),
        _ if is_vector(ty) => {
            // Odoo hands an embedding over as the text form or as a list of
            // numbers; psycopg ships the text and lets the server cast, which
            // this transport cannot do, so it is parsed and encoded here.
            let floats = if let Ok(text) = v.extract::<String>() {
                parse_vector_text(&text)
                    .ok_or_else(|| rerr(format!("not a vector literal: {text}")))?
            } else {
                v.extract::<Vec<f32>>()?
            };
            Box::new(PgVector(floats))
        }
        _ => {
            let _ = py;

            if let tokio_postgres::types::Kind::Array(elem) = ty.kind() {
                return Ok(match *elem {
                    Type::INT2 => Box::new(v.extract::<Vec<Option<i16>>>()?),
                    Type::INT4 => Box::new(v.extract::<Vec<Option<i32>>>()?),
                    Type::INT8 => Box::new(v.extract::<Vec<Option<i64>>>()?),
                    Type::FLOAT4 => Box::new(v.extract::<Vec<Option<f32>>>()?),
                    Type::FLOAT8 => Box::new(v.extract::<Vec<Option<f64>>>()?),
                    Type::BOOL => Box::new(v.extract::<Vec<Option<bool>>>()?),
                    Type::OID => Box::new(v.extract::<Vec<Option<u32>>>()?),
                    Type::BYTEA => Box::new(v.extract::<Vec<Option<Vec<u8>>>>()?),
                    Type::NUMERIC => Box::new(extract_opt_vec(v, conv_numeric)?),
                    Type::DATE => Box::new(extract_opt_vec(v, conv_date)?),
                    Type::TIME => Box::new(extract_opt_vec(v, conv_time)?),
                    Type::TIMESTAMP => Box::new(extract_opt_vec(v, conv_timestamp)?),
                    Type::TIMESTAMPTZ => Box::new(extract_opt_vec(v, conv_timestamptz)?),
                    Type::JSON | Type::JSONB => Box::new(extract_opt_vec(v, conv_json)?),
                    Type::INET | Type::CIDR => Box::new(extract_opt_vec(v, conv_ip)?),

                    Type::CHAR => {
                        let mut out: Vec<Option<i8>> = Vec::new();
                        for item in v.try_iter()? {
                            let item = item?;
                            out.push(if item.is_none() {
                                None
                            } else {
                                let s: String = item.str()?.extract()?;
                                Some(*s.as_bytes().first().unwrap_or(&0) as i8)
                            });
                        }
                        Box::new(out)
                    }
                    _ => {
                        let mut out: Vec<Option<String>> = Vec::new();
                        for item in v.try_iter()? {
                            let item = item?;
                            out.push(if item.is_none() {
                                None
                            } else {
                                Some(item.str()?.extract()?)
                            });
                        }
                        Box::new(out)
                    }
                });
            }

            Box::new(v.str()?.extract::<String>()?)
        }
    })
}

fn parse_tstz(s: &str) -> PyResult<chrono::DateTime<chrono::Utc>> {
    use chrono::{DateTime, NaiveDate, TimeZone, Utc};
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Ok(d.with_timezone(&Utc));
    }

    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%:z",
    ] {
        if let Ok(d) = DateTime::parse_from_str(s, fmt) {
            return Ok(d.with_timezone(&Utc));
        }
    }

    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(n.and_utc());
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()));
    }
    Err(rerr(format!("cannot parse {s:?} as a timestamptz")))
}

fn py_to_json(v: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    use serde_json::Value as J;
    if v.is_none() {
        return Ok(J::Null);
    }
    if let Ok(b) = v.cast::<pyo3::types::PyBool>() {
        return Ok(J::Bool(b.is_true()));
    }
    if let Ok(i) = v.extract::<i64>() {
        return Ok(J::from(i));
    }
    if let Ok(f) = v.extract::<f64>() {
        return Ok(J::from(f));
    }
    if let Ok(s) = v.extract::<String>() {
        return Ok(J::String(s));
    }
    if let Ok(d) = v.cast::<PyDict>() {
        let mut m = serde_json::Map::new();
        for (k, val) in d.iter() {
            m.insert(k.str()?.extract()?, py_to_json(&val)?);
        }
        return Ok(J::Object(m));
    }
    if let Ok(l) = v.cast::<PyList>() {
        let mut out = Vec::new();
        for item in l.iter() {
            out.push(py_to_json(&item)?);
        }
        return Ok(J::Array(out));
    }

    Ok(J::String(v.str()?.extract()?))
}

fn json_to_py(py: Python<'_>, v: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use serde_json::Value as J;
    Ok(match v {
        J::Null => py.None(),
        J::Bool(b) => b.into_py_any(py)?,
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_py_any(py)?
            } else {
                n.as_f64().unwrap_or(0.0).into_py_any(py)?
            }
        }
        J::String(s) => s.into_py_any(py)?,
        J::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.into_py_any(py)?
        }
        J::Object(m) => {
            let d = PyDict::new(py);
            for (k, val) in m {
                d.set_item(k, json_to_py(py, val)?)?;
            }
            d.into_py_any(py)?
        }
    })
}

struct DateTimeTypes {
    date: Py<PyAny>,
    time: Py<PyAny>,
    datetime: Py<PyAny>,
    timedelta: Py<PyAny>,
    uuid: Py<PyAny>,
    utc: Py<PyAny>,
    /// `decimal.Decimal`, because that is what psycopg returns for NUMERIC.
    /// A float is a different value -- `0.1` is not `Decimal('0.1')` -- and
    /// a NUMERIC wider than f64 came back as NaN, silently.
    decimal: Py<PyAny>,
}

fn decimal_to_py(
    py: Python<'_>,
    dt: &DateTimeTypes,
    d: rust_decimal::Decimal,
) -> PyResult<Py<PyAny>> {
    // Through the string form: rust_decimal keeps the wire scale, so
    // `1.250` stays `Decimal('1.250')` as psycopg would have it.
    Ok(dt.decimal.bind(py).call1((d.to_string(),))?.unbind())
}

static DT_TYPES: pyo3::sync::PyOnceLock<DateTimeTypes> = pyo3::sync::PyOnceLock::new();

fn dt_types(py: Python<'_>) -> PyResult<&'static DateTimeTypes> {
    DT_TYPES.get_or_try_init(py, || {
        let m = py.import("datetime")?;
        Ok(DateTimeTypes {
            date: m.getattr("date")?.unbind(),
            time: m.getattr("time")?.unbind(),
            datetime: m.getattr("datetime")?.unbind(),
            timedelta: m.getattr("timedelta")?.unbind(),
            uuid: py.import("uuid")?.getattr("UUID")?.unbind(),
            utc: m.getattr("timezone")?.getattr("utc")?.unbind(),
            decimal: py.import("decimal")?.getattr("Decimal")?.unbind(),
        })
    })
}

struct RawBytes(Vec<u8>);

impl<'a> tokio_postgres::types::FromSql<'a> for RawBytes {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawBytes(raw.to_vec()))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

fn warn_unknown_type(ty: &Type) {
    use std::sync::Mutex;
    static SEEN: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
    let mut guard = SEEN.lock().unwrap();
    let seen = guard.get_or_insert_with(std::collections::HashSet::new);
    if seen.insert(ty.name().to_string()) {
        tracing::warn!(
            target: "odoo_kernel::cursor",
            r#type = ty.name(),
            "no decoder for this Postgres type; handing the raw bytes to Python, \
             as psycopg does for an unregistered type"
        );
    }
}

struct PgInterval {
    micros: i64,
    days: i32,
    months: i32,
}

impl<'a> tokio_postgres::types::FromSql<'a> for PgInterval {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 16 {
            return Err("interval must be 16 bytes".into());
        }
        Ok(PgInterval {
            micros: i64::from_be_bytes(raw[0..8].try_into()?),
            days: i32::from_be_bytes(raw[8..12].try_into()?),
            months: i32::from_be_bytes(raw[12..16].try_into()?),
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INTERVAL
    }
}

/// The 16 raw bytes of a `uuid`, handed to Python's `uuid.UUID(bytes=...)`.
struct PgUuid([u8; 16]);

impl<'a> tokio_postgres::types::FromSql<'a> for PgUuid {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(PgUuid(raw.try_into().map_err(|_| "uuid must be 16 bytes")?))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::UUID
    }
}

/// pgvector's binary format: `int16` dimensions, `int16` unused, then that
/// many big-endian `float4`. The extension is in this workspace's database
/// template and agromarin's AI modules store embeddings in it, so `vector` is
/// a first-class column type here even though it is not a catalog one.
///
/// psycopg has no loader for it and therefore returns the TEXT form, `[1,2,3]`.
/// This decodes the binary and renders the same string, so the two cursors
/// agree; the rust cursor cannot ask for text format, which is the only
/// reason a conversion is needed at all.
#[derive(Debug)]
struct PgVector(Vec<f32>);

impl<'a> tokio_postgres::types::FromSql<'a> for PgVector {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 4 {
            return Err("vector header is 4 bytes".into());
        }
        let dim = u16::from_be_bytes([raw[0], raw[1]]) as usize;
        if raw.len() != 4 + dim * 4 {
            return Err("vector length does not match its dimension".into());
        }
        let mut out = Vec::with_capacity(dim);
        for k in 0..dim {
            let at = 4 + k * 4;
            out.push(f32::from_be_bytes([
                raw[at],
                raw[at + 1],
                raw[at + 2],
                raw[at + 3],
            ]));
        }
        Ok(PgVector(out))
    }

    fn accepts(ty: &Type) -> bool {
        is_vector(ty)
    }
}

impl tokio_postgres::types::ToSql for PgVector {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        use bytes::BufMut;
        out.put_u16(self.0.len() as u16);
        out.put_u16(0);
        for f in &self.0 {
            out.put_f32(*f);
        }
        Ok(tokio_postgres::types::IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        is_vector(ty)
    }

    tokio_postgres::types::to_sql_checked!();
}

/// `vector` comes from an extension, so its oid is per-database and there is
/// no `Type` constant to match on; the name is what identifies it.
fn is_vector(ty: &Type) -> bool {
    ty.name() == "vector"
}

/// PostGIS geometry and geography. psycopg has no loader for either and so
/// returns the type's TEXT output, which for these two IS the hex-encoded
/// WKB -- byte for byte what arrives here in binary. Rendering it as
/// uppercase hex reproduces psycopg's string exactly.
///
/// Only these two. `box2d` and `box3d` print `BOX(...)` rather than hex, and
/// `box2d` has no binary output function at all, so it cannot be read through
/// this cursor by any means (see the note in README).
fn is_postgis_wkb(ty: &Type) -> bool {
    matches!(ty.name(), "geometry" | "geography")
}

/// psycopg renders it with no spaces, and floats that are whole print without
/// a decimal point -- `[1,2,3]`, not `[1.0, 2.0, 3.0]`.
fn vector_to_text(v: &[f32]) -> String {
    let mut out = String::from("[");
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if f.fract() == 0.0 && f.is_finite() {
            out.push_str(&format!("{}", *f as i64));
        } else {
            out.push_str(&format!("{f}"));
        }
    }
    out.push(']');
    out
}

fn parse_vector_text(text: &str) -> Option<Vec<f32>> {
    let inner = text.trim().strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|p| p.trim().parse::<f32>().ok())
        .collect()
}

fn date_to_py(py: Python<'_>, dt: &DateTimeTypes, d: NaiveDate) -> PyResult<Py<PyAny>> {
    use chrono::Datelike;
    Ok(dt
        .date
        .bind(py)
        .call1((d.year(), d.month(), d.day()))?
        .unbind())
}

fn time_to_py(py: Python<'_>, dt: &DateTimeTypes, t: chrono::NaiveTime) -> PyResult<Py<PyAny>> {
    use chrono::Timelike;
    Ok(dt
        .time
        .bind(py)
        .call1((t.hour(), t.minute(), t.second(), t.nanosecond() / 1_000))?
        .unbind())
}

fn naive_datetime_to_py(
    py: Python<'_>,
    dt: &DateTimeTypes,
    d: NaiveDateTime,
) -> PyResult<Py<PyAny>> {
    use chrono::{Datelike, Timelike};
    Ok(dt
        .datetime
        .bind(py)
        .call1((
            d.year(),
            d.month(),
            d.day(),
            d.hour(),
            d.minute(),
            d.second(),
            d.and_utc().timestamp_subsec_micros(),
        ))?
        .unbind())
}

fn utc_datetime_to_py(
    py: Python<'_>,
    dt: &DateTimeTypes,
    d: chrono::DateTime<chrono::Utc>,
) -> PyResult<Py<PyAny>> {
    use chrono::{Datelike, Timelike};
    let utc = dt.utc.bind(py);
    Ok(dt
        .datetime
        .bind(py)
        .call1((
            d.year(),
            d.month(),
            d.day(),
            d.hour(),
            d.minute(),
            d.second(),
            d.timestamp_subsec_micros(),
            utc,
        ))?
        .unbind())
}

fn cell_to_py(py: Python<'_>, row: &tokio_postgres::Row, i: usize) -> PyResult<Py<PyAny>> {
    let ty = row.columns()[i].type_();
    let dt = dt_types(py)?;
    Ok(match *ty {
        Type::BOOL => match row.try_get::<_, Option<bool>>(i).map_err(rerr)? {
            Some(v) => v.into_py_any(py)?,
            None => py.None(),
        },
        Type::INT2 => match row.try_get::<_, Option<i16>>(i).map_err(rerr)? {
            Some(v) => v.into_py_any(py)?,
            None => py.None(),
        },
        Type::INT4 => match row.try_get::<_, Option<i32>>(i).map_err(rerr)? {
            Some(v) => v.into_py_any(py)?,
            None => py.None(),
        },
        Type::INT8 | Type::OID => match row.try_get::<_, Option<i64>>(i) {
            Ok(Some(v)) => v.into_py_any(py)?,
            Ok(None) => py.None(),
            Err(_) => match row.try_get::<_, Option<u32>>(i).map_err(rerr)? {
                Some(v) => v.into_py_any(py)?,
                None => py.None(),
            },
        },
        Type::FLOAT4 => match row.try_get::<_, Option<f32>>(i).map_err(rerr)? {
            Some(v) => v.into_py_any(py)?,
            None => py.None(),
        },
        Type::FLOAT8 => match row.try_get::<_, Option<f64>>(i).map_err(rerr)? {
            Some(v) => v.into_py_any(py)?,
            None => py.None(),
        },
        Type::NUMERIC => {
            match row
                .try_get::<_, Option<rust_decimal::Decimal>>(i)
                .map_err(rerr)?
            {
                Some(v) => decimal_to_py(py, dt_types(py)?, v)?,
                None => py.None(),
            }
        }
        Type::DATE => match row.try_get::<_, Option<NaiveDate>>(i).map_err(rerr)? {
            Some(d) => {
                use chrono::Datelike;
                dt.date
                    .bind(py)
                    .call1((d.year(), d.month(), d.day()))?
                    .unbind()
            }
            None => py.None(),
        },
        Type::TIMESTAMP => match row.try_get::<_, Option<NaiveDateTime>>(i).map_err(rerr)? {
            Some(d) => {
                use chrono::{Datelike, Timelike};
                dt.datetime
                    .bind(py)
                    .call1((
                        d.year(),
                        d.month(),
                        d.day(),
                        d.hour(),
                        d.minute(),
                        d.second(),
                        d.and_utc().timestamp_subsec_micros(),
                    ))?
                    .unbind()
            }
            None => py.None(),
        },
        Type::INTERVAL => match row.try_get::<_, Option<PgInterval>>(i).map_err(rerr)? {
            Some(v) => dt
                .timedelta
                .bind(py)
                .call1((i64::from(v.months) * 30 + i64::from(v.days), 0, v.micros))?
                .unbind(),
            None => py.None(),
        },
        Type::UUID => match row.try_get::<_, Option<PgUuid>>(i).map_err(rerr)? {
            Some(v) => {
                let kwargs = pyo3::types::PyDict::new(py);
                kwargs.set_item("bytes", pyo3::types::PyBytes::new(py, &v.0))?;
                dt.uuid.bind(py).call((), Some(&kwargs))?.unbind()
            }
            None => py.None(),
        },
        Type::TIMESTAMPTZ => match row
            .try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(i)
            .map_err(rerr)?
        {
            Some(d) => {
                use chrono::{Datelike, Timelike};
                let utc = dt.utc.bind(py);
                dt.datetime
                    .bind(py)
                    .call1((
                        d.year(),
                        d.month(),
                        d.day(),
                        d.hour(),
                        d.minute(),
                        d.second(),
                        d.timestamp_subsec_micros(),
                        utc,
                    ))?
                    .unbind()
            }
            None => py.None(),
        },
        Type::JSON | Type::JSONB => {
            match row
                .try_get::<_, Option<serde_json::Value>>(i)
                .map_err(rerr)?
            {
                Some(v) => json_to_py(py, &v)?,
                None => py.None(),
            }
        }
        Type::BYTEA => match row.try_get::<_, Option<Vec<u8>>>(i).map_err(rerr)? {
            Some(v) => PyBytes::new(py, &v).unbind().into(),
            None => py.None(),
        },

        Type::CHAR => match row.try_get::<_, Option<i8>>(i).map_err(rerr)? {
            Some(v) => ((v as u8) as char).to_string().into_py_any(py)?,
            None => py.None(),
        },
        _ => {
            if let tokio_postgres::types::Kind::Array(elem) = ty.kind() {
                macro_rules! arr {
                    ($t:ty) => {
                        match row.try_get::<_, Option<Vec<Option<$t>>>>(i).map_err(rerr)? {
                            Some(v) => return v.into_py_any(py),
                            None => return Ok(py.None()),
                        }
                    };
                }
                macro_rules! arr_conv {
                    ($t:ty, $conv:expr) => {
                        match row.try_get::<_, Option<Vec<Option<$t>>>>(i).map_err(rerr)? {
                            Some(v) => {
                                let list = PyList::empty(py);
                                for item in v {
                                    match item {
                                        Some(x) => list.append($conv(x)?)?,
                                        None => list.append(py.None())?,
                                    }
                                }
                                return list.into_py_any(py);
                            }
                            None => return Ok(py.None()),
                        }
                    };
                }
                match *elem {
                    Type::BOOL => arr!(bool),
                    Type::INT2 => arr!(i16),
                    Type::INT4 => arr!(i32),
                    Type::INT8 => arr!(i64),
                    Type::FLOAT4 => arr!(f32),
                    Type::FLOAT8 => arr!(f64),
                    Type::TEXT | Type::VARCHAR | Type::NAME | Type::BPCHAR => arr!(String),
                    Type::BYTEA => arr!(Vec<u8>),
                    Type::OID => arr!(u32),

                    Type::NUMERIC => {
                        arr_conv!(rust_decimal::Decimal, |x| decimal_to_py(py, dt, x))
                    }
                    Type::DATE => arr_conv!(NaiveDate, |x| date_to_py(py, dt, x)),
                    Type::TIME => arr_conv!(chrono::NaiveTime, |x| time_to_py(py, dt, x)),
                    Type::TIMESTAMP => {
                        arr_conv!(NaiveDateTime, |x| naive_datetime_to_py(py, dt, x))
                    }
                    Type::TIMESTAMPTZ => {
                        arr_conv!(chrono::DateTime<chrono::Utc>, |x| utc_datetime_to_py(
                            py, dt, x
                        ))
                    }

                    Type::JSON | Type::JSONB => {
                        match row
                            .try_get::<_, Option<Vec<Option<serde_json::Value>>>>(i)
                            .map_err(rerr)?
                        {
                            Some(v) => {
                                let list = PyList::empty(py);
                                for item in &v {
                                    match item {
                                        Some(j) => list.append(json_to_py(py, j)?)?,
                                        None => list.append(py.None())?,
                                    }
                                }
                                return list.into_py_any(py);
                            }
                            None => return Ok(py.None()),
                        }
                    }
                    _ => {}
                }
            }
            if is_postgis_wkb(ty) {
                return match row.try_get::<_, Option<RawBytes>>(i).map_err(rerr)? {
                    Some(raw) => {
                        let mut hex = String::with_capacity(raw.0.len() * 2);
                        for b in &raw.0 {
                            hex.push_str(&format!("{b:02X}"));
                        }
                        hex.into_py_any(py)
                    }
                    None => Ok(py.None()),
                };
            }
            if is_vector(ty) {
                return match row.try_get::<_, Option<PgVector>>(i).map_err(rerr)? {
                    Some(v) => vector_to_text(&v.0).into_py_any(py),
                    None => Ok(py.None()),
                };
            }
            match row.try_get::<_, Option<String>>(i) {
                Ok(Some(v)) => v.into_py_any(py)?,
                Ok(None) => py.None(),

                Err(_) => match row.try_get::<_, Option<RawBytes>>(i) {
                    Ok(Some(raw)) => {
                        warn_unknown_type(ty);
                        pyo3::types::PyBytes::new(py, &raw.0).into_py_any(py)?
                    }
                    Ok(None) => py.None(),
                    Err(e) => {
                        return Err(rerr(format!(
                            "cannot decode column {} of type {} ({e})",
                            row.columns()[i].name(),
                            ty.name()
                        )));
                    }
                },
            }
        }
    })
}

#[pyclass]
pub struct RustResult {
    #[pyo3(get)]
    pub rowcount: i64,
    #[pyo3(get)]
    pub columns: Vec<String>,

    #[pyo3(get)]
    pub rows: Py<PyList>,
}

#[pyclass]
pub struct RustConn {
    client: Arc<Client>,
    handle: Handle,
    stmt_cache: std::sync::Mutex<HashMap<String, tokio_postgres::Statement>>,

    kernel_stmts: Arc<odoo_kernel::orm::StmtCache>,
    in_tx: AtomicBool,
    pub readonly: AtomicBool,
    closed: AtomicBool,
}

fn statement_invalidates_prepared(sql: &str) -> bool {
    let head = sql
        .trim_start()
        .trim_start_matches('(')
        .trim_start()
        .as_bytes();
    let starts =
        |lit: &[u8]| head.len() >= lit.len() && head[..lit.len()].eq_ignore_ascii_case(lit);
    starts(b"rollback") || starts(b"drop ")
}

impl RustConn {
    pub fn client(&self) -> Arc<Client> {
        self.client.clone()
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    pub fn kernel_stmts(&self) -> Arc<odoo_kernel::orm::StmtCache> {
        self.kernel_stmts.clone()
    }

    pub fn new(client: Arc<Client>, handle: Handle) -> Self {
        RustConn {
            client,
            handle,
            stmt_cache: std::sync::Mutex::new(HashMap::new()),
            kernel_stmts: Arc::new(odoo_kernel::orm::StmtCache::default()),
            in_tx: AtomicBool::new(false),
            readonly: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    fn type_display_name(&self, py: Python<'_>, ty: &Type) -> String {
        let fallback = || ty.name().to_string();
        let rows = match self.block(
            py,
            self.client
                .query("SELECT format_type($1::oid, NULL)", &[&ty.oid()]),
        ) {
            Ok(rows) => rows,
            Err(_) => return fallback(),
        };
        rows.first()
            .and_then(|r| r.try_get::<_, String>(0).ok())
            .unwrap_or_else(fallback)
    }

    fn ensure_tx(&self, py: Python<'_>) -> PyResult<()> {
        if !self.in_tx.swap(true, Ordering::SeqCst) {
            let begin = if self.readonly.load(Ordering::SeqCst) {
                "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY"
            } else {
                "BEGIN ISOLATION LEVEL REPEATABLE READ"
            };
            self.block(py, self.client.batch_execute(begin))
                .map_err(db_err)?;
        }
        Ok(())
    }

    fn end_tx(&self, py: Python<'_>, stmt: &str) -> PyResult<()> {
        if self.in_tx.swap(false, Ordering::SeqCst) {
            self.block(py, self.client.batch_execute(stmt))
                .map_err(db_err)?;
        }
        Ok(())
    }

    fn block<F>(&self, py: Python<'_>, fut: F) -> F::Output
    where
        F: std::future::Future + Send,
        F::Output: Send,
    {
        py.detach(|| self.handle.block_on(fut))
    }
}

fn declared_type(v: &Bound<'_, PyAny>) -> Type {
    if v.is_none() {
        return Type::UNKNOWN;
    }
    let tyname = v
        .get_type()
        .name()
        .map(|n| n.to_string())
        .unwrap_or_default();
    match tyname.as_str() {
        "bool" => Type::BOOL,
        "int" => match v.extract::<i64>() {
            Ok(n) if i16::try_from(n).is_ok() => Type::INT2,
            Ok(n) if i32::try_from(n).is_ok() => Type::INT4,
            Ok(_) => Type::INT8,
            Err(_) => Type::NUMERIC,
        },
        "float" => Type::FLOAT8,

        "str" => Type::UNKNOWN,
        "bytes" => Type::BYTEA,
        "dict" | "Json" | "Jsonb" => Type::JSONB,
        "date" => Type::DATE,
        "datetime" => Type::TIMESTAMP,
        "list" | "tuple" => {
            let first = v
                .try_iter()
                .ok()
                .and_then(|mut it| it.next())
                .and_then(|r| r.ok());
            match first {
                None => Type::UNKNOWN,
                Some(x) if x.cast::<pyo3::types::PyBool>().is_ok() => Type::BOOL_ARRAY,
                Some(x) if x.extract::<i64>().is_ok() => Type::INT8_ARRAY,
                Some(x) if x.extract::<f64>().is_ok() => Type::FLOAT8_ARRAY,
                _ => Type::UNKNOWN,
            }
        }
        _ => Type::UNKNOWN,
    }
}

fn returns_rows(sql: &str) -> bool {
    let head = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if matches!(
        head.as_str(),
        "SELECT" | "WITH" | "SHOW" | "VALUES" | "TABLE" | "EXPLAIN" | "FETCH"
    ) {
        return true;
    }

    has_bare_keyword(sql, "RETURNING")
}

fn has_bare_keyword(sql: &str, keyword: &str) -> bool {
    let bytes = sql.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut i = 0;
    let mut in_squote = false;
    let mut in_dquote = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_squote {
            in_squote = b != b'\'';
            i += 1;
            continue;
        }
        if in_dquote {
            in_dquote = b != b'"';
            i += 1;
            continue;
        }
        match b {
            b'\'' => in_squote = true,
            b'"' => in_dquote = true,
            _ => {
                let rest = &sql[i..];
                if rest.len() >= keyword.len()
                    && rest[..keyword.len()].eq_ignore_ascii_case(keyword)
                    && (i == 0 || !is_ident(bytes[i - 1]))
                    && !bytes.get(i + keyword.len()).copied().is_some_and(is_ident)
                {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

#[pymethods]
impl RustConn {
    #[pyo3(signature = (query, params=None))]
    fn execute(
        &self,
        py: Python<'_>,
        query: &str,
        params: Option<Bound<'_, PyAny>>,
    ) -> PyResult<RustResult> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(rerr("connection is closed"));
        }
        self.ensure_tx(py)?;

        let (sql, stmt, boxed) = match &params {
            None => (query.to_string(), None, Vec::new()),
            Some(p) => {
                let (sql, names) = translate_placeholders(query);

                let mut values: Vec<Bound<'_, PyAny>> = Vec::with_capacity(names.len());
                if let Ok(d) = p.cast::<PyDict>() {
                    for n in &names {
                        values.push(
                            d.get_item(n)?
                                .ok_or_else(|| rerr(format!("missing param {n}")))?,
                        );
                    }
                } else {
                    let seq: Vec<Bound<'_, PyAny>> = p.try_iter()?.collect::<PyResult<_>>()?;
                    if seq.len() != names.len() {
                        return Err(rerr(format!(
                            "placeholder/param count mismatch: {} vs {}",
                            names.len(),
                            seq.len()
                        )));
                    }
                    values = seq;
                }

                let stmt = {
                    let types: Vec<Type> = values.iter().map(declared_type).collect();
                    let key = format!(
                        "{sql}\u{0}{}",
                        types
                            .iter()
                            .map(|t| t.oid().to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    let cached = self.stmt_cache.lock().unwrap().get(&key).cloned();
                    match cached {
                        Some(s) => s,
                        None => {
                            let s = self
                                .block(py, self.client.prepare_typed(&sql, &types))
                                .map_err(db_err)?;
                            {
                                let mut cache = self.stmt_cache.lock().unwrap();

                                if cache.len() >= odoo_kernel::db::MAX_PREPARED {
                                    cache.clear();
                                }
                                cache.insert(key, s.clone());
                            }
                            s
                        }
                    }
                };
                let mut stmt = stmt;

                {
                    let unsupported = |t: &Type| {
                        !matches!(
                            *t,
                            Type::BOOL
                                | Type::INT2
                                | Type::INT4
                                | Type::INT8
                                | Type::FLOAT4
                                | Type::FLOAT8
                                | Type::NUMERIC
                                | Type::DATE
                                | Type::TIMESTAMP
                                | Type::TIMESTAMPTZ
                                | Type::JSON
                                | Type::JSONB
                                | Type::BYTEA
                                | Type::TEXT
                                | Type::VARCHAR
                                | Type::NAME
                                | Type::BPCHAR
                                | Type::UNKNOWN
                        ) && !matches!(t.kind(), tokio_postgres::types::Kind::Array(_))
                            // `vector` IS encodable here (pgvector's format is
                            // implemented below), so re-declaring it `text`
                            // would turn a working parameter into 42804,
                            // "column is of type vector but expression is of
                            // type text".
                            && !is_vector(t)
                    };
                    if stmt.params().iter().any(unsupported) {
                        let types: Vec<Type> = stmt
                            .params()
                            .iter()
                            .map(|t| {
                                if unsupported(t) {
                                    Type::TEXT
                                } else {
                                    t.clone()
                                }
                            })
                            .collect();
                        stmt = self
                            .block(py, self.client.prepare_typed(&sql, &types))
                            .map_err(db_err)?;
                        self.stmt_cache
                            .lock()
                            .unwrap()
                            .insert(sql.clone(), stmt.clone());
                    }
                }
                let types = stmt.params().to_vec();
                if std::env::var_os("POC_TRACE").is_some() {
                    let declared: Vec<Type> = values.iter().map(declared_type).collect();
                    eprintln!(
                        "[param-types] declared={declared:?} server={types:?} sql={}",
                        &sql[..sql.len().min(90)]
                    );
                }
                let mut boxed: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
                for (v, ty) in values.iter().zip(types.iter()) {
                    match py_to_sql(py, v, ty) {
                        Ok(b) => boxed.push(b),
                        Err(e) => {
                            if let Ok(text) = v.extract::<String>() {
                                return Err(rerr(format!(
                                    "SQLSTATE:22P02|invalid input syntax for \
                                     type {}: \"{}\"",
                                    self.type_display_name(py, ty),
                                    text
                                )));
                            }
                            return Err(e);
                        }
                    }
                }
                (sql, Some(stmt), boxed)
            }
        };

        let refs: Vec<&(dyn ToSql + Sync)> = boxed
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();

        if returns_rows(&sql) {
            let mut stmt = stmt;
            if stmt.is_none() {
                let cached = self.stmt_cache.lock().unwrap().get(&sql).cloned();
                stmt = match cached {
                    Some(s) => Some(s),
                    None => match self.block(py, self.client.prepare(&sql)) {
                        Ok(s) => {
                            self.stmt_cache
                                .lock()
                                .unwrap()
                                .insert(sql.clone(), s.clone());
                            Some(s)
                        }
                        Err(_) => None,
                    },
                };
            }
            let rows = match &stmt {
                Some(st) => self.block(py, self.client.query(st, &refs)),
                None => self.block(py, self.client.query(&sql, &refs)),
            }
            .map_err(db_err)?;

            let columns: Vec<String> = match &stmt {
                Some(st) => st.columns().iter().map(|c| c.name().to_string()).collect(),
                None => rows
                    .first()
                    .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                    .unwrap_or_default(),
            };
            let list = PyList::empty(py);
            for row in &rows {
                let mut cells: Vec<Py<PyAny>> = Vec::with_capacity(row.len());
                for i in 0..row.len() {
                    cells.push(cell_to_py(py, row, i)?);
                }
                list.append(PyTuple::new(py, cells)?)?;
            }
            Ok(RustResult {
                rowcount: rows.len() as i64,
                columns,
                rows: list.unbind(),
            })
        } else {
            let n = if refs.is_empty() && sql.contains(';') {
                self.block(py, self.client.batch_execute(&sql))
                    .map_err(db_err)?;
                0
            } else {
                match &stmt {
                    Some(st) => self.block(py, self.client.execute(st, &refs)),
                    None => self.block(py, self.client.execute(&sql, &refs)),
                }
                .map_err(db_err)? as i64
            };
            if statement_invalidates_prepared(&sql) {
                self.stmt_cache.lock().unwrap().clear();
            }
            Ok(RustResult {
                rowcount: n,
                columns: vec![],
                rows: PyList::empty(py).unbind(),
            })
        }
    }

    fn copy(&self, py: Python<'_>, statement: &str) -> PyResult<RustCopy> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(rerr("connection is closed"));
        }
        self.ensure_tx(py)?;
        let sink = self
            .block(py, self.client.copy_in::<_, bytes::Bytes>(statement))
            .map_err(db_err)?;
        Ok(RustCopy {
            sink: Some(Box::pin(sink)),
            handle: self.handle.clone(),
            types: Vec::new(),
            buf: bytes::BytesMut::with_capacity(COPY_FLUSH_AT * 2),
            rows: 0,
            started: false,
            binary: statement_is_binary_copy(statement),
            row_mode: false,
        })
    }

    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        self.end_tx(py, "COMMIT")
    }

    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        let out = self.end_tx(py, "ROLLBACK");
        self.stmt_cache.lock().unwrap().clear();
        out
    }

    fn clear_prepared(&self) {
        self.stmt_cache.lock().unwrap().clear();
        self.kernel_stmts.clear();
    }

    #[getter]
    fn prepared_count(&self) -> usize {
        self.stmt_cache.lock().unwrap().len() + self.kernel_stmts.len()
    }

    #[getter]
    fn in_transaction(&self) -> bool {
        self.in_tx.load(Ordering::SeqCst)
    }

    #[getter]
    fn prepared_names(&self) -> Vec<String> {
        self.stmt_cache.lock().unwrap().keys().cloned().collect()
    }

    fn set_readonly(&self, value: bool) {
        self.readonly.store(value, Ordering::SeqCst);
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = self.end_tx(py, "ROLLBACK");
        }
        Ok(())
    }

    #[getter]
    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::{returns_rows, translate_placeholders};

    #[test]
    fn positional_placeholders_are_numbered() {
        let (sql, names) = translate_placeholders("SELECT * FROM t WHERE a = %s AND b = %s");
        assert_eq!(sql, "SELECT * FROM t WHERE a = $1 AND b = $2");
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn named_placeholders_keep_order() {
        let (sql, names) = translate_placeholders("SELECT %(x)s, %(y)s");
        assert_eq!(sql, "SELECT $1, $2");
        assert_eq!(names, vec!["x", "y"]);
    }

    #[test]
    fn double_percent_unescapes() {
        let (sql, names) = translate_placeholders("SELECT 50 %% 7");
        assert_eq!(sql, "SELECT 50 % 7");
        assert!(names.is_empty());
    }

    #[test]
    fn percent_inside_string_literal_is_left_alone() {
        let (sql, names) = translate_placeholders("SELECT * FROM t WHERE a LIKE '%save%'");
        assert_eq!(sql, "SELECT * FROM t WHERE a LIKE '%save%'");
        assert!(names.is_empty());
    }

    #[test]
    fn percent_inside_quoted_identifier_is_left_alone() {
        let (sql, _) = translate_placeholders(r#"SELECT "od%sd" FROM t"#);
        assert_eq!(sql, r#"SELECT "od%sd" FROM t"#);
    }

    #[test]
    fn placeholder_inside_line_comment_is_left_alone() {
        let (sql, names) = translate_placeholders("SELECT 1 -- pct %s here\n, 2");
        assert!(names.is_empty(), "got {names:?}");
        assert_eq!(sql, "SELECT 1 -- pct %s here\n, 2");
    }

    #[test]
    fn placeholder_inside_block_comment_is_left_alone() {
        let (sql, names) = translate_placeholders("SELECT /* %s /* nested %s */ */ 1, %s");
        assert_eq!(names.len(), 1, "only the real placeholder: {sql}");
        assert!(sql.ends_with("1, $1"), "got {sql}");
    }

    #[test]
    fn comment_does_not_swallow_the_rest_of_the_statement() {
        let (sql, names) = translate_placeholders("SELECT %s -- c\n, %s");
        assert_eq!(names.len(), 2, "got {sql}");
    }

    #[test]
    fn row_returning_statements_detected() {
        assert!(returns_rows("SELECT 1"));
        assert!(returns_rows("  with x as (select 1) select * from x"));
        assert!(returns_rows("INSERT INTO t (a) VALUES (1) RETURNING id"));
        assert!(!returns_rows("UPDATE t SET a = 1"));
        assert!(!returns_rows("DELETE FROM t"));
    }

    #[test]
    fn returning_is_matched_as_a_keyword_not_a_substring() {
        assert!(!returns_rows("UPDATE t SET note = 'returning tomorrow'"));
        assert!(!returns_rows("UPDATE t SET returning_date = now()"));
        assert!(!returns_rows(r#"UPDATE t SET "returning" = 1"#));
        assert!(returns_rows("UPDATE t SET a = 1 RETURNING id"));
        assert!(returns_rows("DELETE FROM t WHERE a = 1 returning id"));
    }
}

#[pyclass]
pub struct RustCopy {
    sink: Option<std::pin::Pin<Box<tokio_postgres::CopyInSink<bytes::Bytes>>>>,
    handle: Handle,
    types: Vec<Type>,
    buf: bytes::BytesMut,
    rows: i64,
    started: bool,
    binary: bool,
    row_mode: bool,
}

fn binary_encodable(py: Python<'_>, ty: &Type) -> bool {
    let none = py.None();
    let Ok(boxed) = py_to_sql(py, none.bind(py), ty) else {
        return false;
    };
    let mut scratch = bytes::BytesMut::new();
    boxed.to_sql_checked(ty, &mut scratch).is_ok()
}

fn statement_is_binary_copy(statement: &str) -> bool {
    let lowered = statement.to_ascii_lowercase();
    match lowered.rfind("from stdin") {
        Some(at) => lowered[at..].contains("binary"),
        None => false,
    }
}

fn py_to_copy_text(v: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    use pyo3::types::{PyBool, PyBytes, PyFloat, PyInt, PyString};

    if v.is_none() {
        return Ok(None);
    }
    if let Some(jv) = json_wrapper_value(v)? {
        return Ok(Some(
            serde_json::to_string(&jv).map_err(|e| rerr(e.to_string()))?,
        ));
    }
    if v.is_instance_of::<PyBool>() {
        return Ok(Some(
            if v.extract::<bool>()? { "t" } else { "f" }.to_string(),
        ));
    }
    if v.is_instance_of::<PyBytes>() {
        let raw = v.extract::<Vec<u8>>()?;
        let mut out = String::with_capacity(raw.len() * 2 + 2);
        out.push_str("\\x");
        for b in raw {
            out.push_str(&format!("{b:02x}"));
        }
        return Ok(Some(out));
    }
    if v.is_instance_of::<PyInt>()
        || v.is_instance_of::<PyFloat>()
        || v.is_instance_of::<PyString>()
    {
        return Ok(Some(v.str()?.to_string_lossy().into_owned()));
    }
    if v.is_instance_of::<pyo3::types::PyList>() || v.is_instance_of::<pyo3::types::PyTuple>() {
        return Ok(Some(py_seq_to_array_literal(v)?));
    }
    if v.is_instance_of::<pyo3::types::PyDict>() {
        let jv = py_to_json(v)?;
        return Ok(Some(
            serde_json::to_string(&jv).map_err(|e| rerr(e.to_string()))?,
        ));
    }
    Ok(Some(v.str()?.to_string_lossy().into_owned()))
}

fn py_seq_to_array_literal(v: &Bound<'_, PyAny>) -> PyResult<String> {
    let mut out = String::from("{");
    for (i, item) in v.try_iter()?.enumerate() {
        let item = item?;
        if i > 0 {
            out.push(',');
        }
        if item.is_none() {
            out.push_str("NULL");
            continue;
        }
        if item.is_instance_of::<pyo3::types::PyList>()
            || item.is_instance_of::<pyo3::types::PyTuple>()
        {
            out.push_str(&py_seq_to_array_literal(&item)?);
            continue;
        }
        let rendered = match py_to_copy_text(&item)? {
            Some(t) => t,
            None => {
                out.push_str("NULL");
                continue;
            }
        };
        let needs_quotes = rendered.is_empty()
            || rendered.eq_ignore_ascii_case("null")
            || rendered
                .chars()
                .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace());
        if needs_quotes {
            out.push('"');
            for c in rendered.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(&rendered);
        }
    }
    out.push('}');
    Ok(out)
}

fn copy_text_escape(field: &str, out: &mut String) {
    for c in field.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
}

const COPY_BINARY_HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0";

const COPY_BINARY_TRAILER: &[u8] = &[0xff, 0xff];

const COPY_FLUSH_AT: usize = 64 * 1024;

impl RustCopy {
    fn flush(&mut self, py: Python<'_>, force: bool) -> PyResult<()> {
        if self.buf.is_empty() || (!force && self.buf.len() < COPY_FLUSH_AT) {
            return Ok(());
        }
        let chunk = self.buf.split().freeze();
        let sink = self
            .sink
            .as_mut()
            .ok_or_else(|| rerr("COPY already finished"))?;
        use futures_util::SinkExt;
        let handle = &self.handle;
        py.detach(|| handle.block_on(sink.send(chunk)))
            .map_err(db_err)
    }
}

#[pymethods]
impl RustCopy {
    fn set_types(&mut self, py: Python<'_>, types: Vec<u32>) -> PyResult<()> {
        let mut resolved = Vec::with_capacity(types.len());
        for oid in types {
            let ty = Type::from_oid(oid)
                .filter(|t| binary_encodable(py, t))
                .ok_or_else(|| {
                    rerr(format!(
                        "COPY column type oid {oid} is not one this cursor can \
                         encode in binary; refusing rather than writing it as \
                         text into a binary stream"
                    ))
                })?;
            resolved.push(ty);
        }
        self.types = resolved;
        Ok(())
    }

    fn write(&mut self, py: Python<'_>, data: Bound<'_, PyAny>) -> PyResult<()> {
        use bytes::BufMut;
        let bytes: Vec<u8> = if let Ok(b) = data.extract::<Vec<u8>>() {
            b
        } else if let Ok(text) = data.extract::<String>() {
            if self.binary {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "cannot copy str data in binary mode: use bytes instead",
                ));
            }
            text.into_bytes()
        } else {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "COPY write() takes bytes or str",
            ));
        };
        self.started = true;
        self.buf.put_slice(&bytes);
        self.flush(py, false)
    }

    fn write_row(&mut self, py: Python<'_>, row: Bound<'_, PyAny>) -> PyResult<()> {
        use bytes::BufMut;
        self.row_mode = true;
        if !self.started {
            if self.binary {
                self.buf.put_slice(COPY_BINARY_HEADER);
            }
            self.started = true;
        }
        let values: Vec<Bound<'_, PyAny>> = row.try_iter()?.collect::<PyResult<_>>()?;
        if !self.binary {
            let mut line = String::with_capacity(values.len() * 16);
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    line.push('\t');
                }
                match py_to_copy_text(v)? {
                    None => line.push_str("\\N"),
                    Some(field) => copy_text_escape(&field, &mut line),
                }
            }
            line.push('\n');
            self.buf.put_slice(line.as_bytes());
            self.rows += 1;
            return self.flush(py, false);
        }
        if self.types.len() != values.len() {
            return Err(rerr(format!(
                "COPY row has {} values but {} column types were declared",
                values.len(),
                self.types.len()
            )));
        }
        self.buf.put_i16(values.len() as i16);
        for (v, ty) in values.iter().zip(self.types.iter()) {
            if v.is_none() {
                self.buf.put_i32(-1);
                continue;
            }
            let boxed = py_to_sql(py, v, ty)?;
            let start = self.buf.len();
            self.buf.put_i32(0);
            match boxed.to_sql_checked(ty, &mut self.buf) {
                Ok(tokio_postgres::types::IsNull::Yes) => {
                    self.buf.truncate(start);
                    self.buf.put_i32(-1);
                }
                Ok(tokio_postgres::types::IsNull::No) => {
                    let len = (self.buf.len() - start - 4) as i32;
                    self.buf[start..start + 4].copy_from_slice(&len.to_be_bytes());
                }
                Err(e) => return Err(rerr(format!("COPY encode failed: {e}"))),
            }
        }
        self.rows += 1;
        self.flush(py, false)
    }

    fn finish(&mut self, py: Python<'_>) -> PyResult<i64> {
        use bytes::BufMut;
        if self.binary {
            if !self.started {
                self.buf.put_slice(COPY_BINARY_HEADER);
                self.started = true;
                self.buf.put_slice(COPY_BINARY_TRAILER);
            } else if self.row_mode {
                self.buf.put_slice(COPY_BINARY_TRAILER);
            }
            // else: raw `write` only, and the caller owns the trailer.
        }
        self.flush(py, true)?;
        let mut sink = self
            .sink
            .take()
            .ok_or_else(|| rerr("COPY already finished"))?;
        let handle = self.handle.clone();
        let n = py
            .detach(move || handle.block_on(async { sink.as_mut().finish().await }))
            .map_err(db_err)?;
        Ok(n as i64)
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*args))]
    fn __exit__(&mut self, py: Python<'_>, args: &Bound<'_, PyTuple>) -> PyResult<bool> {
        let raised = args.get_item(0).map(|exc| !exc.is_none()).unwrap_or(false);
        if raised {
            self.sink.take();
            return Ok(false);
        }
        if self.sink.is_some() {
            self.finish(py)?;
        }
        Ok(false)
    }

    #[getter]
    fn rowcount(&self) -> i64 {
        self.rows
    }
}

const DEFAULT_DB_THREADS: usize = 4;

#[pyclass]
pub struct RustDb {
    dsn: String,

    rt: std::sync::Mutex<RuntimeFor>,

    worker_threads: usize,
}

struct RuntimeFor {
    pid: u32,
    handle: Handle,

    owned: Option<Arc<tokio::runtime::Runtime>>,
}

impl RustDb {
    pub fn new(dsn: String, rt: Arc<tokio::runtime::Runtime>) -> Self {
        Self::with_threads(dsn, rt, DEFAULT_DB_THREADS)
    }

    fn with_threads(dsn: String, rt: Arc<tokio::runtime::Runtime>, worker_threads: usize) -> Self {
        RustDb {
            dsn,
            worker_threads,
            rt: std::sync::Mutex::new(RuntimeFor {
                pid: std::process::id(),
                handle: rt.handle().clone(),
                owned: Some(rt),
            }),
        }
    }

    fn build_runtime(worker_threads: usize) -> std::io::Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads.max(1))
            .enable_all()
            .thread_name("rustorm-db")
            .build()
    }

    fn handle(&self) -> PyResult<Handle> {
        let mut slot = self.rt.lock().unwrap();
        let pid = std::process::id();
        if slot.pid != pid {
            tracing::warn!(
                target: "odoo_kernel::cursor",
                old_pid = slot.pid, pid,
                "the tokio runtime was built before a fork; rebuilding for this process"
            );
            let rt = Self::build_runtime(self.worker_threads)
                .map_err(|e| rerr(format!("cannot rebuild the tokio runtime after fork: {e}")))?;

            if let Some(dead) = slot.owned.take() {
                std::mem::forget(dead);
            }
            slot.handle = rt.handle().clone();
            slot.owned = Some(Arc::new(rt));
            slot.pid = pid;
        }
        Ok(slot.handle.clone())
    }
}

#[pymethods]
impl RustDb {
    #[new]
    #[pyo3(signature = (dsn, worker_threads = DEFAULT_DB_THREADS))]
    fn py_new(dsn: String, worker_threads: usize) -> PyResult<Self> {
        let rt = Self::build_runtime(worker_threads)
            .map_err(|e| rerr(format!("cannot start the tokio runtime: {e}")))?;
        Ok(Self::with_threads(dsn, Arc::new(rt), worker_threads))
    }

    #[getter]
    fn runtime_pid(&self) -> u32 {
        self.rt.lock().unwrap().pid
    }

    #[pyo3(signature = (dsn = None))]
    pub fn connect(&self, py: Python<'_>, dsn: Option<String>) -> PyResult<RustConn> {
        let dsn = dsn.unwrap_or_else(|| self.dsn.clone());
        let handle = self.handle()?;
        let for_conn = handle.clone();
        let client = py
            .detach(move || {
                handle.block_on(async {
                    let (client, conn) =
                        tokio_postgres::connect(&dsn, tokio_postgres::NoTls).await?;
                    tokio::spawn(async move {
                        let _ = conn.await;
                    });
                    Ok::<_, tokio_postgres::Error>(client)
                })
            })
            .map_err(db_err)?;
        Ok(RustConn::new(Arc::new(client), for_conn))
    }
}

#[cfg(test)]
mod prepared_invalidation_tests {
    use super::statement_invalidates_prepared;

    #[test]
    fn it_matches_psycopgs_rollback_and_drop_tags() {
        assert!(statement_invalidates_prepared("ROLLBACK"));
        assert!(statement_invalidates_prepared(
            "ROLLBACK TO SAVEPOINT sp_undo"
        ));
        assert!(statement_invalidates_prepared("rollback to savepoint sp"));
        assert!(statement_invalidates_prepared("  \n ROLLBACK "));
        assert!(statement_invalidates_prepared("DROP TABLE t"));
        assert!(statement_invalidates_prepared("drop index if exists i"));

        assert!(!statement_invalidates_prepared("RELEASE SAVEPOINT sp_keep"));
        assert!(!statement_invalidates_prepared("COMMIT"));
        assert!(!statement_invalidates_prepared("SAVEPOINT sp"));
        assert!(!statement_invalidates_prepared(
            "SELECT id FROM res_partner"
        ));
        assert!(!statement_invalidates_prepared("SELECT dropped FROM t"));
        assert!(!statement_invalidates_prepared("UPDATE t SET dropping = 1"));
    }
}

#[cfg(test)]
mod copy_format_tests {
    use super::statement_is_binary_copy;

    #[test]
    fn the_format_is_read_from_the_options_after_from_stdin() {
        assert!(statement_is_binary_copy(
            r#"COPY "t" ("a", "b") FROM STDIN (FORMAT BINARY)"#
        ));
        assert!(statement_is_binary_copy(
            "COPY t (a) FROM STDIN WITH (FORMAT BINARY)"
        ));
        assert!(statement_is_binary_copy(
            "copy t (a) from stdin (format binary)"
        ));

        assert!(!statement_is_binary_copy(r#"COPY "t" ("a") FROM STDIN"#));
        assert!(!statement_is_binary_copy(
            r#"COPY "t" ("a") FROM STDIN (ON_ERROR ignore)"#
        ));
    }

    #[test]
    fn a_table_or_column_called_binary_is_not_a_binary_stream() {
        assert!(!statement_is_binary_copy(
            r#"COPY "binary" ("binary_data") FROM STDIN"#
        ));
        assert!(statement_is_binary_copy(
            r#"COPY "binary" ("binary_data") FROM STDIN (FORMAT BINARY)"#
        ));
    }
}

#[cfg(test)]
mod copy_type_tests {
    use tokio_postgres::types::Type;

    const PSYCOPG_BINARY_OIDS: &[u32] = &[
        16, 17, 19, 20, 21, 23, 25, 26, 114, 199, 650, 651, 700, 701, 869, 1000, 1001, 1003, 1005,
        1007, 1009, 1015, 1016, 1021, 1022, 1028, 1041, 1043, 1082, 1083, 1114, 1115, 1182, 1183,
        1184, 1185, 1186, 1187, 1231, 1266, 1270, 1700, 2950, 2951, 3802, 3807, 3905, 3907, 3909,
        3911, 3913, 3927, 6150, 6151, 6152, 6153, 6155, 6157,
    ];

    #[test]
    fn every_oid_psycopg_dumps_in_binary_is_one_this_cursor_can_encode() {
        let unknown: Vec<u32> = PSYCOPG_BINARY_OIDS
            .iter()
            .copied()
            .filter(|oid| Type::from_oid(*oid).is_none())
            .collect();
        assert!(
            unknown.is_empty(),
            "psycopg will hand these OIDs to a binary COPY and tokio-postgres \
             does not know them, so `set_types` would encode them as TEXT and \
             PostgreSQL would misread the bytes: {unknown:?}"
        );
    }
}
