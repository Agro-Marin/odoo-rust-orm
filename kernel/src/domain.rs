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

pub const MAX_DOMAIN_NESTING: usize = 100;

struct Item {
    node: Node,
    depth: usize,
}

fn checked(depth: usize) -> Result<usize> {
    if depth > MAX_DOMAIN_NESTING {
        refuse!(
            "domain nesting too deep (>{MAX_DOMAIN_NESTING} levels); refusing \
             to build it rather than recurse over it"
        );
    }
    Ok(depth)
}

fn nary(op: &str, operands: Vec<Item>) -> Result<Item> {
    let mut children = Vec::new();
    let mut depth = 1;
    for item in operands {
        let same = matches!((&item.node, op), (Node::And(_), "&") | (Node::Or(_), "|"));
        if same {
            let inner = match item.node {
                Node::And(v) | Node::Or(v) => v,
                _ => unreachable!("guarded by `same`"),
            };
            children.extend(inner);
            depth = depth.max(item.depth);
        } else {
            depth = depth.max(item.depth + 1);
            children.push(item.node);
        }
    }
    let node = if op == "&" {
        Node::And(children)
    } else {
        Node::Or(children)
    };
    Ok(Item {
        node,
        depth: checked(depth)?,
    })
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
    let mut stack: Vec<Item> = Vec::new();

    for item in items.iter().rev() {
        match item {
            Json::String(op) if op == "&" || op == "|" => {
                let a = stack.pop();
                let b = stack.pop();
                let (Some(a), Some(b)) = (a, b) else {
                    refuse!("operator {op} missing operands");
                };
                stack.push(nary(op, vec![a, b])?);
            }
            Json::String(op) if op == "!" => {
                let Some(a) = stack.pop() else {
                    refuse!("operator ! missing operand");
                };
                stack.push(match a.node {
                    Node::Not(inner) => Item {
                        node: *inner,
                        depth: a.depth - 1,
                    },
                    other => Item {
                        node: Node::Not(Box::new(other)),
                        depth: checked(a.depth + 1)?,
                    },
                });
            }
            Json::Array(leaf) if leaf.len() == 3 => {
                stack.push(Item {
                    node: parse_leaf(leaf)?,
                    depth: 1,
                });
            }
            other => refuse!("invalid domain term {other}"),
        }
    }

    stack.reverse();
    let node = match stack.len() {
        0 => Node::True,
        1 => stack.pop().unwrap().node,
        n => {
            tracing::trace!(
                target: "odoo_kernel::domain",
                members = n,
                "the domain left several terms on the stack; ANDing them implicitly"
            );
            nary("&", stack)?.node
        }
    };
    Ok(node)
}

pub fn fold_constants(node: Node) -> Node {
    match node {
        Node::And(children) => {
            let mut kept = Vec::with_capacity(children.len());
            for child in children.into_iter().map(fold_constants) {
                match child {
                    Node::False => return Node::False,
                    Node::True => {}
                    other => kept.push(other),
                }
            }
            match kept.len() {
                0 => Node::True,
                1 => kept.pop().unwrap(),
                _ => Node::And(kept),
            }
        }
        Node::Or(children) => {
            let mut kept = Vec::with_capacity(children.len());
            for child in children.into_iter().map(fold_constants) {
                match child {
                    Node::True => return Node::True,
                    Node::False => {}
                    other => kept.push(other),
                }
            }
            match kept.len() {
                0 => Node::False,
                1 => kept.pop().unwrap(),
                _ => Node::Or(kept),
            }
        }
        Node::Not(inner) => match fold_constants(*inner) {
            Node::True => Node::False,
            Node::False => Node::True,
            other => Node::Not(Box::new(other)),
        },
        leaf => leaf,
    }
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
        assert!(matches!(parse(&json!([])).unwrap(), Node::True));
        assert!(parse_nested(&json!("private")).is_none());
    }
}
