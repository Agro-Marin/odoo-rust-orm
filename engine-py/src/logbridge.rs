use std::fmt::Write as _;

use pyo3::prelude::*;
use tracing::field::{Field, Visit};
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

pub struct PythonLogLayer;

fn py_level(level: &Level) -> i32 {
    match *level {
        Level::ERROR => 40,
        Level::WARN => 30,
        Level::INFO => 20,
        Level::DEBUG => 10,
        Level::TRACE => 5,
    }
}

impl<S> Layer<S> for PythonLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut render = Render::default();
        event.record(&mut render);
        let target = meta.target().replace("::", ".");
        let logger = if target.starts_with("odoo_kernel") {
            format!(
                "odoo.rust_kernel.{}",
                target.trim_start_matches("odoo_kernel.")
            )
        } else {
            format!("odoo.rust_kernel.{target}")
        };
        let msg = if render.fields.is_empty() {
            render.message.clone()
        } else {
            format!("{}{}", render.message, render.fields)
        };

        let _ = Python::attach(|py| -> PyResult<()> {
            let logging = py.import("logging")?;
            let logger = logging.call_method1("getLogger", (logger,))?;
            logger.call_method1("log", (py_level(meta.level()), msg))?;
            Ok(())
        });
    }
}

pub fn install() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_env("RUSTPOC_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(PythonLogLayer)
        .try_init();
}
