use std::fmt::Write as _;

use pyo3::prelude::*;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

#[derive(Default)]
struct Render {
    message: String,
    fields: String,
}

impl Visit for Render {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }
}

/// The fields a span was opened with, rendered once and kept for every event
/// inside it. Without this an event from the reader or the compiler carries no
/// trace of the request it belongs to, which is the first thing a reader wants.
struct SpanFields(String);

pub struct PythonLogLayer;

fn py_level(level: &Level) -> i32 {
    match *level {
        Level::ERROR => 40,
        Level::WARN => 30,
        Level::INFO => 20,
        Level::DEBUG => 10,
        Level::TRACE => TRACE_LEVEL,
    }
}

// Python has no level below DEBUG; `install` registers this one as "TRACE" so
// `--log-handler odoo.rust_kernel:TRACE` resolves and reads like any other.
const TRACE_LEVEL: i32 = 5;

fn logger_name(target: &str) -> String {
    let target = target.replace("::", ".");
    match target.strip_prefix("odoo_kernel.") {
        Some(rest) => format!("odoo.rust_kernel.{rest}"),
        None => format!("odoo.rust_kernel.{target}"),
    }
}

impl<S> Layer<S> for PythonLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut render = Render::default();
        attrs.record(&mut render);
        let mut text = String::new();
        if !render.message.is_empty() {
            text.push_str(&render.message);
        }
        text.push_str(&render.fields);
        span.extensions_mut().insert(SpanFields(text));
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut render = Render::default();
        event.record(&mut render);

        let mut context = String::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                let _ = write!(context, "[{}", span.name());
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    context.push_str(&fields.0);
                }
                context.push_str("] ");
            }
        }

        let msg = format!("{context}{}{}", render.message, render.fields);
        let level = py_level(meta.level());
        let name = logger_name(meta.target());

        let _ = Python::attach(|py| -> PyResult<()> {
            let logger = logger_for(py, &name)?;
            let logger = logger.bind(py);
            // asking the Python logger first keeps a level Odoo has muted from
            // paying for the call; `RUSTORM_LOG` decides what reaches here at
            // all, and this decides what Odoo's own handlers then print
            if !logger.call_method1("isEnabledFor", (level,))?.is_truthy()? {
                return Ok(());
            }
            logger.call_method1("log", (level, msg))?;
            Ok(())
        });
    }
}

type LoggerCache = std::sync::Mutex<std::collections::HashMap<String, Py<PyAny>>>;
static LOGGERS: std::sync::OnceLock<LoggerCache> = std::sync::OnceLock::new();

/// `logging.getLogger` walks a lock and a name tree on every call, which at
/// DEBUG over a whole request is thousands of calls; the handle it returns is
/// stable for the life of the process, so it is kept.
fn logger_for(py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
    let cache = LOGGERS.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(name) {
        return Ok(hit.clone_ref(py));
    }
    let logger: Py<PyAny> = py
        .import("logging")?
        .call_method1("getLogger", (name,))?
        .unbind();
    cache
        .lock()
        .unwrap()
        .insert(name.to_string(), logger.clone_ref(py));
    Ok(logger)
}

pub fn install() {
    use tracing_subscriber::prelude::*;
    // `RUSTORM_LOG` is a standard `tracing` EnvFilter string. Per subsystem:
    // `odoo_kernel::scan=debug`; everything at once: `odoo_kernel=debug`.
    // Whatever passes it is then offered to Odoo's own logger under
    // `odoo.rust_kernel.<target>`, so a handler there narrows it further.
    let filter = tracing_subscriber::EnvFilter::try_from_env("RUSTORM_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(PythonLogLayer)
        .try_init()
        .is_ok();
    if installed {
        let _ = Python::attach(|py| -> PyResult<()> {
            let logging = py.import("logging")?;
            logging.call_method1("addLevelName", (TRACE_LEVEL, "TRACE"))?;
            // Odoo resolves a `--log-handler name:LEVEL` with
            // `getattr(logging, LEVEL, logging.INFO)` (`odoo/logutils.py`), so
            // `addLevelName` alone leaves `:TRACE` silently reading as INFO.
            // The module attribute is what makes the finest level reachable
            // from the command line at all.
            logging.setattr("TRACE", TRACE_LEVEL)?;
            Ok(())
        });
        tracing::debug!(
            target: "odoo_kernel::bridge",
            filter = %std::env::var("RUSTORM_LOG").unwrap_or_else(|_| "warn".into()),
            "kernel logging bridged into odoo.rust_kernel.*"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::logger_name;

    #[test]
    fn a_kernel_target_keeps_its_subsystem_and_nothing_else_is_doubled() {
        assert_eq!(logger_name("odoo_kernel::scan"), "odoo.rust_kernel.scan");
        assert_eq!(
            logger_name("odoo_kernel::orm::inner"),
            "odoo.rust_kernel.orm.inner"
        );
        assert_eq!(logger_name("engine_py"), "odoo.rust_kernel.engine_py");
    }
}
