use std::sync::Arc;
use std::time::Instant;

use odoo_kernel::orm::{Caches, Orm, Request, StmtCache};
use odoo_kernel::registry::Registry;

const DSN: &str = "host=/var/run/postgresql user=marin dbname=rustpoc_probe";

fn req(model: &str, fields: &[&str], limit: Option<u64>) -> Request {
    Request {
        id: None,
        model: model.into(),
        method: "search_read".into(),
        domain: serde_json::json!([]),
        fields: fields.iter().map(|s| s.to_string()).collect(),
        limit,
        offset: None,
        order: None,
        groupby: serde_json::Value::Null,
        aggregates: vec![],
        uid: Some(odoo_kernel::orm::UidSpec::Id(2)),
        su: false,
        lang: None,
        allowed_company_ids: None,
        groupby_labels: None,
        active_test: None,
    }
}

fn p50(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn connection_scoped_cache_beats_request_scoped() {
    let (client, conn) = tokio_postgres::connect(DSN, tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let registry = Registry::load(&client).await.expect("registry");
    let caches = Arc::new(Caches::default());
    let shared_stmts = StmtCache::default();

    let cases = [
        req("res.country", &["name", "code"], Some(50)),
        req("res.partner", &["name", "country_id"], Some(20)),
        req("res.currency", &["name", "symbol"], Some(20)),
    ];

    const ITERS: usize = 200;

    {
        let orm = Orm::new(&registry, &client, caches.clone(), &shared_stmts);
        for _ in 0..20 {
            for c in &cases {
                orm.dispatch(c).await.expect("warmup");
            }
        }
    }

    let connection_scoped = {
        let mut ts = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t = Instant::now();
            for c in &cases {
                let orm = Orm::new(&registry, &client, caches.clone(), &shared_stmts);
                orm.dispatch(c).await.unwrap();
            }
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        p50(ts)
    };

    let request_scoped = {
        let mut ts = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t = Instant::now();
            for c in &cases {
                let per_request = StmtCache::default();
                let orm = Orm::new(&registry, &client, caches.clone(), &per_request);
                orm.dispatch(c).await.unwrap();
            }
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        p50(ts)
    };

    println!("\n--- statement cache scope ({ITERS} iters, 3 queries/iter) ---");
    println!("connection-scoped (now):   {connection_scoped:.3} ms/iter");
    println!("request-scoped (before):   {request_scoped:.3} ms/iter");
    println!(
        "saved: {:.1}%  ({:.3} ms/query)",
        (1.0 - connection_scoped / request_scoped) * 100.0,
        (request_scoped - connection_scoped) / 3.0
    );
    assert!(
        connection_scoped < request_scoped,
        "reusing prepared statements must not be slower"
    );
    assert!(
        !shared_stmts.is_empty(),
        "the shared cache should hold prepared statements"
    );
}
