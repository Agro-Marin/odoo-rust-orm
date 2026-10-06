use std::time::Instant;

use odoo_kernel::domain;
use serde_json::json;

#[test]
#[ignore = "synthetic parser benchmark; run with --release --ignored --nocapture"]
fn flat_domain_scaling() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_test_writer()
        .try_init();
    for terms in [2_000, 4_000, 8_000, 16_000] {
        // a OR (b OR (...)): flattening must not copy the growing suffix
        // into a fresh vector for every binary operator.
        let mut tokens = Vec::new();
        for i in 0..terms {
            if i + 1 < terms {
                tokens.push(json!("|"));
            }
            tokens.push(json!(["id", "=", i]));
        }
        let input = json!(tokens);
        let mut elapsed = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            let parsed = domain::parse(std::hint::black_box(&input)).unwrap();
            elapsed.push(started.elapsed());
            let domain::Node::Or(children) = parsed else {
                panic!("expected a flat OR");
            };
            assert_eq!(children.len(), terms);
        }
        elapsed.sort();
        tracing::debug!(terms, median = ?elapsed[2], "domain parse only; excludes input construction and tree drop");
    }
}
