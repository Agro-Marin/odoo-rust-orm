//! The rule loader against a PostgreSQL, with and without the fork's
//! `ir_rule.composition` column.
//!
//! Ignored by default. Run with the database these tests may use:
//!
//! ```text
//! RUSTORM_TEST_DSN='host=/var/run/postgresql user=me dbname=scratch' \
//!     cargo test -p odoo-kernel --test db_rules -- --ignored
//! ```
//!
//! Each test builds the tables it needs in a schema of its own, on its own
//! connection, and drops the schema at the end -- so they run in parallel,
//! leave nothing behind, and never need an Odoo database.

use odoo_kernel::registry::Registry;
use tokio_postgres::Client;

async fn connect(schema: &str) -> Client {
    let dsn = std::env::var("RUSTORM_TEST_DSN")
        .expect("RUSTORM_TEST_DSN names the database these tests may use");
    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; \
             SET search_path TO {schema}"
        ))
        .await
        .expect("a schema of our own");
    client
}

async fn drop_schema(client: &Client, schema: &str) {
    let _ = client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await;
}

/// The tables `Registry::load_security` reads, at the width it reads them,
/// with one model, three read rules -- a granting group rule, a group rule
/// the fork marks `restrict`, and a global one -- and the group they carry.
async fn mini_security_schema(client: &Client, with_composition: bool) {
    let composition_col = if with_composition {
        ", composition varchar DEFAULT 'grant'"
    } else {
        ""
    };
    client
        .batch_execute(&format!(
            "CREATE TABLE ir_model (id int PRIMARY KEY, model varchar);
             CREATE TABLE ir_rule (id int PRIMARY KEY, model_id int, domain_force text,
                                   active bool, perm_read bool{composition_col});
             CREATE TABLE rule_group_rel (rule_group_id int, group_id int);
             CREATE TABLE res_groups_implied_rel (gid int, hid int);
             CREATE TABLE res_groups_users_rel (uid int, gid int);
             CREATE TABLE ir_model_access (model_id int, group_id int, active bool, perm_read bool);
             CREATE TABLE res_company (id int PRIMARY KEY, active bool);
             CREATE TABLE res_company_users_rel (user_id int, cid int);
             INSERT INTO ir_model VALUES (1, 'x.thing');
             INSERT INTO ir_rule (id, model_id, domain_force, active, perm_read) VALUES
               (10, 1, '[(''a'', ''='', 1)]', true, true),
               (11, 1, '[(''b'', ''='', 2)]', true, true),
               (12, 1, '[(''c'', ''='', 3)]', true, true),
               (13, 1, '[(''d'', ''='', 4)]', false, true);
             INSERT INTO rule_group_rel VALUES (10, 7), (11, 7);"
        ))
        .await
        .expect("the mini schema");
    if with_composition {
        client
            .batch_execute("UPDATE ir_rule SET composition = 'restrict' WHERE id = 11")
            .await
            .expect("mark one rule restricting");
    }
}

fn shape(rules: &[odoo_kernel::registry::Rule]) -> Vec<(Vec<i32>, bool, bool)> {
    rules
        .iter()
        .map(|r| {
            (
                r.groups.clone(),
                r.restrict,
                !r.groups.is_empty() && !r.restrict, // what the combiner ORs
            )
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs RUSTORM_TEST_DSN"]
async fn a_restricting_rule_is_read_from_the_forks_column() {
    let schema = "rustorm_t_composition";
    let client = connect(schema).await;
    mini_security_schema(&client, true).await;

    let security = Registry::load_security(&client).await.expect("loads");
    let rules = &security.rules["x.thing"];
    assert_eq!(rules.len(), 3, "the inactive rule is not loaded: {rules:?}");
    assert_eq!(
        shape(rules),
        vec![
            (vec![7], false, true), // 10: a group rule that grants
            (vec![7], true, false), // 11: a group rule that restricts
            (vec![], false, false), // 12: a global rule
        ],
        "loaded in id order, classified as the fork classifies them"
    );
    drop_schema(&client, schema).await;
}

#[tokio::test]
#[ignore = "needs RUSTORM_TEST_DSN"]
async fn without_the_column_every_group_rule_grants() {
    let schema = "rustorm_t_no_composition";
    let client = connect(schema).await;
    mini_security_schema(&client, false).await;

    let security = Registry::load_security(&client)
        .await
        .expect("stock Odoo loads too");
    let rules = &security.rules["x.thing"];
    assert_eq!(rules.len(), 3);
    assert!(
        rules.iter().all(|r| !r.restrict),
        "no column, no restriction: {rules:?}"
    );
    assert_eq!(
        shape(rules).into_iter().map(|s| s.2).collect::<Vec<_>>(),
        vec![true, true, false]
    );
    drop_schema(&client, schema).await;
}
