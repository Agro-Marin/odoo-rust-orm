use odoo_kernel::db::{Db, StmtCache};

fn scratch_dsn() -> String {
    let dsn = odoo_kernel::config::dsn();
    let name = dsn
        .parse::<tokio_postgres::Config>()
        .ok()
        .and_then(|c| c.get_dbname().map(str::to_string))
        .unwrap_or_default();
    assert!(
        ["rustorm", "scratch", "probe"]
            .iter()
            .any(|tag| name.contains(tag)),
        "refusing to run DDL against {name:?}: name the database with rustorm, scratch or probe"
    );
    dsn
}

async fn connect() -> tokio_postgres::Client {
    odoo_kernel::connect::connect(&scratch_dsn()).await.unwrap()
}

#[tokio::test]
#[ignore]
async fn a_stale_plan_inside_a_transaction_is_dropped_and_reported_not_retried() {
    let a = connect().await;
    let b = connect().await;
    b.batch_execute("DROP TABLE IF EXISTS rustorm_probe_t; CREATE TABLE rustorm_probe_t (id int primary key); INSERT INTO rustorm_probe_t VALUES (1)").await.unwrap();
    let stmts = StmtCache::default();
    let db = Db::new(&a, &stmts);
    let rows = db
        .query("SELECT * FROM rustorm_probe_t", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .unwrap();
    b.batch_execute("ALTER TABLE rustorm_probe_t ADD COLUMN extra int")
        .await
        .unwrap();
    let second = db.query("SELECT * FROM rustorm_probe_t", &[]).await;
    let err = second.expect_err("the cached plan is stale and the transaction is aborted");
    eprintln!("second query error: {err:#}");
    assert!(odoo_kernel::db::is_stale_plan_error(&err), "{err:#}");
    assert_eq!(stmts.len(), 0, "the stale entry is dropped");
    let after = a.batch_execute("SELECT 1").await;
    eprintln!("transaction usable after the retry: {}", after.is_ok());
    assert!(
        after.is_err(),
        "the transaction is aborted, so no retry inside Db::query could succeed here"
    );
    a.batch_execute("ROLLBACK").await.unwrap();
    let third = db
        .query("SELECT * FROM rustorm_probe_t", &[])
        .await
        .unwrap();
    assert_eq!(third[0].len(), 2);
    b.batch_execute("DROP TABLE rustorm_probe_t").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn a_timed_out_request_is_cancelled_and_the_next_one_runs_on_a_fresh_snapshot() {
    let dsn = scratch_dsn();
    let a = connect().await;
    let b = connect().await;
    b.batch_execute(
        "DROP TABLE IF EXISTS rustorm_probe_tx; CREATE TABLE rustorm_probe_tx (id int)",
    )
    .await
    .unwrap();
    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .unwrap();
    let n0: i64 = a
        .query_one("SELECT count(*) FROM rustorm_probe_tx", &[])
        .await
        .unwrap()
        .get(0);
    let a_pid: i32 = a
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let slow = a.query("SELECT pg_sleep(5)", &[]);
    let timed_out = tokio::time::timeout(std::time::Duration::from_millis(100), slow)
        .await
        .is_err();
    assert!(timed_out);

    odoo_kernel::connect::cancel(a.cancel_token(), &dsn).await;
    let replacement = connect().await;

    b.batch_execute("INSERT INTO rustorm_probe_tx VALUES (1)")
        .await
        .unwrap();
    let stale: i64 = a
        .query_one("SELECT count(*) FROM rustorm_probe_tx", &[])
        .await
        .map(|r| r.get(0))
        .unwrap_or(-1);
    eprintln!(
        "the poisoned connection still answers from its old snapshot: {stale} (or -1, aborted)"
    );
    assert!(
        stale == n0 || stale == -1,
        "the cancelled connection is exactly the hazard the pool refuses to reuse"
    );

    let fresh_pid: i32 = replacement
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    assert_ne!(a_pid, fresh_pid, "the replacement is a different backend");
    let read_only: String = replacement
        .query_one("SHOW transaction_read_only", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        read_only, "off",
        "the replacement starts outside the leaked READ ONLY transaction"
    );
    let n1: i64 = replacement
        .query_one("SELECT count(*) FROM rustorm_probe_tx", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        n1,
        n0 + 1,
        "the next request sees the row the stale snapshot hides"
    );

    let cancelled = tokio::time::timeout(
        std::time::Duration::from_secs(6),
        b.query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE pid = $1 AND state = 'active' AND query LIKE '%pg_sleep%'",
            &[&a_pid],
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let still_sleeping: i64 = cancelled.get(0);
    assert_eq!(
        still_sleeping, 0,
        "cancel reached the backend; pg_sleep is no longer running"
    );

    let _ = a.batch_execute("ROLLBACK").await;
    b.batch_execute("DROP TABLE rustorm_probe_tx")
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn a_tls_dsn_encrypts_the_session_and_rejects_an_untrusted_certificate() {
    let dsn = std::env::var("RUSTORM_TLS_DSN")
        .expect("set RUSTORM_TLS_DSN to an owned TLS server with a self-signed certificate");
    let client = odoo_kernel::connect::connect(&dsn).await.unwrap();
    let (ssl, version): (bool, Option<String>) = {
        let row = client
            .query_one(
                "SELECT ssl, version FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                &[],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1))
    };
    eprintln!("ssl={ssl} version={version:?}");
    assert!(ssl, "sslmode=require must produce an encrypted session");

    let strict = format!("{dsn} sslmode=verify-full");
    let verified = odoo_kernel::connect::connect(&strict).await;
    eprintln!(
        "verify-full against the snakeoil certificate: {:?}",
        verified.as_ref().err().map(|e| format!("{e:#}"))
    );
    assert!(
        verified.is_err(),
        "a self-signed certificate must not pass verify-full"
    );
}
