use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{NaiveDate, NaiveDateTime};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};
use pyo3::IntoPyObjectExt;
use tokio::runtime::Handle;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::Client;

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
    PyRuntimeError::new_err(format!("SQLSTATE:{code}|{msg}"))
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

fn py_to_sql(
    py: Python<'_>,
    v: &Bound<'_, PyAny>,
    ty: &Type,
) -> PyResult<Box<dyn ToSql + Sync + Send>> {
    if let Ok(cls) = v.get_type().name() {
        if cls == "Json" || cls == "Jsonb" {
            if let Ok(inner) = v.getattr("obj") {
                let jv = py_to_json(&inner)?;
                return Ok(Box::new(jv));
            }
        }
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

            _ => match ty.kind() {
                tokio_postgres::types::Kind::Array(elem) => match *elem {
                    Type::BOOL => Box::new(None::<Vec<Option<bool>>>),
                    Type::INT2 => Box::new(None::<Vec<Option<i16>>>),
                    Type::INT4 => Box::new(None::<Vec<Option<i32>>>),
                    Type::INT8 => Box::new(None::<Vec<Option<i64>>>),
                    Type::FLOAT4 => Box::new(None::<Vec<Option<f32>>>),
                    Type::FLOAT8 => Box::new(None::<Vec<Option<f64>>>),
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
        Type::NUMERIC => {
            let s: String = v.str()?.extract()?;
            Box::new(rust_decimal::Decimal::from_str(&s).map_err(rerr)?)
        }
        Type::DATE => {
            let s: String = v.str()?.extract()?;
            Box::new(NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(rerr)?)
        }
        Type::TIMESTAMP => {
            let s: String = v.str()?.extract()?;
            let dt = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f")
                .or_else(|_| {
                    NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                        .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
                })
                .map_err(rerr)?;
            Box::new(dt)
        }
        Type::TIMESTAMPTZ => {
            let s: String = v.str()?.extract()?;
            Box::new(parse_tstz(&s)?)
        }

        Type::JSON | Type::JSONB => match v.extract::<String>() {
            Ok(s) => Box::new(
                serde_json::from_str::<serde_json::Value>(&s)
                    .unwrap_or(serde_json::Value::String(s)),
            ),
            Err(_) => Box::new(py_to_json(v)?),
        },
        Type::BYTEA => Box::new(v.extract::<Vec<u8>>()?),
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
    datetime: Py<PyAny>,
    utc: Py<PyAny>,
}

static DT_TYPES: pyo3::sync::PyOnceLock<DateTimeTypes> = pyo3::sync::PyOnceLock::new();

fn dt_types(py: Python<'_>) -> PyResult<&'static DateTimeTypes> {
    DT_TYPES.get_or_try_init(py, || {
        let m = py.import("datetime")?;
        Ok(DateTimeTypes {
            date: m.getattr("date")?.unbind(),
            datetime: m.getattr("datetime")?.unbind(),
            utc: m.getattr("timezone")?.getattr("utc")?.unbind(),
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
                Some(v) => {
                    use rust_decimal::prelude::ToPrimitive;
                    v.to_f64().unwrap_or(f64::NAN).into_py_any(py)?
                }
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
                match *elem {
                    Type::BOOL => arr!(bool),
                    Type::INT2 => arr!(i16),
                    Type::INT4 => arr!(i32),
                    Type::INT8 => arr!(i64),
                    Type::FLOAT4 => arr!(f32),
                    Type::FLOAT8 => arr!(f64),
                    Type::TEXT | Type::VARCHAR | Type::NAME | Type::BPCHAR => arr!(String),

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
                        )))
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
                _ => Type::TEXT_ARRAY,
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
                    boxed.push(py_to_sql(py, v, ty)?);
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
        })
    }

    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        self.end_tx(py, "COMMIT")
    }

    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        self.end_tx(py, "ROLLBACK")
    }

    fn clear_prepared(&self) {
        self.stmt_cache.lock().unwrap().clear();
        self.kernel_stmts.clear();
    }

    #[getter]
    fn prepared_count(&self) -> usize {
        self.stmt_cache.lock().unwrap().len() + self.kernel_stmts.len()
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
    fn set_types(&mut self, types: Vec<u32>) -> PyResult<()> {
        self.types = types
            .into_iter()
            .map(|oid| Type::from_oid(oid).unwrap_or(Type::TEXT))
            .collect();
        Ok(())
    }

    fn write_row(&mut self, py: Python<'_>, row: Bound<'_, PyAny>) -> PyResult<()> {
        use bytes::BufMut;
        if !self.started {
            self.buf.put_slice(COPY_BINARY_HEADER);
            self.started = true;
        }
        let values: Vec<Bound<'_, PyAny>> = row.try_iter()?.collect::<PyResult<_>>()?;
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
        if !self.started {
            self.buf.put_slice(COPY_BINARY_HEADER);
            self.started = true;
        }
        self.buf.put_slice(COPY_BINARY_TRAILER);
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

    pub fn connect(&self, py: Python<'_>) -> PyResult<RustConn> {
        let dsn = self.dsn.clone();
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
