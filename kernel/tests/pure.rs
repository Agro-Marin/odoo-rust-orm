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
        related: None,
        domain: None,
        domain_callable: false,
        model_field: None,
        index: None,
        cd_fallback: None,
        custom_search: false,
        context: None,
        groups: None,
        // What a registry built from `ir_model` alone would derive; an
        // export overrides it with the value the field CLASS declares.
        falsy: ttype.falsy_json_for_type(name),
        // What a live export records for a field that does not bypass; a
        // bootstrap registry would carry `None` and refuse the traversal.
        bypass_search_access: Some(false),
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
        has_active: false,
        rec_name: Some("name".into()),
        parent_name: None,
        active_name: None,

        read_path_pure: true,
        search_pure: true,
        display_name_default: true,
        name_search_fields: Some(vec!["name".into()]),
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
    let c = Compiler {
        ctx: &ctx,
        model: m,
        alias: m.table.clone(),
        rules: &rules,
        su: true,
        depth: 0,
        stack: Vec::new(),
    };
    sql_of(c.compile(&domain::parse(&dom).unwrap()).unwrap())
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
fn an_equality_on_display_name_is_refused() {
    let reg = base_registry();
    assert!(compile_res(&reg, serde_json::json!([["display_name", "=", "ac"]])).is_err());
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
fn an_empty_pattern_matches_every_comodel_row() {
    let reg = base_registry();
    let sql = compile(&reg, serde_json::json!([["country_id", "ilike", ""]]));
    assert!(!sql.contains("ILIKE"), "got: {sql}");
    assert!(sql.contains(r#""country_id" IN (SELECT"#), "got: {sql}");
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
fn x2many_rejects_name_search_values() {
    let reg = base_registry();
    let rules = RuleSet::default();
    assert!(compile_with(&reg, &rules, json!([["company_ids", "in", ["Belgium"]]])).is_err());
}

fn compile_with(reg: &Registry, rules: &RuleSet, dom: serde_json::Value) -> anyhow::Result<String> {
    let ctx = ExprCtx::new(reg, "en_US", 1);
    let m = reg.get("res.partner").unwrap();
    let c = Compiler {
        ctx: &ctx,
        model: m,
        alias: m.table.clone(),
        rules,
        su: false,
        depth: 0,
        stack: Vec::new(),
    };
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
                    odoo_kernel::sqlgen::col(&j.from_alias, &j.from_col)
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
        sql.contains(r#""m_b" AS "ord_0_b_id" ON "m_a"."b_id" = "ord_0_b_id"."id""#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""m_c" AS "ord_1_c_id" ON "ord_0_b_id"."c_id" = "ord_1_c_id"."id""#),
        "{sql}"
    );

    assert!(sql.contains(r#""ord_1_c_id"."name" ASC"#), "{sql}");
}

#[test]
fn descending_many2one_reverses_the_comodel_order() {
    let sql = order_sql(&chain_registry(), "m.a", "b_id desc").unwrap();
    assert!(sql.contains(r#""ord_1_c_id"."name" DESC"#), "{sql}");
    assert!(sql.contains(r#""ord_0_b_id"."name" DESC"#), "{sql}");
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
    assert!(sql.contains(r#"ord_0_peer_id"#), "{sql}");
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
    line.has_active = true;

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
    tag.has_active = true;
    registry(vec![owner, line, tag])
}

fn owner_sql(reg: &Registry, dom: serde_json::Value, active_test: bool) -> anyhow::Result<String> {
    let dynamic = reg.dynamic();
    let ctx = ExprCtx::pinned(reg, dynamic, "en_US", 1, active_test);
    let m = reg.get("m.owner").unwrap();
    let c = Compiler {
        ctx: &ctx,
        model: m,
        alias: m.table.clone(),
        rules: &RuleSet::default(),
        su: false,
        depth: 0,
        stack: Vec::new(),
    };
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
    let compiler = Compiler {
        ctx: &ctx,
        model: co,
        alias: co.table.clone(),
        rules: &rules,
        su: false,
        depth: 0,
        stack: Vec::new(),
    };
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
        let err = compile_res(&reg, json!([["country_id", op, [["name", "=", "X"]]]]))
            .expect_err("must refuse");
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
