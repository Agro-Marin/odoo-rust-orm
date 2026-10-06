use odoo_kernel::registry::{Dynamic, Field, FieldType, Model, Registry, Security};
use std::collections::HashMap;

pub fn field(name: &str, ttype: FieldType) -> Field {
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
        python_read_access: Some(false),
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

pub fn model(name: &str, order: &str, fields: Vec<Field>) -> Model {
    Model {
        name: name.into(),
        table: name.replace('.', "_"),
        order: order.into(),
        fields: fields.into_iter().map(|f| (f.name.clone(), f)).collect(),
        rec_name: Some("name".into()),
        parent_name: None,
        parent_store: false,
        inherits_rules: true,
        table_inheritance_root: None,
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
        access_guard_pure: true,
        access_company_anchor: None,
        access_anchors: None,
        name_search_fields: Some(vec!["name".into()]),
        display_name_search_exact: Vec::new(),
        impure_read_methods: Vec::new(),
    }
}

pub fn registry(models: Vec<Model>) -> Registry {
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
