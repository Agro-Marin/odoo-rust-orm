use crate::error::{refusal, refuse};
use anyhow::Result;
use serde_json::Value as Json;

#[derive(Debug, Clone)]
pub enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Leaf),
    True,
    False,
}

#[derive(Debug, Clone)]
pub struct Leaf {
    pub field: String,
    pub op: String,
    pub value: Json,
}

pub fn parse(domain: &Json) -> Result<Node> {
    let Json::Array(items) = domain else {
        refuse!("domain must be a JSON array, got {domain}");
    };
    tracing::trace!(
        target: "odoo_kernel::domain",
        terms = items.len(),
        domain = %domain,
        "parsing a prefix-notation domain"
    );
    let mut stack: Vec<Node> = Vec::new();

    for item in items.iter().rev() {
        match item {
            Json::String(op) if op == "&" || op == "|" => {
                let a = stack.pop();
                let b = stack.pop();
                let (Some(a), Some(b)) = (a, b) else {
                    refuse!("operator {op} missing operands");
                };
                stack.push(if op == "&" {
                    Node::And(vec![a, b])
                } else {
                    Node::Or(vec![a, b])
                });
            }
            Json::String(op) if op == "!" => {
                let Some(a) = stack.pop() else {
                    refuse!("operator ! missing operand");
                };
                stack.push(Node::Not(Box::new(a)));
            }
            Json::Array(leaf) if leaf.len() == 3 => {
                stack.push(parse_leaf(leaf)?);
            }
            other => refuse!("invalid domain term {other}"),
        }
    }

    stack.reverse();
    let node = match stack.len() {
        0 => Node::True,
        1 => stack.pop().unwrap(),
        // an implicit conjunction: Odoo ANDs the terms a prefix domain left
        // on the stack, and a domain that relies on it reads differently
        // from one that spells `&` out
        n => {
            tracing::trace!(
                target: "odoo_kernel::domain",
                members = n,
                "the domain left several terms on the stack; ANDing them implicitly"
            );
            Node::And(stack)
        }
    };
    Ok(node)
}

fn parse_leaf(leaf: &[Json]) -> Result<Node> {
    let op = leaf[1]
        .as_str()
        .ok_or_else(|| refusal!("leaf operator must be a string"))?
        .to_string();

    if let Some(n) = leaf[0].as_i64() {
        let v = leaf[2].as_i64().unwrap_or(-1);
        let constant = match (n == v, op.as_str()) {
            (true, "=") | (false, "!=") => Node::True,
            _ => Node::False,
        };
        // `[0, '=', 1]` and its siblings are Odoo's spelling of a constant;
        // they carry no field and collapse before any column is consulted
        tracing::trace!(
            target: "odoo_kernel::domain",
            left = n, right = v, %op, folded = ?constant,
            "folded a constant leaf"
        );
        return Ok(constant);
    }
    let field = leaf[0]
        .as_str()
        .ok_or_else(|| refusal!("leaf field must be a string"))?
        .to_string();
    Ok(Node::Leaf(Leaf {
        field,
        op,
        value: leaf[2].clone(),
    }))
}

/// A value that MAY be a nested domain, as `Option` rather than `Result`.
///
/// Three callers ask "is this leaf value itself a domain?" and walk into it
/// when it is: the reachability seeds, the comodel walk, and the internal
/// operator check. A `no` from them is an answer, not a refusal -- and routing
/// it through `parse` files one `odoo_kernel::refusal` line per scalar leaf in
/// every domain the kernel sees, which buries the refusals that are real.
///
/// The filter is exactly `parse`'s own item-level grammar, so anything it
/// accepts `parse` accepts: this decides nothing `parse` would decide
/// differently.
pub fn parse_nested(value: &Json) -> Option<Node> {
    let items = value.as_array()?;
    let looks_like_a_domain = items.iter().all(|item| match item {
        Json::String(s) => matches!(s.as_str(), "&" | "|" | "!"),
        Json::Array(leaf) => leaf.len() == 3,
        _ => false,
    });
    if !looks_like_a_domain {
        return None;
    }
    parse(value).ok()
}

pub fn referenced_fields(node: &Node, out: &mut Vec<String>) {
    match node {
        Node::And(v) | Node::Or(v) => v.iter().for_each(|n| referenced_fields(n, out)),
        Node::Not(n) => referenced_fields(n, out),
        Node::Leaf(l) => out.push(l.field.split('.').next().unwrap_or(&l.field).to_string()),
        _ => {}
    }
}

pub fn reject_internal_operators(node: &Node) -> Result<()> {
    match node {
        Node::And(v) | Node::Or(v) => v.iter().try_for_each(reject_internal_operators),
        Node::Not(n) => reject_internal_operators(n),
        Node::Leaf(l) => {
            if matches!(l.op.as_str(), "any!" | "not any!") {
                refuse!(
                    "{:?} is an internal operator that Domain() rejects from a caller",
                    l.op
                );
            }
            if matches!(l.op.as_str(), "any" | "not any")
                && let Some(sub) = parse_nested(&l.value)
            {
                reject_internal_operators(&sub)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Every shape a leaf value can take, against both readers. `parse_nested`
    // exists only to keep a "no" out of the refusal log, so it must answer
    // exactly what `parse` answers -- a divergence here would silently change
    // which comodels the reachability walk reaches.
    #[test]
    fn the_quiet_reader_accepts_exactly_what_parse_accepts() {
        let values = [
            json!([]),
            json!([["a", "=", 1]]),
            json!(["|", ["a", "=", 1], ["b", "=", 2]]),
            json!(["!", ["a", "=", 1]]),
            json!(["private"]),
            json!([1, 2, 3]),
            json!("private"),
            json!(false),
            json!(3),
            json!({"a": 1}),
            json!([["a", "="]]),
            json!([["a", "=", 1], "unknown"]),
        ];
        for value in values {
            assert_eq!(
                parse_nested(&value).is_some(),
                parse(&value).is_ok(),
                "the two readers disagree on {value}"
            );
        }
    }

    #[test]
    fn a_quiet_no_is_not_an_empty_domain() {
        // `parse` reads an absent domain as TRUE; `parse_nested` must not,
        // or a scalar leaf value would look like "match everything"
        assert!(matches!(parse(&json!([])).unwrap(), Node::True));
        assert!(parse_nested(&json!("private")).is_none());
    }
}
