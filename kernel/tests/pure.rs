use std::collections::HashMap;

use odoo_kernel::domain;
use odoo_kernel::registry::{Dynamic, Field, FieldType, Model, Registry, Security};
use odoo_kernel::security::{RuleSet, parse_py};
use odoo_kernel::sqlgen::{Compiler, ExprCtx, parse_order};
use sea_query::{Alias, Expr, ExprTrait, PostgresQueryBuilder, Query};
use serde_json::json;

fn field(name: &str, ttype: FieldType) -> Field {
    Field {
        name: name.into(),
        ttype,
        relation: None,
        relation_table: None,
        column1: None,
        column2: None,
        relation_field: None,
        company_dependent: false,
        has_column: true,
        stored: true,
        pg_type: match ttype {
            FieldType::Integer | FieldType::Many2one => "int4".into(),
            FieldType::Boolean => "bool".into(),
            FieldType::Float => "float8".into(),
            _ => "varchar".into(),
        },
        not_null: false,
        translated: false,
        translate_whole: false,
        column_cast: Some(
            match ttype {
                FieldType::Integer | FieldType::Many2one => "int4",
                FieldType::Boolean => "bool",
                FieldType::Float => "float8",
                _ => "VARCHAR",
            }
            .into(),
        ),
        related: None,
        domain: None,
        domain_callable: false,
        model_field: None,
        index: None,
        cd_fallback: None,
        custom_search: false,
        context: None,
        groups: None,
        falsy: ttype.falsy_json_for_type(name),
        bypass_search_access: Some(false),
        compute_sudo: false,
        inherited: false,
        required: false,
        group_by_field: None,
        order_by_field: None,
        search_kind: None,
    }
}

#[test]
fn a_field_group_spec_is_evaluated_the_way_has_groups_does() {
    let reg = base_registry();
    let mut f = field("secret", FieldType::Char);
    let held: std::collections::HashSet<i32> = [7].into();

    f.groups = None;
    assert!(reg.field_readable(&f, &held), "no spec: readable");

    f.groups = Some(".".into());
    assert!(!reg.field_readable(&f, &held), "NO_ACCESS denies everyone");

    f.groups = Some("base.group_user".into());
    assert!(!reg.field_readable(&f, &held), "unheld positive denies");

    f.groups = Some("!base.group_portal".into());
    assert!(reg.field_readable(&f, &held), "unmatched negation allows");
}

fn m2o(name: &str, comodel: &str) -> Field {
    let mut f = field(name, FieldType::Many2one);
    f.relation = Some(comodel.into());
    f
}

fn model(name: &str, order: &str, fields: Vec<Field>) -> Model {
    Model {
        name: name.into(),
        table: name.replace('.', "_"),
        order: order.into(),
        fields: fields.into_iter().map(|f| (f.name.clone(), f)).collect(),
        rec_name: Some("name".into()),
        parent_name: None,
        parent_store: false,
        active_name: None,
        display_name_column: Vec::new(),
        display_name_guard: None,

        read_path_pure: true,
        search_pure: true,
        display_name_default: true,
        order_pure: true,
        read_group_pure: true,
        display_name_access_pure: true,
        check_access_pure: true,
        name_search_fields: Some(vec!["name".into()]),
        display_name_search_exact: Vec::new(),
        impure_read_methods: Vec::new(),
    }
}

fn registry(models: Vec<Model>) -> Registry {
    Registry::new(
        models.into_iter().map(|m| (m.name.clone(), m)).collect(),
        vec!["en_US".into()],
        false,
        Dynamic {
            security: Security::default(),
            defaults: HashMap::new(),
            signals: Vec::new(),
            langs: Vec::new(),
            week_start: HashMap::new(),
        },
    )
}

fn unaccent_registry() -> Registry {
    let mut reg = base_registry();
    reg.has_unaccent = true;
    reg
}

fn m2m(name: &str, comodel: &str, table: &str, c1: &str, c2: &str) -> Field {
    let mut f = field(name, FieldType::Many2many);
    f.has_column = false;
    f.relation = Some(comodel.into());
    f.relation_table = Some(table.into());
    f.column1 = Some(c1.into());
    f.column2 = Some(c2.into());
    f
}

fn base_registry() -> Registry {
    registry(vec![
        model(
            "res.partner",
            "display_name, id",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
                field("credit_limit", FieldType::Float),
                field("active", FieldType::Boolean),
                m2o("country_id", "res.country"),
                m2m(
                    "company_ids",
                    "res.country",
                    "partner_company_rel",
                    "partner_id",
                    "company_id",
                ),
            ],
        ),
        model(
            "res.country",
            "name",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
            ],
        ),
    ])
}

fn sql_of(cond: sea_query::Condition) -> String {
    let mut q = Query::select();
    q.expr(Expr::cust("1"))
        .from(Alias::new("res_partner"))
        .cond_where(cond);
    q.to_string(PostgresQueryBuilder)
}

fn compile_res(reg: &Registry, dom: serde_json::Value) -> anyhow::Result<String> {
    let ctx = ExprCtx::new(reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    let rules = RuleSet::default();
    let c = Compiler::root(&ctx, m, &rules, true);
    Ok(sql_of(c.compile(&domain::parse(&dom)?)?))
}

fn compile(reg: &Registry, dom: serde_json::Value) -> String {
    let ctx = ExprCtx::new(reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    let rules = RuleSet::default();
    let c = Compiler::root(&ctx, m, &rules, true);
    sql_of(c.compile(&domain::parse(&dom).unwrap()).unwrap())
}

#[test]
fn nesting_deeper_than_odoo_allows_is_refused_at_parse_time() {
    // `[op, leaf, <rest>]` with the operator alternating never flattens, so
    // every level is one more level of structural depth, as Odoo counts it.
    fn alternating(levels: usize) -> serde_json::Value {
        let leaf = json!(["name", "=", "x"]);
        let mut dom = vec![leaf.clone()];
        for i in 0..levels {
            let op = if i % 2 == 0 { "&" } else { "|" };
            let mut next = vec![json!(op), leaf.clone()];
            next.extend(dom);
            dom = next;
        }
        serde_json::Value::Array(dom)
    }
    assert!(domain::parse(&alternating(domain::MAX_DOMAIN_NESTING - 1)).is_ok());
    let err =
        domain::parse(&alternating(domain::MAX_DOMAIN_NESTING + 1)).expect_err("one past the cap");
    assert!(err.to_string().contains("nesting too deep"), "got {err}");
}

#[test]
fn a_flat_run_of_the_same_operator_is_one_level_and_compiles() {
    // 20,000 leaves under 19,999 `&`: Odoo's DomainNary flattens this to one
    // AND, and so must the parser, or the tree is 20,000 deep and the
    // compiler recurses over every level of it.
    let reg = base_registry();
    let mut dom: Vec<serde_json::Value> = vec![json!("&"); 19_999];
    dom.extend((0..20_000).map(|i| json!(["credit_limit", ">", i])));
    let node = domain::parse(&serde_json::Value::Array(dom)).expect("flattened");
    match &node {
        domain::Node::And(v) => assert_eq!(v.len(), 20_000),
        other => panic!("expected one AND, got {other:?}"),
    }
    let sql = compile(
        &reg,
        json!([
            "&",
            ["name", "=", "a"],
            "&",
            ["name", "=", "b"],
            ["name", "=", "c"]
        ]),
    );
    assert_eq!(
        sql.matches("AND").count(),
        2,
        "one flat AND of three: {sql}"
    );
}

#[test]
fn a_run_of_negations_collapses_in_pairs() {
    // Odoo's `~~x` is `x`. Ten thousand `!` used to build a tree ten thousand
    // deep that `compile` then recursed over until the stack ran out.
    let reg = base_registry();
    let mut dom: Vec<serde_json::Value> = vec![json!("!"); 10_000];
    dom.push(json!(["name", "=", "x"]));
    let node = domain::parse(&serde_json::Value::Array(dom)).expect("even count: the leaf");
    assert!(matches!(node, domain::Node::Leaf(_)), "got {node:?}");

    let mut dom: Vec<serde_json::Value> = vec![json!("!"); 10_001];
    dom.push(json!(["name", "=", "x"]));
    let node = domain::parse(&serde_json::Value::Array(dom)).expect("odd count: one Not");
    assert!(matches!(node, domain::Node::Not(_)), "got {node:?}");

    let sql = compile(
        &reg,
        serde_json::Value::Array(
            std::iter::repeat_n(json!("!"), 10_001)
                .chain([json!(["name", "=", "x"])])
                .collect(),
        ),
    );
    assert!(
        sql.contains("<>") || sql.contains("IS NULL"),
        "negated once: {sql}"
    );
}

fn related(name: &str, path: &str, ttype: FieldType) -> Field {
    let mut f = field(name, ttype);
    f.has_column = false;
    f.stored = false;
    f.related = Some(path.into());
    f
}

fn user_ctx(reg: &Registry) -> ExprCtx<'_> {
    ExprCtx::new(reg, "en_US", 1)
        .with_access(2, std::sync::Arc::new(std::collections::HashSet::new()))
}

fn partner_with_related_country_name() -> Registry {
    registry(vec![
        model(
            "res.partner",
            "id",
            vec![
                field("id", FieldType::Integer),
                m2o("country_id", "res.country"),
                related("country_name", "country_id.name", FieldType::Char),
            ],
        ),
        model(
            "res.country",
            "name",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
            ],
        ),
    ])
}

#[test]
fn a_related_field_is_refused_for_a_user_unless_sudoed_or_inherited() {
    let reg = partner_with_related_country_name();
    let partner = reg.get("res.partner").unwrap();
    let mut f = partner.fields["country_name"].clone();

    let su = ExprCtx::new(&reg, "en_US", 1);
    assert!(
        su.read_expr(partner, &f, "res_partner").is_ok(),
        "superuser reads it"
    );

    let err = user_ctx(&reg)
        .read_expr(partner, &f, "res_partner")
        .expect_err("a plain related field is computed in Python for a user");
    assert!(
        err.to_string().contains("neither sudoed nor inherited"),
        "got {err}"
    );

    f.compute_sudo = true;
    assert!(
        user_ctx(&reg).read_expr(partner, &f, "res_partner").is_ok(),
        "compute_sudo"
    );

    f.compute_sudo = false;
    f.inherited = true;
    assert!(
        user_ctx(&reg).read_expr(partner, &f, "res_partner").is_ok(),
        "inherited"
    );
}

#[test]
fn ordering_by_a_related_field_is_refused_for_a_user_the_same_way() {
    let reg = partner_with_related_country_name();
    let partner = reg.get("res.partner").unwrap();
    let ctx = user_ctx(&reg);
    let err = match parse_order(&ctx, partner, "res_partner", "country_name") {
        Err(e) => e,
        Ok(_) => panic!("ordering reads the same subquery and must be refused too"),
    };
    assert!(
        err.to_string()
            .contains("cannot order res.partner by related"),
        "got {err:#}"
    );
}

#[test]
fn empty_domain_is_true() {
    assert!(matches!(
        domain::parse(&json!([])).unwrap(),
        domain::Node::True
    ));
}

#[test]
fn implicit_and_over_consecutive_leaves() {
    let n = domain::parse(&json!([["a", "=", 1], ["b", "=", 2]])).unwrap();
    match n {
        domain::Node::And(v) => assert_eq!(v.len(), 2),
        other => panic!("expected And, got {other:?}"),
    }
}

#[test]
fn prefix_operators_bind_following_operands() {
    let n = domain::parse(&json!(["|", ["a", "=", 1], ["b", "=", 2]])).unwrap();
    assert!(matches!(n, domain::Node::Or(_)));
    let n = domain::parse(&json!(["!", ["a", "=", 1]])).unwrap();
    assert!(matches!(n, domain::Node::Not(_)));
}

#[test]
fn true_and_false_leaves() {
    assert!(matches!(
        domain::parse(&json!([[1, "=", 1]])).unwrap(),
        domain::Node::True
    ));
    assert!(matches!(
        domain::parse(&json!([[0, "=", 1]])).unwrap(),
        domain::Node::False
    ));
}

#[test]
fn malformed_domains_are_errors_not_panics() {
    assert!(domain::parse(&json!("nope")).is_err());
    assert!(domain::parse(&json!(["&", ["a", "=", 1]])).is_err());
    assert!(domain::parse(&json!([["a", "="]])).is_err());
}

#[test]
fn negative_operator_includes_nulls() {
    let sql = compile(&base_registry(), json!([["name", "!=", "x"]]));
    assert!(sql.contains("IS NULL"), "got {sql}");
}

#[test]
fn falsy_string_equals_matches_null() {
    let sql = compile(&base_registry(), json!([["name", "=", ""]]));
    assert!(sql.contains("IS NULL"), "got {sql}");
}

#[test]
fn an_identifier_with_a_quote_in_it_is_escaped() {
    use odoo_kernel::db::ident;
    assert_eq!(ident("res_partner"), "\"res_partner\"");
    assert_eq!(ident("we\"ird"), "\"we\"\"ird\"");
}

#[test]
fn not_over_a_leaf_inverts_the_operator() {
    let reg = base_registry();
    let sql = compile(
        &reg,
        serde_json::json!(["!", ["name", "not in", ["a", false]]]),
    );
    assert!(sql.contains("IS NULL"), "the null rows must survive: {sql}");
    assert!(!sql.contains("NOT ("), "no wrapping NOT: {sql}");
}

#[test]
fn not_over_an_and_becomes_an_or_of_negations() {
    let reg = base_registry();
    let a = compile(
        &reg,
        serde_json::json!(["!", "&", ["name", "=", "x"], ["active", "=", true]]),
    );
    let b = compile(
        &reg,
        serde_json::json!(["|", ["name", "!=", "x"], ["active", "!=", true]]),
    );
    assert_eq!(a, b, "the two must compile identically");
}

#[test]
fn not_over_not_cancels() {
    let reg = base_registry();
    let a = compile(&reg, serde_json::json!(["!", "!", ["name", "=", "x"]]));
    let b = compile(&reg, serde_json::json!([["name", "=", "x"]]));
    assert_eq!(a, b);
}

#[test]
fn negating_an_inequality_adds_the_unset_case_only_where_odoo_does() {
    let reg = base_registry();

    let float_sql = compile(&reg, serde_json::json!(["!", ["credit_limit", "<", 5]]));
    assert!(float_sql.contains(">="), "got: {float_sql}");
    assert!(!float_sql.contains("IS NULL"), "got: {float_sql}");
}

#[test]
fn a_number_compared_with_text_admits_the_unset_rows_as_odoo_does() {
    let reg = base_registry();
    let sql = compile(&reg, serde_json::json!(["!", ["name", ">", 3]]));
    assert!(sql.contains("<= '3'"), "got: {sql}");
    assert!(
        sql.contains("IS NULL"),
        "'' <= '3', so unset names match: {sql}"
    );
}

#[test]
fn an_inequality_on_html_is_refused() {
    let reg = registry(vec![model(
        "res.partner",
        "id",
        vec![
            field("id", FieldType::Integer),
            field("comment", FieldType::Html),
        ],
    )]);
    let err = compile_res(&reg, serde_json::json!([["comment", "<", "note"]])).unwrap_err();
    assert!(format!("{err:#}").contains("sanitized"), "{err:#}");
    assert!(compile_res(&reg, serde_json::json!([["comment", "=", "note"]])).is_ok());
}

#[test]
fn an_inequality_on_a_boolean_is_refused() {
    let reg = base_registry();
    let err = compile_res(&reg, serde_json::json!([["active", "<=", true]])).unwrap_err();
    assert!(format!("{err:#}").contains("boolean"), "{err:#}");
}

