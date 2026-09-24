use std::collections::HashMap;

use odoo_kernel::registry::{AccessTopology, Registry};
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

async fn mini_access_schema(client: &Client) {
    client
        .batch_execute(
            "CREATE TABLE ir_model (id int PRIMARY KEY, model varchar);
             CREATE TABLE ir_access (id int PRIMARY KEY, model_id int, group_id int,
                                     kind varchar, guard_scope varchar, domain varchar,
                                     active bool, for_read bool);
             CREATE TABLE res_groups_implied_rel (gid int, hid int);
             CREATE TABLE res_groups_users_rel (uid int, gid int);
             CREATE TABLE res_company (id int PRIMARY KEY, active bool);
             CREATE TABLE res_company_users_rel (user_id int, cid int);
             INSERT INTO ir_model VALUES (1, 'x.thing'), (2, 'x.root'), (3, 'x.never');
             INSERT INTO ir_access VALUES
               (10, 1, 7, 'permission', 'everyone', '[(''a'', ''='', 1)]', true, true),
               (11, 1, 8, 'guard', 'members', '[(''b'', ''='', 2)]', true, true),
               (12, 1, 9, 'guard', 'everyone', '[(''c'', ''='', 3)]', true, true),
               (13, 1, 7, 'permission', 'everyone', '[(''d'', ''='', 4)]', false, true),
               (14, 1, 7, 'permission', 'everyone', '[(''e'', ''='', 5)]', true, false),
               (15, 1, 6, 'permission', 'everyone', '[(0, ''='', 1)]', true, true),
               (20, 2, 5, 'permission', 'everyone', NULL, true, true),
               (21, 2, 9, 'guard', 'everyone', '[(0, ''='', 1)]', true, true),
               (30, 3, 5, 'permission', 'everyone', '[(0, ''='', 1)]', true, true);",
        )
        .await
        .expect("the mini schema");
}

fn shape(rules: &[odoo_kernel::registry::Rule]) -> Vec<(Vec<i32>, bool)> {
    rules
        .iter()
        .map(|r| (r.groups.clone(), r.restrict))
        .collect()
}

#[tokio::test]
#[ignore = "needs RUSTORM_TEST_DSN"]
async fn each_read_row_becomes_the_rule_its_kind_and_scope_make_it() {
    let schema = "rustorm_t_access_rows";
    let client = connect(schema).await;
    mini_access_schema(&client).await;

    let security = Registry::load_security(&client, &AccessTopology::default())
        .await
        .expect("loads");
    assert_eq!(
        shape(&security.rules["x.thing"]),
        vec![
            (vec![7], false),
            (vec![8], true),
            (vec![], true),
            (vec![6], false),
        ],
        "a permission ORs for its group, a members guard ANDs for its group, an \
         everyone guard ANDs for all; the inactive row and the row that does not \
         read are not loaded"
    );
    drop_schema(&client, schema).await;
}

#[tokio::test]
#[ignore = "needs RUSTORM_TEST_DSN"]
async fn a_permission_that_admits_nothing_grants_no_access_and_such_a_guard_denies() {
    let schema = "rustorm_t_access_false";
    let client = connect(schema).await;
    mini_access_schema(&client).await;

    let security = Registry::load_security(&client, &AccessTopology::default())
        .await
        .expect("loads");
    assert_eq!(security.access["x.thing"], vec![7]);
    assert!(
        !security.access.contains_key("x.never"),
        "its only permission is [(0, '=', 1)]: {:?}",
        security.access.get("x.never")
    );
    assert_eq!(security.denying_guards["x.root"], vec![None]);
    assert!(!security.denying_guards.contains_key("x.thing"));
    drop_schema(&client, schema).await;
}

#[tokio::test]
#[ignore = "needs RUSTORM_TEST_DSN"]
async fn a_model_under_a_table_root_is_bound_by_the_roots_rows_too() {
    let schema = "rustorm_t_access_root";
    let client = connect(schema).await;
    mini_access_schema(&client).await;

    let topology = AccessTopology {
        bound_by: HashMap::from([("x.thing".to_string(), vec!["x.root".to_string()])]),
        parents: HashMap::from([("x.thing".to_string(), vec!["x.root".to_string()])]),
    };
    let security = Registry::load_security(&client, &topology)
        .await
        .expect("loads");
    assert_eq!(security.access["x.thing"], vec![7, 5]);
    assert_eq!(security.denying_guards["x.thing"], vec![None]);
    assert_eq!(security.rules["x.thing"].len(), 6);
    assert_eq!(security.parents["x.thing"], vec!["x.root".to_string()]);
    drop_schema(&client, schema).await;
}
