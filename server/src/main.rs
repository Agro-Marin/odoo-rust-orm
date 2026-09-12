use std::time::Instant;

mod http;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value as Json;

use odoo_kernel::orm::{Orm, Request, StmtCache};
use odoo_kernel::registry::Registry;

#[derive(Parser)]
#[command(name = "rustorm", about = "Odoo ORM engine in Rust (read path)")]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "RUSTORM_DB",
        default_value = "rustorm_probe"
    )]
    db: String,

    #[arg(long, global = true)]
    export: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    Inspect {
        model: Option<String>,
    },

    Query {
        #[arg(long)]
        model: String,
        #[arg(long, default_value = "search_read")]
        method: String,
        #[arg(long, default_value = "[]")]
        domain: String,
        #[arg(long, value_delimiter = ',')]
        fields: Vec<String>,
        #[arg(long)]
        limit: Option<u64>,
        #[arg(long)]
        offset: Option<u64>,
        #[arg(long)]
        order: Option<String>,
        #[arg(long, value_delimiter = ',')]
        groupby: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        aggregates: Vec<String>,

        #[arg(long)]
        uid: Option<String>,
        #[arg(long)]
        su: bool,
        #[arg(long)]
        lang: Option<String>,

        #[arg(long, value_delimiter = ',')]
        company: Vec<i32>,

        #[arg(long)]
        no_active_test: bool,
    },

    RunCorpus {
        #[arg(long)]
        file: String,
    },

    Bench {
        #[arg(long)]
        file: String,
        #[arg(long, default_value_t = 20)]
        iters: u32,
        #[arg(long)]
        json: bool,
    },

    Serve {
        #[arg(long, default_value_t = 8072)]
        port: u16,
        #[arg(long, default_value_t = 16)]
        pool: usize,
        #[arg(long, default_value_t = 30)]
        request_timeout_secs: u64,
        #[arg(long, default_value_t = 5)]
        acquire_timeout_secs: u64,
    },
}

async fn connect(db: &str) -> Result<tokio_postgres::Client> {
    let conn_str = odoo_kernel::config::dsn_for(Some(db));
    odoo_kernel::connect::connect(&conn_str)
        .await
        .with_context(|| format!("connecting to {db}"))
}

async fn load_registry(
    client: &tokio_postgres::Client,
    export: &Option<String>,
) -> Result<Registry> {
    match export {
        Some(path) => {
            let text = std::fs::read_to_string(path)?;
            let value: Json = serde_json::from_str(&text)?;
            Registry::from_export(client, &value).await
        }
        None => anyhow::bail!(
            "this command needs --export: the ir_model bootstrap cannot tell which \
             models override the read path, marks none of them pure, and refuses \
             every dispatch. `inspect` is the one command that reads the bootstrap."
        ),
    }
}

struct Session {
    client: tokio_postgres::Client,
    registry: Registry,
    caches: std::sync::Arc<odoo_kernel::orm::Caches>,
    stmts: StmtCache,

    registry_ms: f64,
}

impl Session {
    async fn open(db: &str, export: &Option<String>) -> Result<Self> {
        let client = connect(db).await?;
        let t0 = Instant::now();
        let registry = load_registry(&client, export).await?;
        Ok(Session {
            client,
            registry,
            caches: std::sync::Arc::new(odoo_kernel::orm::Caches::default()),
            stmts: StmtCache::default(),
            registry_ms: t0.elapsed().as_secs_f64() * 1000.0,
        })
    }

    fn orm(&self) -> Orm<'_> {
        Orm::new(
            &self.registry,
            &self.client,
            self.caches.clone(),
            &self.stmts,
        )
    }
}

async fn data_fingerprint(client: &tokio_postgres::Client, tables: &[String]) -> Option<i64> {
    let _ = client
        .execute("SELECT pg_stat_force_next_flush()", &[])
        .await;
    client
        .query_one(
            "SELECT coalesce(sum(n_tup_ins + n_tup_upd + n_tup_del), 0)::bigint
               FROM pg_stat_user_tables
              WHERE schemaname = current_schema AND relname = ANY($1)",
            &[&tables],
        )
        .await
        .ok()
        .map(|r| r.get(0))
}

