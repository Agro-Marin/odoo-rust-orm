use std::collections::HashMap;

use odoo_kernel::registry::{AccessTopology, Registry};
use tokio_postgres::Client;

mod support;

#[tokio::test]
#[ignore = "needs RUSTORM_TEST_DSN"]
async fn scalar_nulls_follow_record_conversion_not_search_comparands() {
    use odoo_kernel::db::{Db, StmtCache};
    use odoo_kernel::registry::FieldType;
    use odoo_kernel::security::{UserCtx, eval_py, parse_py};
    use serde_json::json;
    use support::{field, model, registry};

    let schema = "rustorm_t_scalar_nulls";
    let client = connect(schema).await;
    client
        .batch_execute(
            "CREATE TABLE res_company (id int PRIMARY KEY, label varchar, number int,
         amount float8, enabled bool, reference int);
         INSERT INTO res_company VALUES (3, NULL, NULL, NULL, NULL, NULL),
         (5, 'five', 7, 1.5, true, 9);",
        )
        .await
        .unwrap();
    let cases = [
        ("label", FieldType::Char, json!(false), json!("five")),
        ("number", FieldType::Integer, json!(0), json!(7)),
        ("amount", FieldType::Float, json!(0.0), json!(1.5)),
        ("enabled", FieldType::Boolean, json!(false), json!(true)),
        (
            "reference",
            FieldType::Many2oneReference,
            json!(0),
            json!(9),
        ),
    ];
    let registry = registry(vec![model(
        "res.company",
        "id",
        cases
            .iter()
            .map(|(name, kind, _, _)| field(name, *kind))
            .collect(),
    )]);
    let cache = StmtCache::default();
    let db = Db::new(&client, &cache);
    let mut user = UserCtx {
        uid: 7,
        company_id: 3,
        company_ids: vec![],
        groups: Default::default(),
        scopes: Default::default(),
    };
    for (name, _, null_value, value) in cases {
        for (ids, expression, expected) in [
            (
                vec![3],
                format!("user.env.companies.{name}"),
                null_value.clone(),
            ),
            (
                vec![],
                format!("user.env.companies.{name}"),
                null_value.clone(),
            ),
            (vec![5], format!("user.env.companies.{name}"), value.clone()),
            (
                vec![3, 5],
                format!("user.env.companies.mapped('{name}')"),
                json!([null_value, value]),
            ),
        ] {
            user.company_ids = ids;
            let actual = eval_py(&parse_py(&expression).unwrap(), &registry, &db, &user)
                .await
                .unwrap();
            tracing::debug!(expression, ?user.company_ids, %actual, %expected, "checking typed record values");
            assert_eq!(actual, expected);
        }
    }
    user.company_ids = vec![999];
    let actual = eval_py(
        &parse_py("user.env.companies.number").unwrap(),
        &registry,
        &db,
        &user,
    )
    .await;
    tracing::debug!(?actual, "a missing row is not a null field");
    assert!(matches!(actual, Err(ref error) if error.is::<odoo_kernel::error::Refusal>()));
    drop_schema(&client, schema).await;
}

