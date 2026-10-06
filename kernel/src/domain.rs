use crate::error::{refusal, refuse};
use anyhow::Result;
use serde_json::Value as Json;
use std::collections::VecDeque;

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
    node: Pending,
    depth: usize,
}

// Keep a junction's children appendable at either end while parsing. A long
// right-associated prefix expression otherwise copies its entire suffix at
// every operator. Public nodes remain compact vectors after construction.
enum Pending {
    Node(Node),
    Junction { and: bool, children: VecDeque<Node> },
}

impl From<Node> for Pending {
    fn from(node: Node) -> Self {
        match node {
            Node::And(children) => Self::Junction {
                and: true,
                children: children.into(),
            },
            Node::Or(children) => Self::Junction {
                and: false,
                children: children.into(),
            },
            node => Self::Node(node),
        }
    }
}

impl Pending {
    fn finish(self) -> Node {
        match self {
            Self::Node(node) => node,
            Self::Junction { and, children } => {
                if and {
                    Node::And(children.into())
                } else {
                    Node::Or(children.into())
                }
            }
        }
    }
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

fn nary(and: bool, operands: impl IntoIterator<Item = Item>) -> Result<Item> {
    let mut children = VecDeque::new();
    let mut depth = 1;
    for item in operands {
        match item.node {
            Pending::Junction {
                and: other,
                children: mut inner,
            } if and == other => {
                // Move the smaller side; prepending in reverse preserves order.
                if children.len() < inner.len() {
                    for child in children.into_iter().rev() {
                        inner.push_front(child);
                    }
                    children = inner;
                } else {
                    children.extend(inner);
                }
                depth = depth.max(item.depth);
            }
            node => {
                depth = depth.max(item.depth + 1);
                children.push_back(node.finish());
            }
        }
    }
    Ok(Item {
        node: Pending::Junction { and, children },
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
                stack.push(nary(op == "&", [a, b])?);
            }
            Json::String(op) if op == "!" => {
                let Some(a) = stack.pop() else {
                    refuse!("operator ! missing operand");
                };
                stack.push(match a.node.finish() {
                    Node::Not(inner) => Item {
                        node: Pending::from(*inner),
                        depth: a.depth - 1,
                    },
                    other => Item {
                        node: Pending::Node(Node::Not(Box::new(other))),
                        depth: checked(a.depth + 1)?,
                    },
                });
            }
            Json::Array(leaf) if leaf.len() == 3 => {
                stack.push(Item {
                    node: Pending::from(parse_leaf(leaf)?),
                    depth: 1,
                });
            }
            other => refuse!("invalid domain term {other}"),
        }
    }

    stack.reverse();
    let node = match stack.len() {
        0 => Node::True,
        1 => stack.pop().unwrap().node.finish(),
        n => {
            tracing::trace!(
                target: "odoo_kernel::domain",
                members = n,
                "the domain left several terms on the stack; ANDing them implicitly"
            );
            nary(true, stack)?.node.finish()
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

    // Domain accepts only TRUE_LEAF (1, '=', 1) and FALSE_LEAF (0, '=', 1),
    // using Python tuple equality, including numerically equal bools/floats.
    // Other non-string operands are invalid, not expressions to evaluate.
    let sentinel_number = |v: &Json| match v {
        Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Json::Number(n) => n.as_f64(),
        _ => None,
    };
    if !leaf[0].is_string() {
        let constant = match (
            sentinel_number(&leaf[0]),
            op.as_str(),
            sentinel_number(&leaf[2]),
        ) {
            (Some(1.0), "=", Some(1.0)) => Node::True,
            (Some(0.0), "=", Some(1.0)) => Node::False,
            _ => refuse!("invalid constant domain leaf: {leaf:?}"),
        };
        tracing::trace!(
            target: "odoo_kernel::domain",
            left = %leaf[0], right = %leaf[2], %op, folded = ?constant,
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
    fn junction_building_preserves_truth_tables_and_leaf_order() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_test_writer()
            .try_init();
        fn next(seed: &mut u64) -> u64 {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *seed >> 32
        }
        fn generate(out: &mut Vec<Json>, seed: &mut u64, depth: usize) {
            match if depth == 0 { 0 } else { next(seed) % 4 } {
                0 => out.push(json!(["flag", "=", next(seed) % 5])),
                1 => {
                    out.push(json!("!"));
                    generate(out, seed, depth - 1);
                }
                kind => {
                    out.push(json!(if kind == 2 { "&" } else { "|" }));
                    generate(out, seed, depth - 1);
                    generate(out, seed, depth - 1);
                }
            }
        }
        fn prefix(tokens: &[Json], index: &mut usize, mask: u32) -> bool {
            let item = &tokens[*index];
            *index += 1;
            match item.as_str() {
                Some("!") => !prefix(tokens, index, mask),
                Some("&") => prefix(tokens, index, mask) & prefix(tokens, index, mask),
                Some("|") => prefix(tokens, index, mask) | prefix(tokens, index, mask),
                _ => mask & (1 << item[2].as_u64().unwrap()) != 0,
            }
        }
        fn eval(node: &Node, mask: u32) -> bool {
            match node {
                Node::And(children) => children.iter().all(|c| eval(c, mask)),
                Node::Or(children) => children.iter().any(|c| eval(c, mask)),
                Node::Not(child) => !eval(child, mask),
                Node::Leaf(leaf) => mask & (1 << leaf.value.as_u64().unwrap()) != 0,
                Node::True => true,
                Node::False => false,
            }
        }
        fn leaves(node: &Node, out: &mut Vec<u64>) {
            match node {
                Node::And(children) | Node::Or(children) => {
                    children.iter().for_each(|c| leaves(c, out))
                }
                Node::Not(child) => leaves(child, out),
                Node::Leaf(leaf) => out.push(leaf.value.as_u64().unwrap()),
                _ => {}
            }
        }
        let mut seed = 42;
        for case in 0..200 {
            let mut tokens = Vec::new();
            for _ in 0..1 + case % 4 {
                generate(&mut tokens, &mut seed, 6);
            }
            let parsed = parse(&json!(tokens)).unwrap();
            for mask in 0..32 {
                let mut index = 0;
                let mut expected = true;
                while index < tokens.len() {
                    expected &= prefix(&tokens, &mut index, mask);
                }
                assert_eq!(eval(&parsed, mask), expected, "case={case}, mask={mask}");
            }
            let expected: Vec<_> = tokens
                .iter()
                .filter_map(|t| t.as_array().map(|a| a[2].as_u64().unwrap()))
                .collect();
            let mut actual = Vec::new();
            leaves(&parsed, &mut actual);
            assert_eq!(actual, expected);
            tracing::debug!(
                case,
                terms = tokens.len(),
                "prefix interpreter and flattened AST agree for all 32 assignments"
            );
        }
    }

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