pub(crate) fn error_kind(e: &anyhow::Error) -> &'static str {
    use odoo_kernel::error::ErrorKind;
    match ErrorKind::of(e) {
        ErrorKind::Database | ErrorKind::Internal => "internal",
        ErrorKind::Refused | ErrorKind::AccessDenied | ErrorKind::RegistryStale => "refusal",
    }
}

fn percentiles(times: &mut [f64]) -> (f64, f64, f64) {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = times[times.len() / 2];
    let p95 = times[(times.len() as f64 * 0.95) as usize % times.len()];
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    (p50, p95, mean)
}

fn load_corpus(path: &str) -> Result<Vec<Request>> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn init_logging() {
    let default = if std::env::var_os("POC_TRACE").is_some() {
        "warn,odoo_kernel::sql=debug"
    } else {
        "warn"
    };
    let requested = std::env::var("RUSTORM_LOG").ok().filter(|v| !v.is_empty());
    let filter = tracing_subscriber::EnvFilter::try_from_env("RUSTORM_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let effective = filter.to_string();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .try_init();
    // `RUSTORM_LOG=odoo_kernel=debug` turns on every kernel target at once;
    // one target at a time is `odoo_kernel::scan=debug`. Saying which filter
    // is live is what stops a session concluding "nothing happened" from a
    // filter that was never in effect.
    tracing::debug!(
        target: "odoo_kernel::http",
        filter = %effective,
        from_env = requested.is_some(),
        "logging initialised"
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Inspect { model } => {
            let client = connect(&cli.db).await?;
            let t0 = Instant::now();
            let registry = match &cli.export {
                Some(_) => load_registry(&client, &cli.export).await?,
                None => Registry::load(&client).await?,
            };
            tracing::info!(
                models = registry.models.len(),
                ms = t0.elapsed().as_secs_f64() * 1000.0,
                "registry loaded"
            );
            match model {
                Some(m) => {
                    let model = registry.get(&m)?;
                    println!(
                        "{} (table {}, order '{}')",
                        model.name, model.table, model.order
                    );
                    let mut names: Vec<_> = model.fields.keys().collect();
                    names.sort();
                    for n in names {
                        let f = &model.fields[n];
                        println!(
                            "  {:30} {:?} pg={} col={} nn={} tr={}",
                            f.name, f.ttype, f.pg_type, f.has_column, f.not_null, f.translated
                        );
                    }
                }
                None => {
                    let mut names: Vec<_> = registry.models.keys().collect();
                    names.sort();
                    for n in names {
                        let m = &registry.models[n];
                        println!(
                            "{:50} {:4} fields  order '{}'",
                            m.name,
                            m.fields.len(),
                            m.order
                        );
                    }
                }
            }
        }
        Cmd::Query {
            model,
            method,
            domain,
            fields,
            limit,
            offset,
            order,
            groupby,
            aggregates,
            uid,
            su,
            lang,
            company,
            no_active_test,
        } => {
            let session = Session::open(&cli.db, &cli.export).await?;
            let orm = session.orm();
            let req = Request {
                id: None,
                registry_sequence: None,
                model,
                method,
                domain: serde_json::from_str(&domain)?,
                fields,
                limit,
                offset,
                order,
                groupby: serde_json::json!(groupby),
                aggregates,
                uid: uid.map(|u| match u.parse::<i32>() {
                    Ok(n) => odoo_kernel::orm::UidSpec::Id(n),
                    Err(_) => odoo_kernel::orm::UidSpec::Symbol(u),
                }),
                su,
                lang,
                allowed_company_ids: (!company.is_empty()).then_some(company),
                groupby_labels: None,
                active_test: no_active_test.then_some(false),
                x2many_active_test: None,
                tz: None,
                root_active_test: None,
                trusted_domain: false,
            };
            let raw = orm.dispatch_in_transaction(&req).await?;
            let value: Json = serde_json::from_str(&raw)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Cmd::RunCorpus { file } => {
            let session = Session::open(&cli.db, &cli.export).await?;
            let orm = session.orm();
            let corpus = load_corpus(&file)?;
            let mut tables: Vec<String> = corpus
                .iter()
                .filter_map(|c| session.registry.get(&c.model).ok().map(|m| m.table.clone()))
                .collect();
            tables.sort();
            tables.dedup();
            let fingerprint = data_fingerprint(&session.client, &tables).await;
            let mut out = format!(
                "{{\n\"db\": {},\n\"has_unaccent\": {},\n\"data_fingerprint\": {},\n\"cases\": [\n",
                serde_json::to_string(&cli.db)?,
                session.registry.has_unaccent,
                match fingerprint {
                    Some(f) => f.to_string(),
                    None => "null".into(),
                }
            );
            for (i, req) in corpus.iter().enumerate() {
                let case_id = serde_json::to_string(&req.id.clone().unwrap_or_default())?;
                if i > 0 {
                    out.push_str(",\n");
                }
                match orm.dispatch_in_transaction(req).await {
                    Ok(raw) => {
                        out.push_str(&format!(r#"{{"id":{case_id},"ok":true,"result":{raw}}}"#))
                    }
                    Err(e) => {
                        let msg = serde_json::to_string(&format!("{e:#}"))?;
                        let kind = error_kind(&e);
                        out.push_str(&format!(
                            r#"{{"id":{case_id},"ok":false,"kind":"{kind}","error":{msg}}}"#
                        ))
                    }
                }
            }
            out.push_str("\n]\n}");
            println!("{out}");
        }
        Cmd::Bench { file, iters, json } => {
            let session = Session::open(&cli.db, &cli.export).await?;
            let registry_ms = session.registry_ms;
            let orm = session.orm();
            let corpus = load_corpus(&file)?;

            for req in &corpus {
                let _ = orm.dispatch_in_transaction(req).await;
            }
            let mut per_case: Vec<(String, Vec<f64>)> = corpus
                .iter()
                .map(|c| (c.id.clone().unwrap_or_default(), Vec::new()))
                .collect();
            let bench_start = Instant::now();
            for _ in 0..iters {
                for (i, req) in corpus.iter().enumerate() {
                    let t = Instant::now();
                    let r = orm.dispatch_in_transaction(req).await;
                    let dt = t.elapsed().as_secs_f64() * 1000.0;
                    if r.is_ok() {
                        per_case[i].1.push(dt);
                    }
                }
            }
            let total = bench_start.elapsed().as_secs_f64();
            let mut all: Vec<f64> = Vec::new();
            let mut cases = serde_json::Map::new();
            if !json {
                println!(
                    "{:12} {:>9} {:>9} {:>9}",
                    "case", "p50(ms)", "p95(ms)", "mean(ms)"
                );
            }
            for (id, mut times) in per_case {
                if times.is_empty() {
                    if json {
                        cases.insert(id, Json::Null);
                    } else {
                        println!("{id:12} (errored)");
                    }
                    continue;
                }
                let (p50, p95, mean) = percentiles(&mut times);
                all.extend(&times);
                if json {
                    cases.insert(
                        id,
                        serde_json::json!({"p50": p50, "p95": p95, "mean": mean}),
                    );
                } else {
                    println!("{id:12} {p50:9.3} {p95:9.3} {mean:9.3}");
                }
            }
            let summary = (!all.is_empty()).then(|| {
                let (p50, p95, mean) = percentiles(&mut all);
                (all.len(), p50, p95, mean)
            });
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "db": cli.db,
                        "iters": iters,
                        "transaction": "REPEATABLE READ READ ONLY per call",
                        "registry_load_ms": registry_ms,
                        "elapsed_s": total,
                        "calls": summary.map(|s| s.0).unwrap_or(0),
                        "p50_ms": summary.map(|s| s.1),
                        "p95_ms": summary.map(|s| s.2),
                        "mean_ms": summary.map(|s| s.3),
                        "cases": cases,
                    }))?
                );
            } else if let Some((n, p50, p95, _)) = summary {
                println!(
                    "\nTOTAL {n} calls in {total:.2}s  p50={p50:.3}ms p95={p95:.3}ms  registry_load={registry_ms:.1}ms",
                );
            }
        }
        Cmd::Serve {
            port,
            pool,
            request_timeout_secs,
            acquire_timeout_secs,
        } => {
            let options = crate::http::ServeOptions {
                pool_size: pool.max(1),
                request_timeout: std::time::Duration::from_secs(request_timeout_secs),
                acquire_timeout: std::time::Duration::from_secs(acquire_timeout_secs),
            };
            crate::http::serve(&cli.db, port, cli.export.as_deref(), options).await?;
        }
    }
    Ok(())
}