#[test]
fn a_like_on_a_many2one_searches_the_comodel_display_name() {
    let reg = base_registry();
    let sql = compile(&reg, serde_json::json!([["country_id", "ilike", "be"]]));
    assert!(sql.contains("res_country"), "got: {sql}");
    assert!(sql.contains(r#""name" ILIKE '%be%'"#), "got: {sql}");
    assert!(sql.contains(r#""country_id" IN (SELECT"#), "got: {sql}");
}

#[test]
fn a_like_on_display_name_searches_the_name_columns() {
    let reg = base_registry();
    let sql = compile(&reg, serde_json::json!([["display_name", "ilike", "ac"]]));
    assert!(sql.contains(r#""name" ILIKE '%ac%'"#), "got: {sql}");

    assert!(!sql.contains("IN (SELECT"), "no subquery is needed: {sql}");
}

#[test]
fn a_negative_like_on_display_name_negates_the_condition() {
    let reg = base_registry();
    let sql = compile(
        &reg,
        serde_json::json!([["display_name", "not ilike", "ac"]]),
    );
    assert!(sql.contains("NOT"), "got: {sql}");
}

#[test]
fn an_equality_on_display_name_searches_the_name_columns_and_handles_unset_names() {
    let reg = base_registry();
    let eq = compile(&reg, json!([["display_name", "=", "ac"]]));
    assert!(
        eq.contains(r#""name" IN ('ac')"#) && !eq.contains("IS NULL"),
        "{eq}"
    );
    let with_unset = compile(&reg, json!([["display_name", "in", ["ac", false]]]));
    assert!(
        with_unset.contains("IN ('ac')") && with_unset.contains("IS NULL"),
        "{with_unset}"
    );
    let not_unset = compile(&reg, json!([["display_name", "!=", false]]));
    assert!(
        not_unset.contains("NOT IN ('')") && !not_unset.contains("IS NULL"),
        "an unset name is NULL or empty, and NOT IN ('') excludes both: {not_unset}"
    );
    assert!(
        compile_res(&reg, json!([["display_name", ">", "ac"]])).is_err(),
        "inequalities stay refused"
    );
}

#[test]
fn a_display_name_that_searches_itself_is_refused_not_recursed() {
    let mut reg = base_registry();
    {
        let m = reg.models.get_mut("res.partner").unwrap();
        m.rec_name = Some("display_name".into());
        m.name_search_fields = Some(vec!["display_name".into()]);
    }
    let err = compile_res(&reg, serde_json::json!([["display_name", "ilike", "a"]])).unwrap_err();
    assert!(format!("{err:#}").contains("display_name"), "{err:#}");
}

#[test]
fn a_comodel_whose_display_name_searches_itself_is_refused_too() {
    let mut reg = base_registry();
    {
        let co = reg.models.get_mut("res.country").unwrap();
        co.name_search_fields = Some(vec!["display_name".into()]);
    }
    let err = compile_res(&reg, serde_json::json!([["country_id", "ilike", "a"]])).unwrap_err();
    assert!(format!("{err:#}").contains("display_name"), "{err:#}");
}

#[test]
fn a_negative_like_negates_the_any_not_the_pattern() {
    let reg = base_registry();
    let sql = compile(&reg, serde_json::json!([["country_id", "not ilike", "be"]]));
    assert!(
        sql.contains("ILIKE") && !sql.contains("NOT ILIKE"),
        "got: {sql}"
    );
    assert!(sql.contains("IS NULL"), "got: {sql}");
}

#[test]
fn an_empty_pattern_on_a_many2one_is_the_relation_being_set() {
    let reg = base_registry();
    let sql = compile(&reg, serde_json::json!([["country_id", "ilike", ""]]));
    assert!(!sql.contains("ILIKE"), "got: {sql}");
    assert!(
        sql.contains(r#""country_id" IS NOT NULL"#),
        "Odoo rewrites it to != False: {sql}"
    );
}

#[test]
fn a_python_display_name_is_still_refused() {
    let mut reg = base_registry();
    reg.models
        .get_mut("res.country")
        .unwrap()
        .name_search_fields = None;
    let err = compile_res(&reg, serde_json::json!([["country_id", "ilike", "be"]])).unwrap_err();
    assert!(format!("{err:#}").contains("res.country"), "{err:#}");
}

#[test]
fn a_long_in_list_becomes_one_array_parameter() {
    let reg = base_registry();
    let ids: Vec<i64> = (1..=101).collect();
    let sql = compile(&reg, serde_json::json!([["id", "in", ids]]));
    assert!(sql.contains("= ANY("), "got: {sql}");
    assert!(!sql.contains("IN ("), "got: {sql}");
}

#[test]
fn a_long_not_in_list_becomes_all() {
    let reg = base_registry();
    let ids: Vec<i64> = (1..=101).collect();
    let sql = compile(&reg, serde_json::json!([["id", "not in", ids]]));
    assert!(sql.contains("<> ALL("), "got: {sql}");
}

#[test]
fn a_short_in_list_is_still_a_bound_list() {
    let reg = base_registry();
    let sql = compile(&reg, serde_json::json!([["id", "in", [1, 2, 3]]]));
    assert!(sql.contains("IN (1, 2, 3)"), "got: {sql}");
}

#[test]
fn empty_in_list_is_false_and_not_in_is_true() {
    assert!(compile(&base_registry(), json!([["name", "in", []]])).contains("FALSE"));
    assert!(compile(&base_registry(), json!([["name", "not in", []]])).contains("TRUE"));
}

#[test]
fn positive_equality_does_not_leak_nulls() {
    let sql = compile(&base_registry(), json!([["name", "=", "x"]]));
    assert!(!sql.contains("IS NULL"), "got {sql}");
}

fn compile_on(reg: &Registry, dom: serde_json::Value) -> String {
    compile(reg, dom)
}

#[test]
fn ilike_is_unaccented_when_the_database_provides_it() {
    let reg = unaccent_registry();
    let sql = compile_on(&reg, json!([["name", "ilike", "san"]]));
    assert!(sql.contains("unaccent"), "got {sql}");
    assert_eq!(sql.matches("unaccent").count(), 2, "both sides: {sql}");
}

#[test]
fn ilike_is_untouched_without_the_extension() {
    let sql = compile_on(&base_registry(), json!([["name", "ilike", "san"]]));
    assert!(!sql.contains("unaccent"), "got {sql}");
}

#[test]
fn plain_like_is_never_unaccented() {
    let reg = unaccent_registry();
    let sql = compile_on(&reg, json!([["name", "like", "san"]]));
    assert!(!sql.contains("unaccent"), "got {sql}");
}

#[test]
fn negative_ilike_unaccents_and_keeps_null_handling() {
    let reg = unaccent_registry();
    let sql = compile_on(&reg, json!([["name", "not ilike", "san"]]));
    assert!(sql.contains("NOT ILIKE"), "got {sql}");
    assert!(sql.contains("unaccent"), "got {sql}");
    assert!(sql.contains("IS NULL"), "got {sql}");
}

#[test]
fn order_by_plain_column() {
    let reg = base_registry();
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    let items = parse_order(&ctx, m, &m.table, "name desc").unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0].order, sea_query::Order::Desc));
}

#[test]
fn unknown_order_field_raises() {
    let reg = base_registry();
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    let err = match parse_order(&ctx, m, &m.table, "no_such_field desc") {
        Err(e) => e,
        Ok(items) => panic!("expected an error, got {} items", items.len()),
    };
    assert!(format!("{err}").contains("no_such_field"), "got {err}");
}

#[test]
fn partially_unresolvable_order_raises() {
    let reg = base_registry();
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    assert!(parse_order(&ctx, m, &m.table, "no_such_field, name").is_err());
}

#[test]
fn non_stored_order_field_raises() {
    let mut reg = base_registry();
    let mut f = field("computed_rank", FieldType::Integer);
    f.has_column = false;
    reg.models
        .get_mut("res.partner")
        .unwrap()
        .fields
        .insert(f.name.clone(), f);
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    assert!(parse_order(&ctx, m, &m.table, "computed_rank").is_err());
}

#[test]
fn custom_search_field_is_refused_in_domains() {
    let mut reg = base_registry();
    reg.models
        .get_mut("res.partner")
        .unwrap()
        .fields
        .get_mut("name")
        .unwrap()
        .custom_search = true;
    let rules = RuleSet::default();
    let err = compile_with(&reg, &rules, json!([["name", "=", "x"]])).unwrap_err();
    assert!(format!("{err:#}").contains("custom search"), "got {err:#}");
}

#[test]
fn x2many_in_with_falsy_also_matches_records_with_no_links() {
    let sql = compile(
        &base_registry(),
        json!([["company_ids", "in", [1, 2, false]]]),
    );
    assert!(
        sql.contains("NOT \"res_partner\".\"id\" IN"),
        "empty-link branch missing: {sql}"
    );
    assert!(sql.contains(" OR "), "expected a disjunction: {sql}");
    assert!(sql.contains("partner_company_rel"), "got {sql}");
}

#[test]
fn x2many_in_without_falsy_is_a_plain_membership() {
    let sql = compile(&base_registry(), json!([["company_ids", "in", [1, 2]]]));
    assert!(!sql.contains(" OR "), "unexpected disjunction: {sql}");
}

#[test]
fn x2many_equals_false_means_no_links() {
    let sql = compile(&base_registry(), json!([["company_ids", "=", false]]));
    assert!(sql.contains("NOT \"res_partner\".\"id\" IN"), "got {sql}");
    assert!(!sql.contains(" OR "), "no id branch expected: {sql}");
}

#[test]
fn x2many_empty_list_is_false() {
    assert!(compile(&base_registry(), json!([["company_ids", "in", []]])).contains("FALSE"));
}

#[test]
fn x2many_name_members_search_the_comodel_display_name() {
    let reg = base_registry();
    let rules = RuleSet::default();
    let sql = compile_with(&reg, &rules, json!([["company_ids", "in", ["Belgium"]]])).unwrap();
    assert!(sql.contains("res_country"), "got {sql}");
    assert!(sql.contains("Belgium"), "got {sql}");
}

fn compile_with(reg: &Registry, rules: &RuleSet, dom: serde_json::Value) -> anyhow::Result<String> {
    let ctx = ExprCtx::new(reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    let c = Compiler::root(&ctx, m, rules, false);
    Ok(sql_of(c.compile(&domain::parse(&dom)?)?))
}

#[test]
fn a_comodel_whose_read_path_is_python_is_refused_in_a_subquery() {
    let mut reg = base_registry();
    reg.models.get_mut("res.country").unwrap().search_pure = false;
    let err = compile_with(
        &reg,
        &RuleSet::default(),
        serde_json::json!([["country_id.name", "=", "BE"]]),
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("res.country") && msg.contains("`_search` in Python"),
        "expected a refusal naming the comodel, got: {msg}"
    );
}

#[test]
fn a_comodel_that_only_overrides_read_is_still_traversable() {
    let mut reg = base_registry();
    {
        let co = reg.models.get_mut("res.country").unwrap();
        co.read_path_pure = false;
        co.search_pure = true;
    }
    let sql = compile_with(
        &reg,
        &RuleSet::default(),
        serde_json::json!([["country_id.name", "=", "BE"]]),
    )
    .unwrap();
    assert!(sql.contains("res_country"), "got: {sql}");
}

#[test]
fn a_pure_comodel_still_compiles_the_subquery() {
    let reg = base_registry();
    let sql = compile_with(
        &reg,
        &RuleSet::default(),
        serde_json::json!([["country_id.name", "=", "BE"]]),
    )
    .unwrap();
    assert!(sql.contains("res_country"), "got: {sql}");
}

#[test]
fn unevaluated_comodel_rule_refuses_the_subquery() {
    let reg = base_registry();
    let mut rules = RuleSet::default();
    rules.mark_unevaluated("res.country".into(), "unsupported '+' in domain".into());
    let err = compile_with(&reg, &rules, json!([["country_id.name", "=", "Belgium"]]))
        .expect_err("must refuse");
    let msg = format!("{err:#}");
    assert!(msg.contains("res.country"), "got {msg}");
    assert!(msg.contains("could not be evaluated"), "got {msg}");
}

#[test]
fn evaluated_comodel_rule_still_compiles() {
    let reg = base_registry();
    let rules = RuleSet::default();
    let sql = compile_with(&reg, &rules, json!([["country_id.name", "=", "Belgium"]]))
        .expect("no unevaluated rules");
    assert!(sql.contains("res_country"), "got {sql}");
}

#[test]
fn a_ruled_model_that_was_not_compiled_is_refused() {
    let ruled: std::collections::HashSet<String> = ["res.partner".to_string()].into();
    let rules = RuleSet::with_ruled(ruled);
    let err = rules.ensure_evaluated("res.partner").unwrap_err();
    assert!(format!("{err:#}").contains("not compiled"), "{err:#}");

    assert!(rules.ensure_evaluated("res.country").is_ok());
}

#[test]
fn compiling_it_and_finding_no_restriction_is_not_the_same_as_not_compiling() {
    let ruled: std::collections::HashSet<String> = ["res.partner".to_string()].into();
    let mut rules = RuleSet::with_ruled(ruled);
    rules.mark_unrestricted("res.partner".into());
    assert!(rules.ensure_evaluated("res.partner").is_ok());
    assert!(rules.get("res.partner").is_none());
}

#[test]
fn ensure_evaluated_reports_only_the_failing_model() {
    let mut rules = RuleSet::default();
    rules.mark_unevaluated("res.country".into(), "boom".into());
    assert!(rules.ensure_evaluated("res.country").is_err());
    assert!(rules.ensure_evaluated("res.partner").is_ok());
    assert!(rules.is_unevaluated("res.country"));
    assert!(!rules.is_unevaluated("res.partner"));
}

#[test]
fn parse_py_literals() {
    assert!(matches!(
        parse_py("True").unwrap(),
        odoo_kernel::security::PyExpr::Bool(true)
    ));
    assert!(matches!(
        parse_py("'x'").unwrap(),
        odoo_kernel::security::PyExpr::Str(_)
    ));
    assert!(matches!(
        parse_py("-3").unwrap(),
        odoo_kernel::security::PyExpr::Int(-3)
    ));
    assert!(matches!(
        parse_py("[]").unwrap(),
        odoo_kernel::security::PyExpr::Seq(_)
    ));
}

#[test]
fn parse_py_rejects_real_expressions() {
    assert!(parse_py("user.has_group('base.group_user')").is_err());
    assert!(parse_py("[('id','in',[1] - [2])]").is_err());
    assert!(parse_py("{'a': 1}").is_err());
}

#[test]
fn parse_py_supports_list_concatenation() {
    match parse_py("company_ids + [False]").unwrap() {
        odoo_kernel::security::PyExpr::Add(a, b) => {
            assert!(matches!(*a, odoo_kernel::security::PyExpr::Name(_)));
            assert!(matches!(*b, odoo_kernel::security::PyExpr::Seq(_)));
        }
        other => panic!("expected Add, got {other:?}"),
    }
    assert!(parse_py("[('company_id', 'in', company_ids + [False])]").is_ok());
}

#[test]
fn parse_py_chains_additions() {
    let e = parse_py("[1] + [2] + [3]").unwrap();
    assert!(matches!(e, odoo_kernel::security::PyExpr::Add(_, _)));
}

#[test]
fn parse_py_dotted_names() {
    match parse_py("user.company_id.id").unwrap() {
        odoo_kernel::security::PyExpr::Name(parts) => {
            assert_eq!(parts, vec!["user", "company_id", "id"])
        }
        other => panic!("expected Name, got {other:?}"),
    }
}

#[test]
fn an_order_on_a_field_with_a_stand_in_sorts_by_the_stand_in() {
    let mut reg = base_registry();
    reg.models
        .get_mut("res.partner")
        .unwrap()
        .fields
        .get_mut("name")
        .unwrap()
        .order_by_field = Some("id".into());
    let sql = order_sql(&reg, "res.partner", "credit_limit, name desc nulls last").unwrap();
    assert!(
        sql.contains(r#""res_partner"."id" DESC NULLS LAST"#),
        "the direction and nulls travel to the stand-in: {sql}"
    );
    assert!(!sql.contains(r#""res_partner"."name""#), "{sql}");
    let reversed = order_sql(&reg, "res.partner", "name").unwrap();
    assert!(reversed.contains(r#""res_partner"."id" ASC"#), "{reversed}");
}

#[test]
fn a_search_order_is_made_total_with_id_unless_it_names_id() {
    let reg = base_registry();
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    let items = odoo_kernel::sqlgen::parse_total_order(&ctx, m, &m.table, "name desc").unwrap();
    assert_eq!(items.len(), 2, "ties on name are broken by id");
    assert!(matches!(items[1].order, sea_query::Order::Asc));
    for order in ["name, id desc", "id", "credit_limit, id asc"] {
        let plain = parse_order(&ctx, m, &m.table, order).unwrap().len();
        let total = odoo_kernel::sqlgen::parse_total_order(&ctx, m, &m.table, order)
            .unwrap()
            .len();
        assert_eq!(plain, total, "{order} already ends ties");
    }
}

fn order_sql(reg: &Registry, model: &str, order: &str) -> anyhow::Result<String> {
    let ctx = ExprCtx::new(reg, "en_US", 1);
    let m = reg.get(model)?;
    let items = parse_order(&ctx, m, &m.table, order)?;
    let mut q = Query::select();
    q.expr(Expr::cust("1")).from(Alias::new(m.table.as_str()));
    let mut joined = std::collections::BTreeSet::new();
    for item in items {
        for j in &item.joins {
            if joined.insert(j.alias.clone()) {
                q.join_as(
                    sea_query::JoinType::LeftJoin,
                    Alias::new(j.table.as_str()),
                    Alias::new(j.alias.as_str()),
                    j.from
                        .clone()
                        .equals((Alias::new(j.alias.as_str()), Alias::new("id"))),
                );
            }
        }
        match item.nulls {
            Some(n) => q.order_by_expr_with_nulls(item.expr, item.order, n),
            None => q.order_by_expr(item.expr, item.order),
        };
    }
    Ok(q.to_string(PostgresQueryBuilder))
}

fn chain_registry() -> Registry {
    let mut a = model(
        "m.a",
        "flag desc, b_id, id",
        vec![
            field("id", FieldType::Integer),
            field("flag", FieldType::Boolean),
            m2o("b_id", "m.b"),
        ],
    );
    a.rec_name = None;
    let b = model(
        "m.b",
        "c_id, name",
        vec![
            field("id", FieldType::Integer),
            field("name", FieldType::Char),
            m2o("c_id", "m.c"),
        ],
    );
    let c = model(
        "m.c",
        "name",
        vec![
            field("id", FieldType::Integer),
            field("name", FieldType::Char),
        ],
    );
    let byid = model("m.byid", "id", vec![field("id", FieldType::Integer)]);
    let mut d = model(
        "m.d",
        "e_id",
        vec![field("id", FieldType::Integer), m2o("e_id", "m.byid")],
    );
    d.rec_name = None;
    registry(vec![a, b, c, byid, d])
}

#[test]
fn nullable_boolean_order_is_coalesced_to_false() {
    let sql = order_sql(&chain_registry(), "m.a", "flag desc").unwrap();
    assert!(
        sql.contains(r#"COALESCE("m_a"."flag", FALSE) DESC"#),
        "{sql}"
    );
}

#[test]
fn many2one_order_recurses_through_the_whole_chain() {
    let sql = order_sql(&chain_registry(), "m.a", "b_id").unwrap();

    assert!(
        sql.contains(r#""m_b" AS "m_a__b_id" ON "m_a"."b_id" = "m_a__b_id"."id""#),
        "{sql}"
    );
    assert!(
        sql.contains(
            r#""m_c" AS "m_a__b_id__c_id" ON "m_a__b_id"."c_id" = "m_a__b_id__c_id"."id""#
        ),
        "{sql}"
    );

    assert!(sql.contains(r#""m_a__b_id__c_id"."name" ASC"#), "{sql}");
}

#[test]
fn descending_many2one_reverses_the_comodel_order() {
    let sql = order_sql(&chain_registry(), "m.a", "b_id desc").unwrap();
    assert!(sql.contains(r#""m_a__b_id__c_id"."name" DESC"#), "{sql}");
    assert!(sql.contains(r#""m_a__b_id"."name" DESC"#), "{sql}");
}

#[test]
fn a_comodel_ordered_by_id_needs_no_join() {
    let sql = order_sql(&chain_registry(), "m.d", "e_id").unwrap();
    assert!(!sql.contains("JOIN"), "{sql}");
    assert!(sql.contains(r#""m_d"."e_id" ASC"#), "{sql}");
}

#[test]
fn nulls_clause_is_parsed_not_dropped() {
    let sql = order_sql(&chain_registry(), "m.a", "flag asc nulls last").unwrap();
    assert!(sql.contains("NULLS LAST"), "{sql}");
    let sql = order_sql(&chain_registry(), "m.a", "flag desc nulls first").unwrap();
    assert!(sql.contains("NULLS FIRST"), "{sql}");
}

#[test]
fn nulls_on_a_many2one_applies_to_the_foreign_key() {
    let sql = order_sql(&chain_registry(), "m.a", "b_id asc nulls last").unwrap();
    assert!(sql.contains(r#""m_a"."b_id" IS NULL ASC"#), "{sql}");
}

#[test]
fn an_unparsable_order_term_is_an_error_not_a_silent_drop() {
    assert!(order_sql(&chain_registry(), "m.a", "flag desc sideways").is_err());
}

#[test]
fn a_cycle_in_the_order_chain_terminates() {
    let mut s = model(
        "m.self",
        "peer_id, id",
        vec![field("id", FieldType::Integer), m2o("peer_id", "m.self")],
    );
    s.rec_name = None;
    let reg = registry(vec![s]);
    let sql = order_sql(&reg, "m.self", "peer_id").unwrap();
    assert!(sql.contains(r#"m_self__peer_id"#), "{sql}");
    assert_eq!(sql.matches("JOIN").count(), 1, "one hop only: {sql}");
}

fn o2m(name: &str, comodel: &str, inverse: &str) -> Field {
    let mut f = field(name, FieldType::One2many);
    f.has_column = false;
    f.relation = Some(comodel.into());
    f.relation_field = Some(inverse.into());
    f
}

fn x2m_registry(reference: bool, domain: Option<serde_json::Value>) -> Registry {
    let mut lines = o2m("line_ids", "m.line", "owner_id");
    lines.domain = domain;
    let mut owner = model(
        "m.owner",
        "id",
        vec![field("id", FieldType::Integer), lines],
    );
    owner.rec_name = None;
    let mut inv = if reference {
        let mut f = field("owner_id", FieldType::Many2oneReference);
        f.model_field = Some("res_model".into());
        f
    } else {
        m2o("owner_id", "m.owner")
    };
    inv.not_null = false;
    let mut line = model(
        "m.line",
        "id",
        vec![
            field("id", FieldType::Integer),
            field("kind", FieldType::Char),
            field("res_model", FieldType::Char),
            inv,
        ],
    );
    line.rec_name = None;
    registry(vec![owner, line])
}

fn archived_registry(field_context: Option<serde_json::Value>) -> Registry {
    let mut lines = o2m("line_ids", "m.line", "owner_id");
    lines.context = field_context;
    let mut owner = model(
        "m.owner",
        "id",
        vec![field("id", FieldType::Integer), lines],
    );
    owner.rec_name = None;
    let mut line = model(
        "m.line",
        "id",
        vec![
            field("id", FieldType::Integer),
            field("kind", FieldType::Char),
            field("active", FieldType::Boolean),
            m2o("owner_id", "m.owner"),
            m2o("tag_id", "m.tag"),
        ],
    );
    line.rec_name = None;
    line.active_name = Some("active".into());

    let mut tag = model(
        "m.tag",
        "id",
        vec![
            field("id", FieldType::Integer),
            field("name", FieldType::Char),
            field("active", FieldType::Boolean),
        ],
    );
    tag.active_name = Some("active".into());
    registry(vec![owner, line, tag])
}

fn owner_sql(reg: &Registry, dom: serde_json::Value, active_test: bool) -> anyhow::Result<String> {
    let dynamic = reg.dynamic();
    let ctx = ExprCtx::pinned(reg, dynamic, "en_US", 1, active_test);
    let m = reg.get("m.owner").unwrap();
    let rules = RuleSet::default();
    let c = Compiler::root(&ctx, m, &rules, false);
    Ok(sql_of_on(&m.table, c.compile(&domain::parse(&dom)?)?))
}

#[test]
fn an_x2many_traversal_hides_archived_comodel_rows() {
    let reg = archived_registry(None);
    let sql = owner_sql(&reg, serde_json::json!([["line_ids.kind", "=", "a"]]), true).unwrap();
    assert!(sql.contains(r#""active" IN (TRUE)"#), "got: {sql}");
}

#[test]
fn a_many2one_traversal_does_not() {
    let reg = archived_registry(None);
    let sql = owner_sql(
        &reg,
        serde_json::json!([["line_ids.tag_id.name", "=", "x"]]),
        true,
    )
    .unwrap();

    assert_eq!(
        sql.matches(r#""active" IN (TRUE)"#).count(),
        1,
        "got: {sql}"
    );
    assert!(!sql.contains(r#""m_tag"."active""#), "got: {sql}");
}

#[test]
fn the_fields_own_context_turns_the_filter_off() {
    let reg = archived_registry(Some(serde_json::json!({"active_test": false})));
    let sql = owner_sql(&reg, serde_json::json!([["line_ids.kind", "=", "a"]]), true).unwrap();
    assert!(!sql.contains(r#""active""#), "got: {sql}");
}

#[test]
fn an_unmodelled_context_key_is_refused() {
    let reg = archived_registry(Some(serde_json::json!({"default_kind": "a"})));
    let err = owner_sql(&reg, serde_json::json!([["line_ids.kind", "=", "a"]]), true).unwrap_err();
    assert!(format!("{err:#}").contains("default_kind"), "{err:#}");
}

#[test]
fn a_domain_naming_active_suppresses_the_automatic_filter() {
    let reg = archived_registry(None);
    let sql = owner_sql(
        &reg,
        serde_json::json!([["line_ids.active", "=", false]]),
        true,
    )
    .unwrap();
    assert!(!sql.contains("IN (TRUE)"), "got: {sql}");
}

#[test]
fn an_id_list_membership_test_keeps_archived_rows() {
    let reg = archived_registry(None);
    let sql = owner_sql(&reg, serde_json::json!([["line_ids", "in", [1, 2]]]), true).unwrap();
    assert!(!sql.contains(r#""active""#), "got: {sql}");
}

fn field_cond_sql(reg: &Registry) -> anyhow::Result<String> {
    let ctx = ExprCtx::new(reg, "en_US", 1);
    let owner = reg.get("m.owner")?;
    let co = reg.get("m.line")?;
    let f = &owner.fields["line_ids"];
    let rules = RuleSet::default();
    let compiler = Compiler::root(&ctx, co, &rules, false);
    Ok(sql_of_on(
        "m_line",
        Compiler::field_domain_cond(&compiler, f, co, owner)?,
    ))
}

fn sql_of_on(table: &str, cond: sea_query::Condition) -> String {
    let mut q = Query::select();
    q.expr(Expr::cust("1"))
        .from(Alias::new(table))
        .cond_where(cond);
    q.to_string(PostgresQueryBuilder)
}

#[test]
fn a_list_field_domain_is_applied_not_ignored() {
    let reg = x2m_registry(false, Some(json!([["kind", "=", "bank"]])));
    let sql = field_cond_sql(&reg).unwrap();
    assert!(sql.contains(r#""m_line"."kind" IN ('bank')"#), "{sql}");
}

#[test]
fn no_field_domain_adds_no_condition() {
    let reg = x2m_registry(false, None);
    let sql = field_cond_sql(&reg).unwrap();
    assert!(!sql.contains("kind"), "{sql}");
}

#[test]
fn a_reference_inverse_is_filtered_by_model_name() {
    let reg = x2m_registry(true, None);
    let sql = field_cond_sql(&reg).unwrap();
    assert!(sql.contains(r#""m_line"."res_model" = 'm.owner'"#), "{sql}");
}

#[test]
fn a_reference_inverse_without_a_model_field_is_refused() {
    let mut reg = x2m_registry(true, None);
    reg.models
        .get_mut("m.line")
        .unwrap()
        .fields
        .get_mut("owner_id")
        .unwrap()
        .model_field = None;
    let err = field_cond_sql(&reg).unwrap_err().to_string();
    assert!(err.contains("model_field"), "{err}");
}

#[test]
fn a_bootstrap_registry_refuses_x2many_rather_than_guessing() {
    let mut reg = x2m_registry(false, None);
    reg.source = odoo_kernel::registry::Source::Bootstrap;
    let err = field_cond_sql(&reg).unwrap_err().to_string();
    assert!(err.contains("bootstrap"), "{err}");
}

#[test]
fn an_x2many_with_a_python_domain_is_refused_not_widened() {
    let mut tags = m2m("tag_ids", "tag", "partner_tag_rel", "partner_id", "tag_id");
    tags.domain_callable = true;
    let reg = registry(vec![
        model(
            "res.partner",
            "id",
            vec![field("name", FieldType::Char), tags],
        ),
        model("tag", "id", vec![field("name", FieldType::Char)]),
    ]);
    let err = compile_res(&reg, json!([["tag_ids", "!=", false]])).unwrap_err();
    assert!(err.to_string().contains("computed in Python"), "{err}");
}

#[test]
fn a_false_leaf_renders_as_false_not_as_an_empty_condition() {
    let sql = compile(&base_registry(), json!([["name", "=", "x"], [0, "=", 1]]));
    assert!(sql.contains(" AND FALSE"), "{sql}");
    let sql = compile(
        &base_registry(),
        json!(["|", ["name", "=", "x"], [0, "=", 1]]),
    );
    assert!(sql.contains(" OR FALSE"), "{sql}");
}

#[test]
fn a_rule_domain_is_compiled_as_superuser_so_the_comodels_own_rules_do_not_apply() {
    let reg = base_registry();
    let mut rules = RuleSet::default();
    rules.insert(
        "res.country".into(),
        domain::parse(&json!([["name", "ilike", "secret"]])).unwrap(),
    );
    rules.insert(
        "res.partner".into(),
        domain::parse(&json!([["country_id", "any", [["name", "=", "x"]]]])).unwrap(),
    );
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    let c = Compiler::root(&ctx, m, &rules, false);
    let node = rules.get("res.partner").unwrap();

    let as_domain = sql_of(c.compile(node).unwrap());
    assert!(
        as_domain.contains("secret"),
        "a user's own domain traversing res.country meets its rules: {as_domain}"
    );
    let as_rule = sql_of(c.compile_rules(node).unwrap());
    assert!(
        !as_rule.contains("secret"),
        "Odoo compiles rule domains under sudo(): {as_rule}"
    );
    assert!(as_rule.contains("'x'"), "{as_rule}");
}

#[test]
fn a_rule_domain_does_not_need_read_access_on_the_comodel_it_traverses() {
    let reg = base_registry();
    let rules = RuleSet::default();
    let ctx = ExprCtx::new(&reg, "en_US", 1)
        .with_access(9, std::sync::Arc::new(std::collections::HashSet::new()));
    let m = reg.get("res.partner").unwrap();
    let c = Compiler::root(&ctx, m, &rules, false);
    let node = domain::parse(&json!([["country_id.name", "=", "BE"]])).unwrap();
    let err = c.compile(&node).unwrap_err();
    assert!(format!("{err:#}").contains("access denied"), "{err:#}");
    assert!(c.compile_rules(&node).is_ok());
}

#[test]
fn a_rule_domain_traverses_an_x2many_without_the_active_filter() {
    let reg = archived_registry(None);
    let dynamic = reg.dynamic();
    let ctx = ExprCtx::pinned(&reg, dynamic, "en_US", 1, true);
    let m = reg.get("m.owner").unwrap();
    let rules = RuleSet::default();
    let c = Compiler::root(&ctx, m, &rules, false);
    let node = domain::parse(&json!([["line_ids.kind", "=", "a"]])).unwrap();
    let as_domain = sql_of_on("m_owner", c.compile(&node).unwrap());
    assert!(as_domain.contains(r#""active" IN (TRUE)"#), "{as_domain}");
    let as_rule = sql_of_on("m_owner", c.compile_rules(&node).unwrap());
    assert!(
        !as_rule.contains(r#""active""#),
        "rules are compiled with active_test=False: {as_rule}"
    );
}

#[test]
fn a_non_text_rec_name_withdraws_the_default_display_name() {
    let mut m = model(
        "m.x",
        "id",
        vec![
            field("id", FieldType::Integer),
            m2o("partner_id", "res.partner"),
        ],
    );
    m.rec_name = Some("partner_id".into());
    Registry::normalize_model(&mut m, &std::collections::HashMap::new());
    assert!(m.rec_name.is_none());
    assert!(
        !m.display_name_default,
        "Odoo renders the partner's display_name in Python"
    );

    let mut n = model("m.y", "id", vec![field("id", FieldType::Integer)]);
    n.rec_name = None;
    Registry::normalize_model(&mut n, &std::collections::HashMap::new());
    assert!(
        n.display_name_default,
        "no _rec_name at all is Odoo's `model,id` branch"
    );

    let mut s = model(
        "m.z",
        "id",
        vec![
            field("id", FieldType::Integer),
            field("state", FieldType::Selection),
        ],
    );
    s.rec_name = Some("state".into());
    Registry::normalize_model(&mut s, &std::collections::HashMap::new());
    assert_eq!(s.rec_name.as_deref(), Some("state"));
    assert!(s.display_name_default);
}

#[test]
fn a_bypass_search_access_field_opens_its_subquery_without_the_comodels_rules() {
    let mut reg = base_registry();
    let mut rules = RuleSet::default();
    rules.insert(
        "res.country".into(),
        domain::parse(&json!([["name", "ilike", "secret"]])).unwrap(),
    );
    let dom = json!([["country_id.name", "=", "BE"]]);
    let guarded = compile_with(&reg, &rules, dom.clone()).unwrap();
    assert!(guarded.contains("secret"), "{guarded}");

    reg.models
        .get_mut("res.partner")
        .unwrap()
        .fields
        .get_mut("country_id")
        .unwrap()
        .bypass_search_access = Some(true);
    let bypassed = compile_with(&reg, &rules, dom).unwrap();
    assert!(!bypassed.contains("secret"), "{bypassed}");
    assert!(bypassed.contains("res_country"), "{bypassed}");
}

#[test]
fn a_model_that_orders_in_python_refuses_every_order() {
    let mut reg = chain_registry();
    reg.models.get_mut("m.c").unwrap().order_pure = false;
    let err = order_sql(&reg, "m.a", "b_id").unwrap_err();
    assert!(
        format!("{err:#}").contains("m.c orders its rows in Python"),
        "the refusal reaches through the many2one chain: {err:#}"
    );
    assert!(
        order_sql(&reg, "m.b", "name").is_ok(),
        "m.b itself is untouched"
    );
}

#[test]
fn a_search_count_refuses_the_x2many_active_test_knob_rather_than_ignoring_it() {
    let req: odoo_kernel::orm::Request = serde_json::from_value(json!({
        "model": "res.partner", "method": "search_count", "domain": [], "x2many_active_test": false
    }))
    .unwrap();
    assert!(req.x2many_active_test == Some(false));
}

fn translated_registry() -> Registry {
    let mut reg = base_registry();
    let mut cd = field("credit_limit", FieldType::Float);
    cd.company_dependent = true;
    cd.pg_type = "jsonb".into();
    let m = reg.models.get_mut("res.partner").unwrap();
    m.fields.get_mut("name").unwrap().translated = true;
    m.fields.insert(cd.name.clone(), cd);
    reg
}

fn built_sql(reg: &Registry, lang: &str, company: i32) -> (String, sea_query::Values) {
    let ctx = ExprCtx::new(reg, lang, company);
    let m = reg.get("res.partner").unwrap();
    let rules = RuleSet::default();
    let c = Compiler::root(&ctx, m, &rules, true);
    let cond = c
        .compile(
            &domain::parse(&json!([["name", "ilike", "a"], ["credit_limit", ">", 5]])).unwrap(),
        )
        .unwrap();
    let mut q = Query::select();
    q.expr(Expr::cust("1"))
        .from(Alias::new("res_partner"))
        .cond_where(cond);
    q.build(PostgresQueryBuilder)
}

#[test]
fn language_and_company_are_bound_parameters_so_one_plan_serves_every_identity() {
    let reg = translated_registry();
    let (fr, fr_values) = built_sql(&reg, "fr_FR", 1);
    let (de, de_values) = built_sql(&reg, "de_DE", 2);
    assert_eq!(
        fr, de,
        "the statement text must not change with lang or company"
    );
    assert_ne!(format!("{fr_values:?}"), format!("{de_values:?}"));
    assert!(!fr.contains("fr_FR") && !fr.contains("'1'"), "{fr}");
    assert!(
        fr.contains("->> 'en_US'"),
        "the English fallback stays literal: {fr}"
    );
}

#[test]
fn an_order_by_related_uses_its_own_alias_family() {
    let mut reg = base_registry();
    let mut rel = field("country_name", FieldType::Char);
    rel.has_column = false;
    rel.related = Some("country_id.name".into());
    reg.models
        .get_mut("res.partner")
        .unwrap()
        .fields
        .insert(rel.name.clone(), rel);
    let sql = order_sql(&reg, "res.partner", "country_name").unwrap();
    assert!(sql.contains(r#""ord0_res_country""#), "{sql}");
    assert!(!sql.contains("r16_"), "{sql}");
}

#[test]
fn only_security_bearing_signals_reload_the_snapshot() {
    use odoo_kernel::registry::{SignalChange, classify_signal_change, security_signals};
    let tables: Vec<String> = [
        "orm_signaling_assets",
        "orm_signaling_default",
        "orm_signaling_groups",
        "orm_signaling_registry",
        "orm_signaling_templates",
    ]
    .iter()
    .map(|t| t.to_string())
    .collect();
    let base = vec![Some(1), Some(1), Some(1), Some(1), Some(1)];
    let bump = |i: usize| {
        let mut v = base.clone();
        v[i] = Some(2);
        v
    };
    assert_eq!(
        classify_signal_change(&tables, &base, &base),
        SignalChange::None
    );
    assert_eq!(
        classify_signal_change(&tables, &base, &bump(0)),
        SignalChange::Irrelevant
    );
    assert_eq!(
        classify_signal_change(&tables, &base, &bump(4)),
        SignalChange::Irrelevant
    );
    assert_eq!(
        classify_signal_change(&tables, &base, &bump(1)),
        SignalChange::Security
    );
    assert_eq!(
        classify_signal_change(&tables, &base, &bump(2)),
        SignalChange::Security
    );
    assert_eq!(
        classify_signal_change(&tables, &base, &bump(3)),
        SignalChange::Registry
    );
    assert_eq!(
        security_signals(&tables, &bump(0)),
        security_signals(&tables, &base),
        "an asset bump must not change the key the rule cache is filed under"
    );
    assert_ne!(
        security_signals(&tables, &bump(2)),
        security_signals(&tables, &base)
    );
}

#[test]
fn an_id_membership_past_the_threshold_is_one_array_parameter() {
    use odoo_kernel::sqlgen::{col, id_membership};
    let short = sql_of(sea_query::Cond::all().add(id_membership(col("res_partner", "id"), 1..=3)));
    assert!(short.contains("IN (1, 2, 3)"), "{short}");
    let long = sql_of(sea_query::Cond::all().add(id_membership(col("res_partner", "id"), 1..=500)));
    assert!(long.contains("= ANY("), "{long}");
    assert!(!long.contains("IN ("), "{long}");
    let none = sql_of(
        sea_query::Cond::all().add(id_membership(col("res_partner", "id"), std::iter::empty())),
    );
    assert!(none.contains("FALSE"), "{none}");
}

fn count_sql(reg: &Registry, dom: serde_json::Value) -> String {
    compile(reg, dom)
}

#[test]
fn a_like_with_an_empty_or_wildcard_only_pattern_is_the_domain_odoo_makes_of_it() {
    let reg = base_registry();
    assert!(count_sql(&reg, json!([["name", "like", ""]])).ends_with("WHERE TRUE"));
    assert!(count_sql(&reg, json!([["name", "not like", ""]])).ends_with("WHERE FALSE"));
    assert!(count_sql(&reg, json!([["name", "like", "%%"]])).ends_with("WHERE TRUE"));
    let eq = count_sql(&reg, json!([["name", "=like", ""]]));
    assert!(
        eq.contains("IS NULL") && !eq.contains("LIKE"),
        "=like '' is `= False`: {eq}"
    );
    let m2o = count_sql(&reg, json!([["country_id", "not ilike", ""]]));
    assert!(
        m2o.contains(r#""country_id" IS NULL"#) && !m2o.contains("IN (SELECT"),
        "{m2o}"
    );
}

#[test]
fn a_zero_id_is_the_absence_of_a_relation() {
    let reg = base_registry();
    let sql = count_sql(&reg, json!([["country_id", "=", 0]]));
    assert!(sql.contains("IS NULL") && !sql.contains("IN (0)"), "{sql}");
    let x2m = count_sql(&reg, json!([["company_ids", "in", [0]]]));
    assert!(
        x2m.contains("NOT \"res_partner\".\"id\" IN"),
        "no-links branch: {x2m}"
    );
}

#[test]
fn scalar_and_collection_forms_are_normalised_the_way_odoo_does() {
    let reg = base_registry();
    assert!(count_sql(&reg, json!([["name", "=?", 0]])).ends_with("WHERE TRUE"));
    assert!(count_sql(&reg, json!([["name", "=", ["a", "b"]]])).contains("IN ('a', 'b')"));
    assert!(count_sql(&reg, json!([["name", "in", "a"]])).contains("IN ('a')"));
    assert!(count_sql(&reg, json!([["name", "=", []]])).contains("IS NULL"));
}

#[test]
fn numeric_and_boolean_comparands_are_coerced_from_strings() {
    let reg = base_registry();
    assert!(count_sql(&reg, json!([["credit_limit", "=", "5"]])).contains("IN (5)"));
    assert!(count_sql(&reg, json!([["credit_limit", "in", ["5", "x"]]])).contains("IN (5)"));
    assert!(count_sql(&reg, json!([["credit_limit", "=", "x"]])).ends_with("WHERE FALSE"));
    assert!(compile_res(&reg, json!([["credit_limit", ">", "x"]])).is_err());
    assert!(count_sql(&reg, json!([["active", "=", 1]])).contains("IN (TRUE)"));
    assert!(count_sql(&reg, json!([["active", "in", ["false"]]])).contains("IS NULL"));
}

#[test]
fn an_inequality_against_nothing_follows_the_field_type() {
    let reg = base_registry();
    let num = count_sql(&reg, json!([["credit_limit", ">", false]]));
    assert!(num.contains("> 0"), "{num}");
    let m2o = count_sql(&reg, json!([["country_id", ">", false]]));
    assert!(m2o.ends_with("WHERE FALSE"), "{m2o}");
}

#[test]
fn a_datetime_groupby_is_truncated_in_the_context_timezone_and_a_date_is_not() {
    use odoo_kernel::sqlgen::{col, granularity_expr};
    let render = |e: sea_query::Expr| {
        let mut q = Query::select();
        q.expr(e).from(Alias::new("t"));
        q.to_string(PostgresQueryBuilder)
    };
    let dt = render(
        granularity_expr(
            "month",
            col("t", "x"),
            false,
            Some("America/Mexico_City"),
            None,
        )
        .unwrap(),
    );
    assert!(
        dt.contains("timezone('America/Mexico_City'::text, timezone('UTC', \"t\".\"x\"))"),
        "{dt}"
    );
    let date = render(
        granularity_expr(
            "month",
            col("t", "x"),
            true,
            Some("America/Mexico_City"),
            None,
        )
        .unwrap(),
    );
    assert!(!date.contains("timezone("), "a date has no zone: {date}");
    assert!(date.ends_with("::date FROM \"t\""), "{date}");
    let utc = render(granularity_expr("hour", col("t", "x"), false, None, None).unwrap());
    assert!(
        utc.contains("date_trunc('hour'") && !utc.contains("timezone("),
        "{utc}"
    );
    assert!(
        granularity_expr("week", col("t", "x"), false, None, None).is_err(),
        "week depends on the language's week start"
    );
}

#[test]
fn a_timezone_resolves_through_the_alias_table_or_not_at_all() {
    let mut reg = base_registry();
    reg.timezones = ["Asia/Kolkata".to_string(), "UTC".to_string()].into();
    reg.timezone_aliases = [("Asia/Calcutta".to_string(), "Asia/Kolkata".to_string())].into();
    assert_eq!(
        reg.resolve_timezone("Asia/Kolkata").as_deref(),
        Some("Asia/Kolkata")
    );
    assert_eq!(
        reg.resolve_timezone("Asia/Calcutta").as_deref(),
        Some("Asia/Kolkata")
    );
    assert_eq!(
        reg.resolve_timezone("Mars/Olympus"),
        None,
        "Odoo groups such a request in UTC"
    );
}

#[test]
fn an_inequality_against_false_on_a_boolean_is_refused_not_rewritten_forever() {
    let reg = base_registry();
    let err = compile_res(&reg, json!([["active", ">", false]])).unwrap_err();
    assert!(format!("{err:#}").contains("boolean"), "{err:#}");
    let dotted = compile_res(&reg, json!([["country_id.name", ">", false]]));
    assert!(
        dotted.is_ok(),
        "a dotted inequality against nothing follows the target field: {dotted:?}"
    );
}

#[test]
fn an_x2many_membership_test_by_id_or_by_absence_runs_as_superuser() {
    let reg = base_registry();
    let mut rules = RuleSet::default();
    rules.insert(
        "res.country".into(),
        domain::parse(&json!([["name", "ilike", "secret"]])).unwrap(),
    );
    let by_domain = compile_with(
        &reg,
        &rules,
        json!([["company_ids", "any", [["name", "=", "x"]]]]),
    )
    .unwrap();
    assert!(
        by_domain.contains("secret"),
        "a domain value meets the comodel's rules: {by_domain}"
    );
    let by_id = compile_with(&reg, &rules, json!([["company_ids", "in", [1, 2]]])).unwrap();
    assert!(
        !by_id.contains("secret"),
        "Odoo browses the ids under sudo(): {by_id}"
    );
    let absent = compile_with(&reg, &rules, json!([["company_ids", "=", false]])).unwrap();
    assert!(
        !absent.contains("secret"),
        "and tests absence under sudo() too: {absent}"
    );
    let ctx = ExprCtx::new(&reg, "en_US", 1)
        .with_access(9, std::sync::Arc::new(std::collections::HashSet::new()));
    let m = reg.get("res.partner").unwrap();
    let c = Compiler::root(&ctx, m, &rules, false);
    assert!(
        c.compile(&domain::parse(&json!([["company_ids", "!=", false]])).unwrap())
            .is_ok(),
        "no ACL on the comodel is needed to test for a link"
    );
}

#[test]
fn a_week_starts_on_the_languages_first_weekday_and_number_granularities_are_date_parts() {
    use odoo_kernel::sqlgen::{col, granularity_expr, is_number_granularity};
    let render = |e: sea_query::Expr| {
        let mut q = Query::select();
        q.expr(e).from(Alias::new("t"));
        q.to_string(PostgresQueryBuilder)
    };
    let sunday = render(granularity_expr("week", col("t", "x"), false, None, Some(7)).unwrap());
    assert!(
        sunday.contains("INTERVAL '-1 DAY'"),
        "en_US starts on Sunday: {sunday}"
    );
    let monday = render(granularity_expr("week", col("t", "x"), false, None, Some(1)).unwrap());
    assert!(
        monday.contains("INTERVAL '-0 DAY'"),
        "fr_FR starts on Monday: {monday}"
    );
    let dow =
        render(granularity_expr("day_of_week", col("t", "x"), true, Some("UTC"), None).unwrap());
    assert!(
        dow.contains("date_part('dow'") && !dow.contains("::date") && !dow.contains("timezone("),
        "{dow}"
    );
    assert!(is_number_granularity("iso_week_number") && !is_number_granularity("week"));
}

#[test]
fn not_over_a_dotted_path_negates_the_traversal_not_the_comparison() {
    let reg = base_registry();
    let negated = compile(
        &reg,
        serde_json::json!(["!", ["country_id.name", "=", "BE"]]),
    );
    let not_any = compile(
        &reg,
        serde_json::json!([["country_id", "not any", [["name", "=", "BE"]]]]),
    );
    assert_eq!(negated, not_any, "`!` on a path is `head not any [...]`");
    let flipped = compile(&reg, serde_json::json!([["country_id.name", "!=", "BE"]]));
    assert_ne!(
        negated, flipped,
        "a flipped comparison keeps `any`, which drops rows whose head is unset"
    );
    let x2m = compile(
        &reg,
        serde_json::json!(["!", ["company_ids.name", "not in", ["x"]]]),
    );
    let x2m_not_any = compile(
        &reg,
        serde_json::json!([["company_ids", "not any", [["name", "not in", ["x"]]]]]),
    );
    assert_eq!(x2m, x2m_not_any);
}

#[test]
fn in_with_a_scalar_that_is_falsy_is_the_empty_set() {
    let reg = base_registry();
    assert_eq!(
        compile(&reg, serde_json::json!([["name", "in", false]])),
        compile(&reg, serde_json::json!([["id", "in", []]])),
        "`in False` is FALSE, as _optimize_in_set makes it"
    );
    assert_eq!(
        compile(&reg, serde_json::json!([["name", "not in", 0]])),
        compile(&reg, serde_json::json!([["id", "not in", []]])),
        "`not in 0` is TRUE"
    );
    assert_eq!(
        compile(&reg, serde_json::json!([["name", "in", "x"]])),
        compile(&reg, serde_json::json!([["name", "in", ["x"]]])),
        "a truthy scalar is wrapped"
    );
    assert_eq!(
        compile(&reg, serde_json::json!([["country_id.name", "in", false]])),
        compile(
            &reg,
            serde_json::json!([["country_id", "any", [["id", "in", []]]]])
        ),
        "through a path the collapse happens in the sub-domain"
    );
    let not_in = compile(
        &reg,
        serde_json::json!([["country_id.name", "not in", false]]),
    );
    assert_eq!(
        not_in,
        compile(
            &reg,
            serde_json::json!([["country_id", "any", [["id", "not in", []]]]])
        ),
        "`head.f not in False` is `head any [TRUE]`, i.e. the head must be set"
    );
    assert_ne!(
        not_in,
        compile(&reg, serde_json::json!([["id", "not in", []]])),
        "it is not TRUE: a row with no country does not match"
    );
    assert!(not_in.contains("country_id"), "got: {not_in}");
}

#[test]
fn a_properties_field_in_a_domain_is_refused() {
    let mut reg_models = vec![];
    let mut partner = model(
        "res.partner",
        "display_name, id",
        vec![
            field("id", FieldType::Integer),
            field("name", FieldType::Char),
        ],
    );
    let mut props = field("properties", FieldType::Properties);
    props.pg_type = "jsonb".into();
    partner.fields.insert("properties".into(), props);
    reg_models.push(partner);
    let reg = registry(reg_models);
    let err = compile_res(&reg, serde_json::json!([["properties", "=", false]])).unwrap_err();
    assert!(format!("{err:#}").contains("properties"), "{err:#}");
}

#[test]
fn not_negates_the_optimised_leaf_not_the_raw_one() {
    let mut partner = model(
        "res.partner",
        "display_name, id",
        vec![
            field("id", FieldType::Integer),
            field("name", FieldType::Char),
        ],
    );
    let mut stamp = field("write_date", FieldType::Datetime);
    stamp.pg_type = "timestamp".into();
    partner.fields.insert("write_date".into(), stamp);
    let reg = registry(vec![partner]);
    // `write_date <= False` is FALSE before negation, so its negation is TRUE
    let negated = compile(&reg, serde_json::json!(["!", ["write_date", "<=", false]]));
    let everything = compile(&reg, serde_json::json!([["id", "not in", []]]));
    assert_eq!(negated, everything, "got: {negated}");
    let positive = compile(&reg, serde_json::json!([["write_date", "<=", false]]));
    assert_ne!(negated, positive);
}

#[test]
fn a_string_member_of_a_relational_in_is_a_display_name_lookup() {
    let reg = base_registry();
    let by_name = compile(
        &reg,
        serde_json::json!([["country_id", "in", ["Allemagne"]]]),
    );
    let any = compile(
        &reg,
        serde_json::json!([["country_id", "any", [["display_name", "in", ["Allemagne"]]]]]),
    );
    assert_eq!(by_name, any);
    let eq = compile(&reg, serde_json::json!([["country_id", "=", "Allemagne"]]));
    assert_eq!(eq, any, "`=` goes through `in` first");
    let mixed = compile(
        &reg,
        serde_json::json!([["country_id", "in", ["Allemagne", 3]]]),
    );
    let or = compile(
        &reg,
        serde_json::json!([
            "|",
            ["country_id", "any", [["display_name", "in", ["Allemagne"]]]],
            ["country_id", "in", [3]]
        ]),
    );
    assert_eq!(
        mixed, or,
        "ids stay a membership test, OR-ed with the lookup"
    );
    let neg = compile(
        &reg,
        serde_json::json!([["country_id", "not in", ["Allemagne", 3]]]),
    );
    let and = compile(
        &reg,
        serde_json::json!([
            "&",
            [
                "country_id",
                "not any",
                [["display_name", "in", ["Allemagne"]]]
            ],
            ["country_id", "not in", [3]]
        ]),
    );
    assert_eq!(
        neg, and,
        "negative form AND-s the two, positive operator inside"
    );
    let x2m = compile(&reg, serde_json::json!([["company_ids", "in", ["x"]]]));
    assert!(x2m.contains("res_country"), "x2many too: {x2m}");
}

fn restricted_registry() -> Registry {
    let mut reg = base_registry();
    reg.group_ids.insert("base.group_system".into(), 1);
    reg.group_ids.insert("base.group_no_one".into(), 3);
    let partner = reg.models.get_mut("res.partner").unwrap();
    let mut secret = field("secret", FieldType::Char);
    secret.groups = Some("base.group_system".into());
    partner.fields.insert("secret".into(), secret);
    let mut secret_country = m2o("secret_country_id", "res.country");
    secret_country.groups = Some("base.group_system".into());
    partner
        .fields
        .insert("secret_country_id".into(), secret_country);
    let mut debug_only = field("debug_only", FieldType::Char);
    debug_only.groups = Some("base.group_no_one".into());
    partner.fields.insert("debug_only".into(), debug_only);
    reg
}

fn compile_as_user(reg: &Registry, dom: serde_json::Value) -> anyhow::Result<String> {
    let ctx = ExprCtx::new(reg, "en_US", 1)
        .with_access(9, std::sync::Arc::new(std::collections::HashSet::new()));
    let m = reg.get("res.partner").unwrap();
    let rules = RuleSet::default();
    let c = Compiler::root(&ctx, m, &rules, false);
    Ok(sql_of(c.compile(&domain::parse(&dom)?)?))
}

#[test]
fn a_restricted_many2one_cannot_be_filtered_through_a_name_search_or_a_path() {
    let reg = restricted_registry();
    for dom in [
        json!([["secret", "=", "x"]]),
        json!([["secret_country_id", "ilike", "be"]]),
        json!([["secret_country_id.name", "=", "BE"]]),
    ] {
        let err = compile_as_user(&reg, dom.clone()).expect_err("must refuse");
        assert!(
            format!("{err:#}").contains("may not filter"),
            "{dom}: {err:#}"
        );
    }
}

#[test]
fn the_debug_group_is_never_held_because_the_kernel_sees_no_session() {
    let reg = restricted_registry();
    let held: std::collections::HashSet<i32> = [3].into();
    let f = reg
        .get("res.partner")
        .unwrap()
        .fields
        .get("debug_only")
        .unwrap();
    assert!(!reg.field_readable(f, &held));
    let mut negated = f.clone();
    negated.groups = Some("!base.group_no_one".into());
    assert!(reg.field_readable(&negated, &held));
}

#[test]
fn an_order_term_the_user_may_not_read_is_dropped_as_odoo_does() {
    let reg = restricted_registry();
    let ctx = ExprCtx::new(&reg, "en_US", 1)
        .with_access(9, std::sync::Arc::new(std::collections::HashSet::new()));
    let m = reg.get("res.partner").unwrap();
    let items = parse_order(
        &ctx,
        m,
        "res_partner",
        "secret desc, name, secret_country_id",
    )
    .unwrap();
    assert_eq!(items.len(), 1, "only `name` survives");
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let items = parse_order(&ctx, m, "res_partner", "secret desc, name").unwrap();
    assert_eq!(items.len(), 2, "sudo keeps both");
}

#[test]
fn sibling_order_chains_get_distinct_join_aliases() {
    let reg = registry(vec![
        model(
            "m.root",
            "a_id, b_id",
            vec![
                field("id", FieldType::Integer),
                m2o("a_id", "m.a"),
                m2o("b_id", "m.b"),
            ],
        ),
        model(
            "m.a",
            "x_id",
            vec![field("id", FieldType::Integer), m2o("x_id", "m.x")],
        ),
        model(
            "m.b",
            "x_id",
            vec![field("id", FieldType::Integer), m2o("x_id", "m.y")],
        ),
        model(
            "m.x",
            "name",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
            ],
        ),
        model(
            "m.y",
            "name",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
            ],
        ),
    ]);
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let m = reg.get("m.root").unwrap();
    let items = parse_order(&ctx, m, "m_root", &m.order).unwrap();
    let mut aliases: Vec<String> = items
        .iter()
        .flat_map(|i| i.joins.iter().map(|j| j.alias.clone()))
        .collect();
    aliases.sort();
    aliases.dedup();
    assert_eq!(
        aliases,
        [
            "m_root__a_id",
            "m_root__a_id__x_id",
            "m_root__b_id",
            "m_root__b_id__x_id"
        ]
    );
}

#[test]
fn an_inequality_on_id_against_nothing_is_false_because_id_has_no_falsy_value() {
    let reg = base_registry();
    let sql = count_sql(&reg, json!([["id", ">", false]]));
    assert!(sql.ends_with("WHERE FALSE"), "{sql}");
}

#[test]
fn the_internal_any_bang_operators_are_refused_at_the_caller_boundary() {
    for dom in [
        json!([["country_id", "any!", [["name", "=", "x"]]]]),
        json!([["country_id", "not any!", [["name", "=", "x"]]]]),
        json!([[
            "country_id",
            "any",
            [["x_id", "any!", [["name", "=", "x"]]]]
        ]]),
    ] {
        let node = domain::parse(&dom).unwrap();
        let err = domain::reject_internal_operators(&node).expect_err("Domain() rejects it");
        assert!(
            format!("{err:#}").contains("internal operator"),
            "{dom}: {err:#}"
        );
    }
    let node = domain::parse(&json!([["country_id", "any", [["name", "=", "x"]]]])).unwrap();
    assert!(domain::reject_internal_operators(&node).is_ok());
}

#[test]
fn a_composed_any_bang_bypasses_the_comodel_rules_while_any_applies_them() {
    let reg = base_registry();
    let mut rules = RuleSet::default();
    rules.insert(
        "res.country".into(),
        domain::parse(&json!([["name", "ilike", "secret"]])).unwrap(),
    );
    let with = compile_with(
        &reg,
        &rules,
        json!([["country_id", "any", [["name", "=", "x"]]]]),
    )
    .unwrap();
    assert!(with.contains("secret"), "{with}");
    let bang = compile_with(
        &reg,
        &rules,
        json!([["country_id", "any!", [["name", "=", "x"]]]]),
    )
    .unwrap();
    assert!(!bang.contains("secret"), "{bang}");
}

fn related_registry(compute_sudo: bool, required_hop: bool) -> Registry {
    let mut reg = base_registry();
    let partner = reg.models.get_mut("res.partner").unwrap();
    let mut related = field("country_name", FieldType::Char);
    related.has_column = false;
    related.related = Some("country_id.name".into());
    related.compute_sudo = compute_sudo;
    partner.fields.insert("country_name".into(), related);
    partner.fields.get_mut("country_id").unwrap().required = required_hop;
    reg
}

#[test]
fn a_related_field_searched_for_an_unset_value_also_matches_an_unset_hop() {
    let reg = related_registry(false, false);
    let sql = count_sql(&reg, json!([["country_name", "=", false]]));
    assert!(
        sql.contains(r#""res_partner"."country_id" IN (SELECT"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""res_partner"."country_id" IS NULL"#),
        "an unset hop matches: {sql}"
    );
    let set = count_sql(&reg, json!([["country_name", "=", "BE"]]));
    assert!(
        !set.contains(r#""country_id" IS NULL"#),
        "a set value needs a set hop: {set}"
    );
}

#[test]
fn a_negative_operator_on_a_related_field_negates_the_positive_form() {
    let reg = related_registry(false, false);
    let sql = count_sql(&reg, json!([["country_name", "!=", "BE"]]));
    assert!(
        sql.contains(r#""res_partner"."country_id" IS NULL"#),
        "{sql}"
    );
    assert!(sql.contains("NOT IN (SELECT"), "{sql}");
    assert!(sql.contains("'BE'"), "{sql}");
}

#[test]
fn a_required_hop_never_gets_the_unset_branch_and_compute_sudo_traverses_as_any_bang() {
    let reg = related_registry(true, true);
    let mut rules = RuleSet::default();
    rules.insert(
        "res.country".into(),
        domain::parse(&json!([["name", "ilike", "secret"]])).unwrap(),
    );
    let sql = compile_with(&reg, &rules, json!([["country_name", "=", false]])).unwrap();
    assert!(!sql.contains(r#""country_id" IS NULL"#), "{sql}");
    assert!(
        !sql.contains("secret"),
        "compute_sudo traverses without the comodel rules: {sql}"
    );
    let reg = related_registry(false, false);
    let sql = compile_with(&reg, &rules, json!([["country_name", "=", "BE"]])).unwrap();
    assert!(
        sql.contains("secret"),
        "a non-sudo related field meets the comodel rules: {sql}"
    );
}

#[test]
fn a_many2one_reference_inequality_against_nothing_compares_with_zero() {
    let mut reg = base_registry();
    let partner = reg.models.get_mut("res.partner").unwrap();
    partner.fields.insert(
        "res_id".into(),
        field("res_id", FieldType::Many2oneReference),
    );
    let sql = count_sql(&reg, json!([["res_id", ">", false]]));
    assert!(sql.contains(r#""res_id" > 0"#), "{sql}");
}

fn datetime_registry() -> Registry {
    let mut reg = base_registry();
    let partner = reg.models.get_mut("res.partner").unwrap();
    let mut f = field("create_date", FieldType::Datetime);
    f.pg_type = "timestamp".into();
    partner.fields.insert("create_date".into(), f);
    reg
}

#[test]
fn a_bare_date_against_a_datetime_names_the_whole_utc_day() {
    let reg = datetime_registry();
    let le = count_sql(&reg, json!([["create_date", "<=", "2026-09-10"]]));
    assert!(
        le.contains(r#""create_date" < '2026-09-11 00:00:00"#),
        "{le}"
    );
    let gt = count_sql(&reg, json!([["create_date", ">", "2026-09-10"]]));
    assert!(
        gt.contains(r#""create_date" >= '2026-09-11 00:00:00"#),
        "{gt}"
    );
    let ge = count_sql(&reg, json!([["create_date", ">=", "2026-09-10"]]));
    assert!(
        ge.contains(r#""create_date" >= '2026-09-10 00:00:00"#),
        "{ge}"
    );
    let eq = count_sql(&reg, json!([["create_date", "=", "2026-09-10"]]));
    assert!(
        eq.contains(r#""create_date" >= '2026-09-10 00:00:00"#)
            && eq.contains(r#""create_date" < '2026-09-11 00:00:00"#),
        "{eq}"
    );
    let ne = count_sql(&reg, json!([["create_date", "!=", "2026-09-10"]]));
    assert!(
        ne.contains("IS NULL"),
        "negating a range keeps the unset rows: {ne}"
    );
    let exact = count_sql(&reg, json!([["create_date", "=", "2026-09-10 12:00:00"]]));
    assert!(exact.contains(r#"IN ('2026-09-10 12:00:00"#), "{exact}");
}

#[test]
fn a_bare_date_in_another_timezone_names_that_zones_day_in_utc() {
    let reg = datetime_registry();
    let compile_in = |tz: &str, dom: serde_json::Value| {
        let ctx = ExprCtx::new(&reg, "en_US", 1).with_tz(Some(tz.into()));
        let m = reg.get("res.partner").unwrap();
        let rules = RuleSet::default();
        let c = Compiler::root(&ctx, m, &rules, true);
        c.compile(&domain::parse(&dom).unwrap()).map(sql_of)
    };
    let le = compile_in(
        "Europe/Brussels",
        json!([["create_date", "<=", "2026-09-10"]]),
    )
    .unwrap();
    assert!(
        le.contains(r#""create_date" < '2026-09-10 22:00:00"#),
        "{le}"
    );
    let ge = compile_in(
        "America/Mexico_City",
        json!([["create_date", ">=", "2026-09-10"]]),
    )
    .unwrap();
    assert!(
        ge.contains(r#""create_date" >= '2026-09-10 06:00:00"#),
        "{ge}"
    );
    let jan = compile_in(
        "Europe/Brussels",
        json!([["create_date", ">", "2026-01-10"]]),
    )
    .unwrap();
    assert!(
        jan.contains(r#""create_date" >= '2026-01-10 23:00:00"#),
        "{jan}"
    );
    let err = compile_in("Mars/Olympus", json!([["create_date", "<=", "2026-09-10"]])).unwrap_err();
    assert!(format!("{err:#}").contains("zone table"), "{err:#}");
    let ts = compile_in(
        "Europe/Brussels",
        json!([["create_date", "<=", "2026-09-10 10:00:00"]]),
    )
    .unwrap();
    assert!(
        ts.contains("'2026-09-10 10:00:00"),
        "a full timestamp is not shifted: {ts}"
    );
}

#[test]
fn an_unpadded_date_is_not_a_day_because_fromisoformat_rejects_it() {
    let reg = datetime_registry();
    for bad in ["2026-9-1", "2026-09-1", " 2026-09-10"] {
        let err = compile_res(&reg, json!([["create_date", "<=", bad]]))
            .expect_err("Python raises ValueError; the kernel must not answer");
        assert!(!format!("{err:#}").is_empty(), "{bad}");
    }
}

fn company_dependent_registry(live_fallback: Option<serde_json::Value>) -> Registry {
    use odoo_kernel::registry::CompanyDefault;
    let mut reg = base_registry();
    let partner = reg.models.get_mut("res.partner").unwrap();
    let mut f = field("barcode", FieldType::Char);
    f.company_dependent = true;
    f.pg_type = "jsonb".into();
    f.index = Some("btree_not_null".into());
    f.cd_fallback = Some(json!("export-time"));
    partner.fields.insert("barcode".into(), f);
    if let Some(v) = live_fallback {
        let mut dynamic = (*reg.dynamic()).clone();
        let mut per_field: HashMap<String, CompanyDefault> = HashMap::new();
        per_field.insert(
            "barcode".into(),
            CompanyDefault {
                ordered: vec![(Some(1), v)],
            },
        );
        dynamic.defaults.insert("res.partner".into(), per_field);
        reg.set_dynamic(dynamic);
    }
    reg
}

#[test]
fn the_company_dependent_guard_follows_the_live_fallback_not_the_export() {
    // the live ir.default says 'abc': a NULL column IS 'abc', so no guard
    let reg = company_dependent_registry(Some(json!("abc")));
    let sql = count_sql(&reg, json!([["barcode", "=", "abc"]]));
    assert!(!sql.contains("IS NOT NULL"), "{sql}");
    let other = count_sql(&reg, json!([["barcode", "=", "zzz"]]));
    assert!(other.contains("IS NOT NULL"), "{other}");
    // the like family is evaluated too
    let like = count_sql(&reg, json!([["barcode", "ilike", "B"]]));
    assert!(!like.contains("IS NOT NULL"), "{like}");
    let not_like = count_sql(&reg, json!([["barcode", "not ilike", "B"]]));
    assert!(not_like.contains("IS NOT NULL"), "{not_like}");
    // an unset comparand matches a falsy fallback: no guard, the NULL rows count
    let reg = company_dependent_registry(Some(json!("")));
    let unset = count_sql(&reg, json!([["barcode", "=", false]]));
    assert!(!unset.contains("IS NOT NULL"), "{unset}");
    let set = count_sql(&reg, json!([["barcode", "!=", false]]));
    assert!(set.contains("IS NOT NULL"), "{set}");
    // without a live default the export-time value stands
    let reg = company_dependent_registry(None);
    let sql = count_sql(&reg, json!([["barcode", "=", "export-time"]]));
    assert!(!sql.contains("IS NOT NULL"), "{sql}");
}

#[test]
fn an_empty_pattern_through_a_path_is_optimised_against_the_target_field() {
    let reg = base_registry();
    let sql = count_sql(&reg, json!([["country_id.name", "ilike", ""]]));
    assert!(sql.contains("IN (SELECT"), "{sql}");
    assert!(!sql.contains("''"), "{sql}");
}

fn relational_rec_name_registry() -> Registry {
    let mut reg = base_registry();
    let mut link = model(
        "m.link",
        "id",
        vec![
            field("id", FieldType::Integer),
            m2o("country_id", "res.country"),
        ],
    );
    link.rec_name = Some("country_id".into());
    link.name_search_fields = Some(vec!["country_id".into()]);
    reg.models.insert("m.link".into(), link);
    reg
}

fn count_sql_on(reg: &Registry, model: &str, dom: serde_json::Value) -> String {
    let ctx = ExprCtx::new(reg, "en_US", 1);
    let m = reg.get(model).unwrap();
    let rules = RuleSet::default();
    let c = Compiler::root(&ctx, m, &rules, true);
    let mut q = Query::select();
    q.expr(Expr::cust("1"))
        .from(Alias::new(&m.table))
        .cond_where(c.compile(&domain::parse(&dom).unwrap()).unwrap());
    q.to_string(PostgresQueryBuilder)
}

#[test]
fn a_relational_rec_name_searches_the_comodel_display_name_on_the_path() {
    let reg = relational_rec_name_registry();
    let sql = count_sql_on(&reg, "m.link", json!([["display_name", "not ilike", "a"]]));
    assert!(sql.contains(r#""m_link"."country_id" IN (SELECT"#), "{sql}");
    assert!(!sql.contains(r#""country_id" IS NULL"#), "{sql}");
    assert!(
        sql.contains("NOT ILIKE") || sql.contains("NOT (") || sql.contains("NOT LIKE"),
        "{sql}"
    );
    let unset = count_sql_on(&reg, "m.link", json!([["display_name", "=", false]]));
    assert!(
        unset.contains(r#""m_link"."country_id" IS NULL"#),
        "{unset}"
    );
    assert!(unset.contains("IN (SELECT"), "{unset}");
}

#[test]
fn unrelated_signal_keeps_checked_security_during_concurrent_refresh() {
    let reg = base_registry();
    let checked = reg.dynamic();
    let mut newer = (*checked).clone();
    newer.langs = vec!["new_security_snapshot".into()];
    newer.signals = vec![Some(2), Some(1)];
    reg.set_dynamic(newer);
    let published = reg.dynamic();
    let stamped = reg.stamp_signals(&checked, vec![Some(1), Some(2)]);
    assert_eq!(stamped.langs, checked.langs);
    assert_eq!(stamped.signals, vec![Some(1), Some(2)]);
    assert!(std::sync::Arc::ptr_eq(&reg.dynamic(), &published));
}

fn falsy_registry() -> Registry {
    let mut id = field("id", FieldType::Integer);
    id.not_null = true;
    let mut res_id = field("res_id", FieldType::Many2oneReference);
    res_id.pg_type = "int4".into();
    let mut write_date = field("write_date", FieldType::Datetime);
    write_date.pg_type = "timestamp".into();
    registry(vec![
        model(
            "res.partner",
            "id",
            vec![
                id,
                field("name", FieldType::Char),
                field("color", FieldType::Integer),
                field("active", FieldType::Boolean),
                res_id,
                write_date,
                m2o("country_id", "res.country"),
            ],
        ),
        model(
            "res.country",
            "name",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
            ],
        ),
    ])
}

// Measured against Odoo 19 on a 73-module database; every expectation below
// is the WHERE clause `Model._search(Domain(...)).select()` actually emits.
#[test]
fn an_inequality_against_unset_folds_to_false_without_a_falsy_value() {
    let reg = falsy_registry();
    for f in ["write_date", "country_id", "id"] {
        let sql = compile(&reg, json!([[f, ">", false]]));
        assert!(sql.contains("FALSE"), "{f}: got {sql}");
        assert!(!sql.contains(&format!("\"{f}\"")), "{f}: got {sql}");
    }
    assert!(compile(&reg, json!([["write_date", "<", false]])).contains("FALSE"));
}

#[test]
fn an_inequality_against_unset_compares_against_the_falsy_value() {
    let reg = falsy_registry();

    let sql = compile(&reg, json!([["name", ">", false]]));
    assert!(sql.contains("\"name\" > ''"), "got {sql}");
    assert!(
        !sql.contains("IS NULL"),
        "'' > '' is false, so no null branch: {sql}"
    );

    // `>=` admits the falsy value itself, and NULL is stored as that value.
    let sql = compile(&reg, json!([["name", ">=", false]]));
    assert!(sql.contains("\"name\" >= ''"), "got {sql}");
    assert!(sql.contains("IS NULL"), "got {sql}");

    let sql = compile(&reg, json!([["color", ">", false]]));
    assert!(sql.contains("\"color\" > 0"), "got {sql}");
    assert!(!sql.contains("IS NULL"), "got {sql}");
}

#[test]
fn a_many2one_reference_stores_an_unset_value_as_zero() {
    let reg = falsy_registry();

    let sql = compile(&reg, json!([["res_id", "=", false]]));
    assert!(
        sql.contains("IN (0)"),
        "0 is the falsy value, not just NULL: {sql}"
    );
    assert!(sql.contains("IS NULL"), "got {sql}");

    // Odoo emits `res_id NOT IN (0)` and nothing else: SQL's own NULL
    // semantics already drop the NULL rows, and without the 0 a row holding
    // it would wrongly count as set.
    let sql = compile(&reg, json!([["res_id", "!=", false]]));
    assert!(sql.contains("NOT IN (0)"), "got {sql}");
    assert!(!sql.contains("NULL"), "got {sql}");

    // The reverse direction: an explicit 0 also matches the NULL rows.
    let sql = compile(&reg, json!([["res_id", "in", [0]]]));
    assert!(sql.contains("IS NULL"), "got {sql}");

    let sql = compile(&reg, json!([["res_id", ">=", false]]));
    assert!(sql.contains(">= 0"), "got {sql}");
    assert!(
        sql.contains("IS NULL"),
        "0 >= 0 admits the unset rows: {sql}"
    );
}

#[test]
fn id_is_the_one_integer_with_no_falsy_value() {
    let reg = falsy_registry();

    // `fields.Id` declares none, so there is nothing for False to match and
    // the NOT NULL column cannot be null either.
    let sql = compile(&reg, json!([["id", "=", false]]));
    assert!(sql.contains("FALSE"), "got {sql}");

    // Every other integer keeps the type's 0.
    let sql = compile(&reg, json!([["color", "=", false]]));
    assert!(sql.contains("IN (0)"), "got {sql}");
    assert!(sql.contains("IS NULL"), "got {sql}");
}

#[test]
fn a_boolean_field_takes_no_operator_but_membership() {
    let reg = falsy_registry();
    for op in [">", ">=", "<", "like", "ilike", "not like"] {
        let value = if op.ends_with("like") {
            json!("x")
        } else {
            json!(false)
        };
        let err = compile_res(&reg, json!([["active", op, value]])).expect_err(&format!(
            "{op} on a boolean must be refused, as Odoo raises"
        ));
        let msg = format!("{err:#}");
        assert!(msg.contains("boolean"), "{op}: got {msg}");
    }
    assert!(compile(&reg, json!([["active", "=", true]])).contains("active"));
    assert!(compile(&reg, json!([["active", "in", [true]]])).contains("active"));
}

#[test]
fn the_exported_falsy_value_overrides_what_the_type_would_say() {
    // The point of carrying `falsy` on the field rather than deriving it:
    // when Odoo's field CLASS disagrees with its type, the export wins and a
    // class this kernel has never heard of is still answered correctly.
    let mut declares_none = field("name", FieldType::Char);
    declares_none.falsy = None;
    let mut declares_one = field("res_id", FieldType::Datetime);
    declares_one.pg_type = "timestamp".into();
    declares_one.falsy = Some(json!("1970-01-01 00:00:00"));

    let reg = registry(vec![model(
        "res.partner",
        "id",
        vec![field("id", FieldType::Integer), declares_none, declares_one],
    )]);

    // A char that declares no falsy value folds, where the type rule would
    // have compared against ''.
    let sql = compile(&reg, json!([["name", ">", false]]));
    assert!(sql.contains("FALSE"), "got {sql}");

    // A datetime that declares one compares against it, where the type rule
    // would have folded the whole condition to FALSE.
    let sql = compile(&reg, json!([["res_id", ">=", false]]));
    assert!(!sql.contains("FALSE"), "got {sql}");
    assert!(sql.contains("1970-01-01"), "got {sql}");
    assert!(
        sql.contains("IS NULL"),
        "the falsy value satisfies >=, so the unset rows come too: {sql}"
    );
}

#[test]
fn the_type_derivation_is_only_a_bootstrap_fallback() {
    // What a registry built from `ir_model` alone must guess, since the
    // column type is all it can see. Both exceptions are keyed by name or
    // by type, and neither is guessable from `integer` alone.
    assert_eq!(
        FieldType::Integer.falsy_json_for_type("color"),
        Some(json!(0))
    );
    assert_eq!(FieldType::Integer.falsy_json_for_type("id"), None);
    assert_eq!(
        FieldType::Many2oneReference.falsy_json_for_type("res_id"),
        Some(json!(0))
    );
    assert_eq!(FieldType::Many2one.falsy_json_for_type("parent_id"), None);
    assert_eq!(FieldType::Datetime.falsy_json_for_type("write_date"), None);
    assert_eq!(FieldType::Char.falsy_json_for_type("name"), Some(json!("")));
}

// The comodel's record rules, and the field that turns them off.
//
// Odoo's `_optimize_any_with_rights` rewrites `any` to `any!` when the field
// declares `bypass_search_access`, and `_search(bypass_access=True)` then
// skips the comodel's ACL AND its record rules. Measured on a real database:
// an internal user who CANNOT read a private partner still finds the
// analytic account that points at it, because `partner_id` bypasses.
/// `base_registry()` with `res.partner.country_id` carrying a given
/// `bypass_search_access`, since the flag is what decides the traversal.
fn registry_with_country_bypass(bypass: Option<bool>) -> Registry {
    let mut country_id = m2o("country_id", "res.country");
    country_id.bypass_search_access = bypass;
    registry(vec![
        model(
            "res.partner",
            "display_name, id",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
                country_id,
            ],
        ),
        model(
            "res.country",
            "name",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
                field("code", FieldType::Char),
            ],
        ),
    ])
}

fn country_rule() -> RuleSet {
    let mut rules = RuleSet::default();
    rules.insert(
        "res.country".into(),
        domain::parse(&json!([["code", "=", "BE"]])).unwrap(),
    );
    rules
}

#[test]
fn a_comodels_rules_are_applied_to_an_ordinary_traversal() {
    let reg = registry_with_country_bypass(Some(false));
    let sql = compile_with(
        &reg,
        &country_rule(),
        json!([["country_id.name", "=", "X"]]),
    )
    .expect("compiles");
    assert!(
        sql.contains("\"code\""),
        "the rule belongs in the subquery: {sql}"
    );
}

#[test]
fn a_bypassing_field_drops_the_comodels_rules() {
    let reg = registry_with_country_bypass(Some(true));
    let sql = compile_with(
        &reg,
        &country_rule(),
        json!([["country_id.name", "=", "X"]]),
    )
    .expect("compiles");
    assert!(
        sql.contains("res_country"),
        "the subquery is still there: {sql}"
    );
    assert!(!sql.contains("\"code\""), "the rule must be skipped: {sql}");
}

// `any!` is Odoo's INTERNAL spelling for "skip the comodel's access", and
// `Domain()` rejects it in an incoming domain. This kernel briefly honoured
// it as a bypass, which turned a harmless extra spelling into a record-rule
// bypass a caller could ASK for: routing off gave a ValueError, routing on
// gave the rows. Refused at the parser now, so no operator string can grant
// a bypass and only the field's own declaration can.
#[test]
fn the_any_bang_operator_is_refused_as_odoo_refuses_it() {
    let reg = registry_with_country_bypass(Some(false));
    for op in ["any!", "not any!"] {
        let node = domain::parse(&json!([["country_id", op, [["name", "=", "X"]]]])).unwrap();
        let err =
            domain::reject_internal_operators(&node).expect_err("must refuse at caller boundary");
        let msg = format!("{err:#}");
        assert!(msg.contains("internal"), "{op}: got {msg}");
    }
    // the ordinary spelling still compiles, and still applies the rules
    let sql = compile_with(
        &reg,
        &country_rule(),
        json!([["country_id", "any", [["name", "=", "X"]]]]),
    )
    .expect("compiles");
    assert!(sql.contains("\"code\""), "got {sql}");
}

#[test]
fn a_registry_that_cannot_see_the_flag_refuses_rather_than_guessing() {
    // `ir_model_fields` does not record `bypass_search_access`, so a
    // bootstrap registry carries None. Applying the rules would answer with
    // fewer rows than Odoo wherever the field bypasses, and skipping them
    // would answer with more; neither is safe to guess.
    let reg = registry_with_country_bypass(None);
    let err = compile_with(
        &reg,
        &country_rule(),
        json!([["country_id.name", "=", "X"]]),
    )
    .expect_err("must refuse");
    let msg = format!("{err:#}");
    assert!(msg.contains("bypasses"), "got {msg}");

    // Only where it would have mattered: a comodel with no rules is the same
    // answer either way, so the traversal still compiles.
    let sql = compile_with(
        &reg,
        &RuleSet::default(),
        json!([["country_id.name", "=", "X"]]),
    )
    .expect("no rules, nothing to decide");
    assert!(sql.contains("res_country"), "got {sql}");
}

/// A one2many over `res.country`, inverted by a field whose column may or
/// may not exist.
fn registry_with_o2m(inverse_has_column: bool) -> Registry {
    let mut lines = field("line_ids", FieldType::One2many);
    lines.has_column = false;
    lines.relation = Some("res.country".into());
    lines.relation_field = Some("partner_id".into());

    let mut inverse = m2o("partner_id", "res.partner");
    inverse.has_column = inverse_has_column;
    inverse.stored = inverse_has_column;

    registry(vec![
        model(
            "res.partner",
            "id",
            vec![field("id", FieldType::Integer), lines],
        ),
        model(
            "res.country",
            "name",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
                inverse,
            ],
        ),
    ])
}

#[test]
fn a_one2many_whose_inverse_has_no_column_is_refused() {
    // `store` on the one2many says the INVERSE is a column, and Odoo does not
    // require that: `account.analytic.account.line_ids` inverts
    // `account.analytic.line.auto_account_id`, a non-stored many2one with a
    // `search=` method. Asking the one2many's own flag emitted
    // `s0_account_analytic_line.auto_account_id` and learned from PostgreSQL
    // that it does not exist -- one wasted round trip per call, and a
    // database ERROR in the log for a case the compiler can see.
    let reg = registry_with_o2m(false);
    let err = compile_res(&reg, json!([["line_ids.name", "=", "X"]])).expect_err("must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("auto") || msg.contains("no column to join on"),
        "got {msg}"
    );
    assert!(msg.contains("partner_id"), "names the inverse: {msg}");

    // The same shape with a real column still compiles.
    let reg = registry_with_o2m(true);
    let sql = compile(&reg, json!([["line_ids.name", "=", "X"]]));
    assert!(sql.contains("res_country"), "got {sql}");
    assert!(sql.contains("partner_id"), "got {sql}");
}

#[test]
fn a_computed_x2many_is_refused_even_when_it_names_its_relation() {
    let mut reg = registry_with_o2m(true);
    let lines = reg
        .models
        .get_mut("res.partner")
        .unwrap()
        .fields
        .get_mut("line_ids")
        .unwrap();
    lines.stored = false;
    let err = lines
        .o2m_inverse()
        .expect_err("a computed one2many has no inverse to read");
    assert!(err.to_string().contains("computed in Python"), "{err}");
    let err = compile_res(&reg, json!([["line_ids.name", "=", "X"]])).expect_err("must refuse");
    assert!(format!("{err:#}").contains("computed in Python"), "{err:#}");

    let mut reg = base_registry();
    let companies = reg
        .models
        .get_mut("res.partner")
        .unwrap()
        .fields
        .get_mut("company_ids")
        .unwrap();
    companies.stored = false;
    let err = companies
        .m2m_columns()
        .expect_err("a computed many2many has no relation table to read");
    assert!(err.to_string().contains("computed in Python"), "{err}");
    let err = compile_res(&reg, json!([["company_ids", "!=", false]])).expect_err("must refuse");
    assert!(format!("{err:#}").contains("computed in Python"), "{err:#}");
}

#[test]
fn a_one2many_read_refuses_an_inverse_with_no_column() {
    let reg = registry_with_o2m(false);
    let owner = reg.get("res.partner").unwrap();
    let co = reg.get("res.country").unwrap();
    let err = owner.fields["line_ids"]
        .o2m_inverse_column(&owner.name, co)
        .expect_err("must refuse");
    assert!(err.to_string().contains("no column to join on"), "{err}");
}

fn followed_registry() -> Registry {
    let mut partners = m2m(
        "message_partner_ids",
        "res.partner",
        "unused_rel",
        "res_id",
        "partner_id",
    );
    partners.stored = false;
    partners.custom_search = true;
    partners.search_kind = Some("mail_followers_partner".into());
    registry(vec![
        model(
            "project.project",
            "id",
            vec![
                field("id", FieldType::Integer),
                field("name", FieldType::Char),
                partners,
            ],
        ),
        model(
            "mail.followers",
            "id",
            vec![
                field("id", FieldType::Integer),
                field("res_model", FieldType::Char),
                field("res_id", FieldType::Integer),
                m2o("partner_id", "res.partner"),
            ],
        ),
        model("res.partner", "id", vec![field("name", FieldType::Char)]),
    ])
}

fn compile_as(
    reg: &Registry,
    model: &str,
    su: bool,
    dom: serde_json::Value,
) -> anyhow::Result<String> {
    let ctx = ExprCtx::new(reg, "en_US", 1);
    let m = reg.get(model)?;
    let rules = RuleSet::default();
    let c = Compiler::root(&ctx, m, &rules, su);
    Ok(sql_of(c.compile(&domain::parse(&dom)?)?))
}

#[test]
fn a_rule_on_followers_compiles_to_the_subselect_python_builds() {
    let reg = followed_registry();
    let sql = compile_as(
        &reg,
        "project.project",
        true,
        json!([["message_partner_ids", "in", [7, 9]]]),
    )
    .unwrap();
    assert!(
        sql.contains(r#""project_project"."id" IN (SELECT "s0_mail_followers"."res_id" FROM "mail_followers" AS "s0_mail_followers" WHERE "s0_mail_followers"."res_model" = 'project.project' AND"#),
        "{sql}"
    );
    assert!(sql.contains(r#""s0_mail_followers"."partner_id""#), "{sql}");
    let single = compile_as(
        &reg,
        "project.project",
        true,
        json!([["message_partner_ids", "=", 7]]),
    )
    .unwrap();
    assert!(single.contains("IN (SELECT"), "{single}");
    let negated = compile_as(
        &reg,
        "project.project",
        true,
        json!([["message_partner_ids", "not in", [7]]]),
    )
    .unwrap();
    assert!(
        negated.contains("NOT") && negated.contains("IN (SELECT"),
        "Python negates the positive search: {negated}"
    );
    let empty = compile_as(
        &reg,
        "project.project",
        true,
        json!([["message_partner_ids", "in", []]]),
    )
    .unwrap();
    assert!(empty.contains("FALSE"), "{empty}");
}

#[test]
fn a_follower_search_python_would_answer_differently_is_refused() {
    let reg = followed_registry();
    let as_user = compile_as(
        &reg,
        "project.project",
        false,
        json!([["message_partner_ids", "in", [7]]]),
    );
    assert!(format!("{:#}", as_user.unwrap_err()).contains("portal partners"));
    for dom in [
        json!([["message_partner_ids", "child_of", [7]]]),
        json!([["message_partner_ids", "in", [false]]]),
        json!([["message_partner_ids", "ilike", "bob"]]),
    ] {
        assert!(
            compile_as(&reg, "project.project", true, dom.clone()).is_err(),
            "{dom}"
        );
    }
    let mut unreviewed = followed_registry();
    unreviewed
        .models
        .get_mut("project.project")
        .unwrap()
        .fields
        .get_mut("message_partner_ids")
        .unwrap()
        .search_kind = None;
    let err = compile_as(
        &unreviewed,
        "project.project",
        true,
        json!([["message_partner_ids", "in", [7]]]),
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("custom search method"),
        "{err:#}"
    );
}

#[test]
fn a_rule_granted_by_a_constant_folds_before_its_other_branches_compile() {
    use odoo_kernel::domain::{Node, fold_constants};
    let fold = |dom: serde_json::Value| fold_constants(domain::parse(&dom).unwrap());
    assert!(matches!(
        fold(json!(["|", ["user_has_access", "=", true], [1, "=", 1]])),
        Node::True
    ));
    assert!(matches!(
        fold(json!(["&", ["name", "=", "x"], [0, "=", 1]])),
        Node::False
    ));
    assert!(matches!(
        fold(json!(["&", ["name", "=", "x"], [1, "=", 1]])),
        Node::Leaf(_)
    ));
    assert!(matches!(fold(json!(["!", [1, "=", 1]])), Node::False));
    assert!(matches!(
        fold(json!(["|", ["name", "=", "x"], "|", [0, "=", 1], ["name", "=", "y"]])),
        Node::Or(ref kept) if kept.len() == 2
    ));
}

fn trigram_registry() -> Registry {
    let mut name = field("name", FieldType::Char);
    name.translated = true;
    name.pg_type = "jsonb".into();
    name.index = Some("trigram".into());
    let mut reg = registry(vec![model(
        "res.partner",
        "id",
        vec![field("id", FieldType::Integer), name],
    )]);
    reg.has_trigram = true;
    reg.has_unaccent = true;
    reg
}

// Measured on the fixture with `SET enable_seqscan = off`: WITH this
// conjunct the plan is a `Bitmap Index Scan on product_template__name_index`;
// WITHOUT it the plan is a `Seq Scan` even with sequential scans disabled,
// which is to say the index is unusable rather than merely unattractive.
#[test]
fn a_translated_trigram_field_gets_the_indexable_conjunct() {
    let reg = trigram_registry();
    let sql = compile(&reg, json!([["name", "ilike", "abc"]]));
    assert!(
        sql.contains("jsonb_path_query_array(\"res_partner\".\"name\", '$.*')::text"),
        "the conjunct must be the index's own expression: {sql}"
    );
    assert!(sql.contains("ILIKE unaccent('%abc%')"), "got {sql}");
    // and the base condition is still there, so no row can be lost by it
    assert!(sql.contains("->> 'en_US'"), "got {sql}");
}

#[test]
fn the_conjunct_is_withheld_wherever_odoo_withholds_it() {
    let reg = trigram_registry();
    let has = |dom: serde_json::Value| compile(&reg, dom).contains("jsonb_path_query_array");

    // a run shorter than a trigram cannot constrain the index
    assert!(!has(json!([["name", "like", "ab"]])));
    // "does not contain" is not implied by the prefilter
    assert!(!has(json!([["name", "not ilike", "abc"]])));
    // and the positive case does get it, so the assertions above are not
    // passing for want of any conjunct at all
    assert!(has(json!([["name", "ilike", "abc"]])));

    // no pg_trgm: the index cannot exist, so the conjunct is dead weight
    let mut off = trigram_registry();
    off.has_trigram = false;
    assert!(!compile(&off, json!([["name", "ilike", "abc"]])).contains("jsonb_path_query_array"));
}

#[test]
fn an_untranslated_or_unindexed_field_gets_nothing() {
    // Odoo gates on BOTH `translate` and `index == "trigram"`; the index is
    // over the jsonb document, and a plain column has no such document.
    let plain = trigram_registry();
    let name = plain
        .get("res.partner")
        .unwrap()
        .fields
        .get("name")
        .unwrap()
        .clone();

    let mut untranslated = name.clone();
    untranslated.translated = false;
    let mut unindexed = name;
    unindexed.index = None;

    for f in [untranslated, unindexed] {
        let mut reg = registry(vec![model(
            "res.partner",
            "id",
            vec![field("id", FieldType::Integer), f],
        )]);
        reg.has_trigram = true;
        reg.has_unaccent = true;
        assert!(
            !compile(&reg, json!([["name", "ilike", "abc"]])).contains("jsonb_path_query_array")
        );
    }
}

#[test]
fn an_equality_on_a_translated_trigram_field_is_accelerated_too() {
    // `=` normalises to a single-valued `in`, which Odoo accelerates through
    // `value_to_translated_trigram_pattern` and compares with LIKE, not
    // ILIKE -- the equality it accompanies is case-sensitive.
    let reg = trigram_registry();
    let sql = compile(&reg, json!([["name", "=", "abcd"]]));
    assert!(sql.contains("jsonb_path_query_array"), "got {sql}");
    assert!(sql.contains("LIKE unaccent('%abcd%')"), "got {sql}");
    assert!(
        !sql.contains("ILIKE"),
        "an equality is case-sensitive: {sql}"
    );

    // Withheld where Odoo withholds it: a value below a trigram, a falsy
    // operand, more than one value, and the negative operator.
    for dom in [
        json!([["name", "=", "ab"]]),
        json!([["name", "=", false]]),
        json!([["name", "in", ["abcd", "efgh"]]]),
        json!([["name", "!=", "abcd"]]),
    ] {
        assert!(
            !compile(&reg, dom.clone()).contains("jsonb_path_query_array"),
            "{dom} must not be accelerated"
        );
    }
}

// The reachability walk asks questions the request never asked, and answering
// them through a form that REFUSES files a refusal per question -- which is
// what a campaign reads as a capability the kernel lacks. Each question has a
// non-refusing form, and these pin that the pair agree on every answer: a
// divergence would either re-fill the census with noise or silently narrow the
// walk, and the walk deciding fewer comodels means record rules left uncompiled.
#[test]
fn the_quiet_registry_lookup_agrees_with_the_demanding_one() {
    let reg = registry(vec![model(
        "res.partner",
        "id",
        vec![field("name", FieldType::Char)],
    )]);
    for name in ["res.partner", "res.users", "", "nope.nope"] {
        assert_eq!(
            reg.lookup(name).is_some(),
            reg.get(name).is_ok(),
            "the two readers disagree on {name:?}"
        );
    }
    assert_eq!(
        reg.lookup("res.partner").map(|m| m.name.as_str()),
        Some("res.partner")
    );
}

#[test]
fn the_quiet_path_walk_agrees_with_the_demanding_one() {
    let mut partner = model(
        "res.partner",
        "id",
        vec![
            field("name", FieldType::Char),
            field("parent_id", FieldType::Many2one),
        ],
    );
    partner.fields.get_mut("parent_id").unwrap().relation = Some("res.partner".into());
    // a computed field with no column and no related is the shape the walk
    // meets on `display_name` and on every python-computed field
    let mut computed = field("is_member", FieldType::Boolean);
    computed.has_column = false;
    computed.stored = false;
    partner.fields.insert("is_member".into(), computed);
    let reg = registry(vec![partner]);
    let ctx = ExprCtx::new(&reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();

    let paths: Vec<Vec<String>> = [
        vec!["name"],
        vec!["parent_id"],
        vec!["parent_id", "name"],
        vec!["parent_id", "display_name"],
        vec!["display_name"],
        vec!["is_member"],
        vec!["nope"],
        vec!["name", "nope"],
    ]
    .into_iter()
    .map(|p| p.into_iter().map(str::to_string).collect())
    .collect();

    let (mut resolved, mut refused) = (0, 0);
    for path in &paths {
        let quiet = ctx.normalize_path_seen(m, path);
        let demanding = ctx.normalize_path(m, path);
        if quiet.is_some() {
            resolved += 1
        } else {
            refused += 1
        }
        assert_eq!(
            quiet.is_some(),
            demanding.is_ok(),
            "the two readers disagree on {path:?}"
        );
        if let (Some(q), Ok(d)) = (quiet, demanding) {
            assert_eq!(q, d, "they resolved {path:?} differently");
        }
    }
    // a test where every path resolves, or none does, compares nothing
    assert!(
        resolved >= 3 && refused >= 2,
        "resolved {resolved}, refused {refused}"
    );
}

#[test]
fn a_request_naming_no_groupby_is_answered_not_refused() {
    use odoo_kernel::orm::Request;

    let req = |groupby: serde_json::Value| Request {
        id: None,
        registry_sequence: None,
        model: "res.partner".into(),
        method: "search_read".into(),
        domain: json!([]),
        fields: vec!["name".into()],
        limit: None,
        offset: None,
        order: None,
        groupby,
        aggregates: Vec::new(),
        uid: None,
        su: false,
        lang: None,
        allowed_company_ids: None,
        groupby_labels: None,
        raw_many2one: Vec::new(),
        unredacted_many2one: Vec::new(),
        groupby_hidden_labels_empty: false,
        resolved_rules: Default::default(),
        active_test: None,
        x2many_active_test: None,
        tz: None,
        root_active_test: None,
        trusted_domain: false,
    };

    // the demanding form refuses a missing groupby because read_group needs
    // one; the walk is only asking which fields the request mentions
    assert!(req(json!(null)).groupby_names().is_err());
    assert!(req(json!(null)).groupby_names_seen().is_empty());

    // and where the demanding form answers, the two must agree
    for groupby in [
        json!("country_id"),
        json!(["country_id", "state_id"]),
        json!([]),
    ] {
        let r = req(groupby.clone());
        assert_eq!(
            r.groupby_names().unwrap(),
            r.groupby_names_seen(),
            "the two readers disagree on {groupby}"
        );
    }
}

// A company-dependent many2one keeps its id inside a `jsonb` keyed by company.
// Ordering by one used to join the comodel on the RAW column, which asks
// PostgreSQL for `jsonb = integer` and kills the statement before it runs:
//
//   ERROR: operator does not exist: jsonb = integer
//
// Found by the fuzz stage on a 162-module fixture (base+mail has no such field
// on a model whose comodel is ordered by something other than id), and it hit
// `search_read` as well as `read_group` -- any order term naming the field.
#[test]
fn ordering_by_a_company_dependent_many2one_reads_it_out_of_its_jsonb() {
    let mut currency = model("res.currency", "name", vec![field("name", FieldType::Char)]);
    currency.rec_name = Some("name".into());

    let mut partner = model(
        "res.partner",
        "id",
        vec![
            field("name", FieldType::Char),
            field("property_purchase_currency_id", FieldType::Many2one),
        ],
    );
    let f = partner
        .fields
        .get_mut("property_purchase_currency_id")
        .unwrap();
    f.relation = Some("res.currency".into());
    f.company_dependent = true;
    f.pg_type = "jsonb".into();

    let reg = registry(vec![partner, currency]);
    let sql = order_sql(&reg, "res.partner", "property_purchase_currency_id").unwrap();

    // the join reads the id out of the jsonb, never the column itself
    assert!(
        sql.contains("->"),
        "the join must extract from the jsonb, got: {sql}"
    );
    assert!(
        !sql.contains(r#""res_partner"."property_purchase_currency_id" = "#),
        "the raw jsonb column is being compared to an id: {sql}"
    );

    // and a plain many2one still joins on its column, unchanged
    let mut plain = model(
        "res.partner",
        "id",
        vec![
            field("name", FieldType::Char),
            field("parent_id", FieldType::Many2one),
        ],
    );
    plain.fields.get_mut("parent_id").unwrap().relation = Some("res.currency".into());
    let reg = registry(vec![
        plain,
        model("res.currency", "name", vec![field("name", FieldType::Char)]),
    ]);
    let sql = order_sql(&reg, "res.partner", "parent_id").unwrap();
    assert!(
        sql.contains(r#""res_partner"."parent_id" = "#),
        "a plain many2one must still join on its own column, got: {sql}"
    );
}

// ---------------------------------------------------------------------------
// The write path's statement composition
//
// `harness/write_sql_contract.json` holds the statement text, and TWO tests
// read it: this one asserts the kernel composes it, and `test_shims.py`
// asserts the fork's own `PostgresBackend` composes the same thing from a stub
// model. One literal, derived twice -- so a change to either composer fails
// against the other rather than against a copy of itself.
// ---------------------------------------------------------------------------

fn write_registry() -> Registry {
    let contract = contract();
    let mut fields = vec![field("id", FieldType::Integer)];
    for (fname, spec) in contract["fields"].as_object().unwrap() {
        let translate = spec["translate"].as_str().unwrap();
        let mut f = field(fname, FieldType::Char);
        f.column_cast = Some(spec["cast"].as_str().unwrap().to_string());
        if translate != "no" {
            f.pg_type = "jsonb".into();
            f.translated = true;
        }
        // `whole` is `translate is True`; `term` is a callable such as
        // `html_translate`, stored in the same column and written differently.
        f.translate_whole = translate == "whole";
        fields.push(f);
    }
    let mut cd = field("credit_limit", FieldType::Float);
    cd.company_dependent = true;
    cd.column_cast = Some("JSONB".into());
    fields.push(cd);
    let mut nocast = field("legacy", FieldType::Char);
    nocast.column_cast = None;
    fields.push(nocast);

    let mut m = model("res.partner", "id", fields);
    m.table = contract["table"].as_str().unwrap().to_string();
    registry(vec![m])
}

fn contract() -> serde_json::Value {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/write_sql_contract.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("the contract file")).unwrap()
}

#[test]
fn the_kernel_composes_the_update_the_forks_backend_composes() {
    let reg = write_registry();
    let contract = contract();
    let cases = contract["cases"].as_array().unwrap();
    assert!(!cases.is_empty(), "the contract file names no case");
    for case in cases {
        let fnames: Vec<String> = case["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let shape = match case["shape"].as_str().unwrap() {
            "uniform" => odoo_kernel::write::UpdateShape::Uniform,
            "values" => odoo_kernel::write::UpdateShape::Values,
            other => panic!("unknown shape {other}"),
        };
        let rows = case["rows"].as_u64().unwrap() as usize;
        let got =
            odoo_kernel::write::update_rows_sql(&reg, "res.partner", &fnames, shape, rows).unwrap();
        assert_eq!(
            got,
            case["sql"].as_str().unwrap(),
            "{}",
            case["name"].as_str().unwrap()
        );
    }
}

#[test]
fn the_kernel_composes_the_insert_the_forks_backend_composes() {
    let reg = write_registry();
    let contract = contract();
    let cases = contract["insert_cases"].as_array().unwrap();
    assert!(!cases.is_empty(), "the contract file names no insert case");
    for case in cases {
        let columns: Vec<String> = case["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let rows = case["rows"].as_u64().unwrap() as usize;
        let got = odoo_kernel::write::insert_rows_sql(&reg, "res.partner", &columns, rows).unwrap();
        assert_eq!(
            got,
            case["sql"].as_str().unwrap(),
            "{}",
            case["name"].as_str().unwrap()
        );
    }
}

#[test]
fn an_insert_naming_a_column_this_registry_lacks_is_refused() {
    // A column added by an upgrade the kernel was not rebuilt for: refusing
    // here hands the create to the delegate instead of an error mid-create.
    let reg = write_registry();
    for (column, want) in [
        ("nonesuch", "has no field nonesuch"),
        ("legacy", "no declared column cast"),
    ] {
        let err =
            odoo_kernel::write::insert_rows_sql(&reg, "res.partner", &[column.to_string()], 1)
                .unwrap_err()
                .to_string();
        assert!(
            err.contains(want),
            "{column}: {err} does not mention {want}"
        );
    }
}

#[test]
fn a_whole_value_translated_column_binds_its_value_three_times() {
    let reg = write_registry();
    let m = reg.get("res.partner").unwrap();
    // The merge expression names the value three times, so the caller binds it
    // three times; a term-translated one is a plain assignment.
    assert_eq!(
        odoo_kernel::write::value_repeats(m.fields.get("title").unwrap()),
        3
    );
    assert_eq!(
        odoo_kernel::write::value_repeats(m.fields.get("body").unwrap()),
        1
    );
    assert_eq!(
        odoo_kernel::write::value_repeats(m.fields.get("name").unwrap()),
        1
    );
}

#[test]
fn the_columns_whose_statement_would_need_pythons_knowledge_are_refused() {
    let reg = write_registry();
    for (fname, want) in [
        // the assignment interpolates ir.default's per-company fallbacks
        ("credit_limit", "company-dependent"),
        // a registry built from ir_model carries no declared cast
        ("legacy", "no declared column cast"),
        // and a name this registry does not carry at all
        ("nonesuch", "has no field nonesuch"),
    ] {
        let err = odoo_kernel::write::update_rows_sql(
            &reg,
            "res.partner",
            &[fname.to_string()],
            odoo_kernel::write::UpdateShape::Values,
            1,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(want), "{fname}: {err} does not mention {want}");
    }
}

#[test]
fn a_group_with_one_refused_column_refuses_whole() {
    // The delegate writes the group; splitting it here would issue two
    // statements where Python issues one, and the second would not see the
    // first's row locks in the order Python takes them.
    let reg = write_registry();
    assert!(
        odoo_kernel::write::update_rows_sql(
            &reg,
            "res.partner",
            &["name".to_string(), "credit_limit".to_string()],
            odoo_kernel::write::UpdateShape::Values,
            1,
        )
        .is_err()
    );
}

// ---------------------------------------------------------------------------
// The port's dialect: `$N` into Odoo's `%s`
// ---------------------------------------------------------------------------

#[test]
fn placeholders_become_percent_s_in_text_order_one_parameter_each() {
    use sea_query::Value;
    let (sql, params) = odoo_kernel::write::to_odoo_dialect(
        "\"t\".\"a\" = $2 AND \"t\".\"b\" = $1 AND \"t\".\"c\" = $2",
        &[Value::from(10i32), Value::from("x")],
    )
    .unwrap();
    assert_eq!(
        sql,
        "\"t\".\"a\" = %s AND \"t\".\"b\" = %s AND \"t\".\"c\" = %s"
    );
    // reordered and repeated: each occurrence binds its own copy, in the order
    // the text names them, because Odoo's cursor has no numbered parameters
    assert_eq!(params, vec![json!("x"), json!(10), json!("x")]);
}

#[test]
fn a_dollar_inside_a_literal_is_text_and_a_percent_is_doubled() {
    let (sql, params) = odoo_kernel::write::to_odoo_dialect(
        "jsonb_path_query_first(\"t\".\"n\", '$.*') LIKE '50% $1' AND \"t\".\"x\" % 2 = 0",
        &[],
    )
    .unwrap();
    assert_eq!(
        sql,
        "jsonb_path_query_first(\"t\".\"n\", '$.*') LIKE '50%% $1' AND \"t\".\"x\" %% 2 = 0"
    );
    assert!(params.is_empty());
}

#[test]
fn dates_and_datetimes_are_tagged_so_they_do_not_bind_as_text() {
    use sea_query::Value;
    let d = chrono::NaiveDate::from_ymd_opt(2026, 9, 12).unwrap();
    let dt = d.and_hms_opt(6, 30, 0).unwrap();
    let (_, params) = odoo_kernel::write::to_odoo_dialect(
        "$1 $2 $3",
        &[
            Value::from(d),
            Value::from(dt),
            Value::Array(
                sea_query::ArrayType::Int,
                Some(Box::new(vec![Value::from(1i32), Value::from(2i32)])),
            ),
        ],
    )
    .unwrap();
    assert_eq!(
        params,
        vec![
            json!({"__date__": "2026-09-12"}),
            json!({"__datetime__": "2026-09-12 06:30:00"}),
            json!([1, 2]),
        ]
    );
}

#[test]
fn a_placeholder_with_no_value_or_an_open_literal_is_refused() {
    assert!(
        odoo_kernel::write::to_odoo_dialect("a = $2", &[sea_query::Value::from(1i32)]).is_err()
    );
    assert!(odoo_kernel::write::to_odoo_dialect("a = 'open", &[]).is_err());
}

#[test]
fn a_conditional_equality_on_a_dotted_path_is_decided_inside_the_traversal() {
    // `('country_id.name', '=?', '')` is `country_id any [name =? '']` in Odoo,
    // which is `country_id any []`: the partner HAS a country. Collapsing the
    // whole leaf to TRUE first answered every row.
    let reg = base_registry();
    let unset = compile_res(&reg, json!([["country_id.name", "=?", ""]])).unwrap();
    assert_eq!(
        unset,
        compile_res(&reg, json!([["country_id", "any", []]])).unwrap()
    );
    assert_ne!(unset, compile_res(&reg, json!([])).unwrap());

    let set = compile_res(&reg, json!([["country_id.name", "=?", "Mexico"]])).unwrap();
    assert_eq!(
        set,
        compile_res(
            &reg,
            json!([["country_id", "any", [["name", "=", "Mexico"]]]])
        )
        .unwrap()
    );

    // a plain field keeps the collapse, which is what `=?` is for
    assert_eq!(
        compile_res(&reg, json!([["name", "=?", false]])).unwrap(),
        compile_res(&reg, json!([])).unwrap()
    );
}

#[test]
fn a_dotted_leaf_compiles_as_its_any_form_for_every_operator_that_folds() {
    // Odoo turns `a.b op v` into `a any [b op v]` before it optimises the
    // operator. `=?` was the one rewrite the kernel applied to the whole path
    // first; these are the operators and values whose leaf can fold to a
    // constant, which is exactly where applying it outside the traversal would
    // change the answer.
    let reg = base_registry();
    let mut diffs = Vec::new();
    for (op, v) in [
        ("=", json!(false)),
        ("!=", json!(false)),
        ("in", json!([])),
        ("not in", json!([])),
        ("in", json!([false])),
        ("not in", json!([false])),
        ("=?", json!(false)),
        ("like", json!("")),
        ("ilike", json!("")),
        ("not ilike", json!("")),
        ("not like", json!("")),
        ("=like", json!("%")),
        ("=ilike", json!("%")),
        (">", json!(false)),
        ("<", json!(false)),
        ("=", json!("x")),
        ("!=", json!("x")),
        ("not ilike", json!("x")),
        ("in", json!(["x", false])),
        ("not in", json!(["x", false])),
    ] {
        let dotted = compile_res(&reg, json!([["country_id.name", op, v]]));
        let any = compile_res(&reg, json!([["country_id", "any", [["name", op, v]]]]));
        match (dotted, any) {
            (Ok(a), Ok(b)) if a == b => {}
            (a, b) => diffs.push(format!("{op} {v}:\n   dotted {a:?}\n   any    {b:?}")),
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}