#[tokio::test]
#[ignore = "needs RUSTORM_TEST_DSN"]
async fn scalar_mapping_preserves_list_shape_order_and_singleton_access() {
    use odoo_kernel::db::{Db, StmtCache};
    use odoo_kernel::registry::FieldType;
    use odoo_kernel::security::{UserCtx, eval_py, parse_py};
    use serde_json::json;
    use support::{field, model, registry};

    let schema = "rustorm_t_scalar_mapping";
    let client = connect(schema).await;
    client
        .batch_execute(
            "CREATE TABLE res_company (id int PRIMARY KEY, code varchar, parent_id int);
         INSERT INTO res_company VALUES (3, 'three', 3), (5, 'five', 5),
         (7, 'seven', 5), (9, 'nine', NULL);",
        )
        .await
        .unwrap();
    let mut parent = field("parent_id", FieldType::Many2one);
    parent.relation = Some("res.company".into());
    let registry = registry(vec![model(
        "res.company",
        "id",
        vec![field("code", FieldType::Char), parent],
    )]);
    let cache = StmtCache::default();
    let db = Db::new(&client, &cache);
    let mut user = UserCtx {
        uid: 7,
        company_id: 3,
        company_ids: vec![],
        groups: Default::default(),
        scopes: Default::default(),
    };
    for (ids, expected) in [
        (vec![], json!([])),
        (vec![5], json!(["five"])),
        (vec![5, 3], json!(["five", "three"])),
        (vec![5, 5, 3], json!(["five", "five", "three"])),
    ] {
        user.company_ids = ids;
        let actual = eval_py(
            &parse_py("user.env.companies.mapped('code')").unwrap(),
            &registry,
            &db,
            &user,
        )
        .await
        .unwrap();
        tracing::debug!(?user.company_ids, %actual, %expected, "scalar mapping follows record order and cardinality");
        assert_eq!(actual, expected);
    }
    for source in [
        "user.env.companies.code",
        "user.env.companies.mapped('parent_id').code",
    ] {
        let actual = eval_py(&parse_py(source).unwrap(), &registry, &db, &user).await;
        tracing::debug!(
            source,
            ?actual,
            "a preceding relational map does not map the scalar access"
        );
        assert!(
            matches!(actual, Err(ref error) if error.is::<odoo_kernel::error::Refusal>()),
            "{source}: {actual:?}"
        );
    }
    user.company_ids = vec![5];
    let actual = eval_py(
        &parse_py("user.env.companies.code").unwrap(),
        &registry,
        &db,
        &user,
    )
    .await
    .unwrap();
    assert_eq!(actual, json!("five"));
    user.company_ids = vec![5, 999];
    let actual = eval_py(
        &parse_py("user.env.companies.mapped('code')").unwrap(),
        &registry,
        &db,
        &user,
    )
    .await;
    tracing::debug!(
        ?actual,
        "missing records must not silently disappear from mapped results"
    );
    assert!(matches!(actual, Err(ref error) if error.is::<odoo_kernel::error::Refusal>()));
    for (ids, expression, expected) in [
        (
            vec![7, 3, 5, 9],
            "user.env.companies.parent_id.ids",
            json!([5, 3]),
        ),
        (vec![7, 5], "user.env.companies.parent_id.id", json!(5)),
        (
            vec![7, 5],
            "user.env.companies.mapped('parent_id').code",
            json!("five"),
        ),
        (
            vec![7, 3, 5],
            "user.env.companies.mapped('parent_id.code')",
            json!(["five", "three"]),
        ),
    ] {
        user.company_ids = ids;
        let actual = eval_py(&parse_py(expression).unwrap(), &registry, &db, &user)
            .await
            .unwrap();
        tracing::debug!(expression, %actual, %expected, "many2one traversal preserves first-seen order and uniqueness");
        assert_eq!(actual, expected);
    }
    drop_schema(&client, schema).await;
}

