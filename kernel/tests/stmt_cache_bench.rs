use std::sync::Arc;
use std::time::Instant;

use odoo_kernel::orm::{Caches, Orm, Request, StmtCache};
use odoo_kernel::registry::Registry;

fn req(model: &str, fields: &[&str], limit: Option<u64>) -> Request {
    Request {
        id: None,
        registry_sequence: None,
        model: model.into(),
        method: "search_read".into(),
        domain: serde_json::json!([]),
        fields: fields.iter().map(|s| s.to_string()).collect(),
        limit,
        offset: None,
        order: None,
        groupby: serde_json::Value::Null,
        aggregates: vec![],
        having: serde_json::Value::Null,
        uid: Some(odoo_kernel::orm::UidSpec::Id(2)),
        su: false,
        lang: None,
        allowed_company_ids: None,
        principal_groups: None,
        groupby_labels: None,
        raw_many2one: Vec::new(),
        unredacted_many2one: Vec::new(),
        groupby_hidden_labels_empty: false,
        resolved_rules: Default::default(),
        sql_nonce: None,
        order_fragments: Vec::new(),
        active_test: None,
        x2many_active_test: None,
        tz: None,
        root_active_test: None,
        trusted_domain: false,
    }
}

fn p50(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn connection_scoped_cache_beats_request_scoped() {
    let Some(export) = std::env::var_os("RUSTORM_EXPORT") else {
        eprintln!(
            "SKIP: set RUSTORM_EXPORT to a registry export (the bootstrap marks no model pure)"
        );
        return;
    };
    let dsn = odoo_kernel::config::dsn();
    let client = odoo_kernel::connect::connect(&dsn).await.expect("connect");
    let export: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&export).expect("read export"))
            .expect("parse export");
    let registry = Registry::from_export(&client, &export)
        .await
        .expect("registry");
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

    let mut connection_scoped = Vec::with_capacity(ITERS);
    let mut request_scoped = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let a_first = i % 2 == 0;
        for side in 0..2 {
            let shared = (side == 0) == a_first;
            let t = Instant::now();
            for c in &cases {
                if shared {
                    let orm = Orm::new(&registry, &client, caches.clone(), &shared_stmts);
                    orm.dispatch(c).await.unwrap();
                } else {
                    let per_request = StmtCache::default();
                    let orm = Orm::new(&registry, &client, caches.clone(), &per_request);
                    orm.dispatch(c).await.unwrap();
                }
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if shared {
                connection_scoped.push(ms);
            } else {
                request_scoped.push(ms);
            }
        }
    }
    let connection_scoped = p50(connection_scoped);
    let request_scoped = p50(request_scoped);

    println!("\n--- statement cache scope ({ITERS} iters, 3 queries/iter, A/B interleaved) ---");
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