async fn connect(schema: &str) -> Client {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_test_writer()
        .try_init();
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
                                     active bool, operation varchar, reach varchar,
                                     anchor varchar);
             CREATE TABLE res_groups_implied_rel (gid int, hid int);
             CREATE TABLE res_groups_users_rel (uid int, gid int);
             CREATE TABLE ir_model_data (module varchar, name varchar, model varchar,
                                         res_id int);
             INSERT INTO ir_model_data VALUES ('base', 'group_everyone', 'res.groups', 4);
             CREATE TABLE res_company (id int PRIMARY KEY, active bool);
             CREATE TABLE res_company_users_rel (user_id int, cid int);
             INSERT INTO ir_model VALUES (1, 'x.thing'), (2, 'x.root'), (3, 'x.never');
             INSERT INTO ir_access VALUES
               (10, 1, 7, 'permission', 'everyone', '[(''a'', ''='', 1)]', true, 'crud'),
               (11, 1, 8, 'guard', 'members', '[(''b'', ''='', 2)]', true, 'crud'),
               (12, 1, 9, 'guard', 'everyone', '[(''c'', ''='', 3)]', true, 'crud'),
               (13, 1, 7, 'permission', 'everyone', '[(''d'', ''='', 4)]', false, 'crud'),
               (14, 1, 7, 'permission', 'everyone', '[(''e'', ''='', 5)]', true, 'cud'),
               (15, 1, 6, 'permission', 'everyone', '[(0, ''='', 1)]', true, 'crud'),
               (20, 2, 5, 'permission', 'everyone', NULL, true, 'crud'),
               (21, 2, 9, 'guard', 'everyone', '[(0, ''='', 1)]', true, 'crud'),
               (30, 3, 5, 'permission', 'everyone', '[(0, ''='', 1)]', true, 'crud');",
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
async fn context_names_preserve_python_values_and_do_not_ignore_attributes() {
    use odoo_kernel::db::{Db, StmtCache};
    use odoo_kernel::registry::{Dynamic, Security};
    use odoo_kernel::security::{UserCtx, eval_py, parse_py};
    use serde_json::json;

    let schema = "rustorm_t_context_names";
    let client = connect(schema).await;
    let stmts = StmtCache::default();
    let db = Db::new(&client, &stmts);
    let registry = Registry::new(
        HashMap::new(),
        vec![],
        false,
        Dynamic {
            security: Security::default(),
            defaults: HashMap::new(),
            signals: vec![],
            langs: vec![],
            week_start: HashMap::new(),
        },
    );
    let mut user = UserCtx {
        uid: 7,
        company_id: 3,
        company_ids: vec![3, 5],
        groups: std::sync::Arc::new([9, 2].into_iter().collect()),
        scopes: Default::default(),
    };
    for (source, expected) in [
        ("company_id", json!(3)),
        ("company_ids", json!([3, 5])),
        ("group_ids", json!([2, 9])),
        ("(company_id)", json!(3)),
        ("(company_ids)", json!([3, 5])),
        ("(company_id,)", json!([3])),
        (
            "([('company_id', '=', (company_id))])",
            json!([["company_id", "=", 3]]),
        ),
    ] {
        let actual = eval_py(&parse_py(source).unwrap(), &registry, &db, &user)
            .await
            .unwrap();
        tracing::debug!(source, %actual, %expected, "checking rule context values");
        assert_eq!(actual, expected, "{source}");
    }
    for source in [
        "company_id.id",
        "company_id.real",
        "company_id.denominator",
        "company_ids.ids",
        "company_ids.mapped('id')",
        "group_ids.ids",
        "user.env.companies.mapped('ids')",
    ] {
        let actual = eval_py(&parse_py(source).unwrap(), &registry, &db, &user).await;
        tracing::debug!(
            source,
            ?actual,
            "unsupported attributes must delegate to Python"
        );
        assert!(
            matches!(actual, Err(ref error) if error.is::<odoo_kernel::error::Refusal>()),
            "{source}: {actual:?}"
        );
    }
    for ids in [vec![], vec![5], vec![5, 3]] {
        user.company_ids = ids.clone();
        for source in ["user.env.companies.ids", "user.env.companies.mapped('id')"] {
            let actual = eval_py(&parse_py(source).unwrap(), &registry, &db, &user)
                .await
                .unwrap();
            tracing::debug!(source, ?ids, %actual, "ID lists retain cardinality and order");
            assert_eq!(actual, json!(ids), "{source}");
        }
        let actual = eval_py(
            &parse_py("user.env.companies.id").unwrap(),
            &registry,
            &db,
            &user,
        )
        .await;
        tracing::debug!(?ids, ?actual, "id requires at most one record");
        match ids.as_slice() {
            [] => assert_eq!(actual.unwrap(), json!(false)),
            [id] => assert_eq!(actual.unwrap(), json!(id)),
            _ => assert!(
                matches!(actual, Err(ref error) if error.is::<odoo_kernel::error::Refusal>())
            ),
        }
    }
    drop_schema(&client, schema).await;
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
    assert_eq!(security.everyone, Some(4));
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
        anchors: HashMap::new(),
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

#[tokio::test]
#[ignore = "needs RUSTORM_TEST_DSN"]
async fn a_reach_row_keeps_its_rung_and_anchor_and_hr_or_a_predicate_is_refused() {
    let schema = "rustorm_t_access_reach";
    let client = connect(schema).await;
    mini_access_schema(&client).await;
    client
        .batch_execute(
            "INSERT INTO ir_model VALUES (4, 'x.reached');
             INSERT INTO ir_access VALUES
               (40, 4, 7, 'permission', 'everyone', '[(''a'', ''='', 1)]', true, 'r', 'own', 'review'),
               (41, 4, 7, 'permission', 'everyone', NULL, true, 'r', 'company', NULL),
               (42, 4, 7, 'permission', 'everyone', NULL, true, 'r', 'team', NULL),
               (43, 4, 8, 'permission', 'everyone', '[(''g'', ''in'', group_ids)]', true, 'r', 'partner', NULL);",
        )
        .await
        .expect("the reach rows");

    let security = Registry::load_security(&client, &AccessTopology::default())
        .await
        .expect("loads");
    let rules = &security.rules["x.reached"];
    let reach = |r: &odoo_kernel::registry::Rule| {
        r.reach
            .as_ref()
            .map(|reach| (reach.rung.clone(), reach.anchor.clone()))
    };
    assert_eq!(
        reach(&rules[0]),
        Some(("own".to_string(), Some("review".to_string())))
    );
    assert!(matches!(rules[0].parsed, Some(Ok(_))), "its fixed filter");
    assert_eq!(reach(&rules[1]), Some(("company".to_string(), None)));
    assert!(rules[1].parsed.is_none());
    assert!(
        matches!(&rules[2].parsed, Some(Err(why)) if why.contains("reach team")),
        "{:?}",
        rules[2].parsed
    );
    assert!(
        matches!(&rules[3].parsed, Some(Err(why)) if why.contains("reads the principal")),
        "a filter beside a reach that reads the user is refused: {:?}",
        rules[3].parsed
    );
    assert_eq!(security.access["x.reached"], vec![7, 7, 7, 8]);
    drop_schema(&client, schema).await;
}
