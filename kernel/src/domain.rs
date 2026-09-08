use anyhow::{Result, bail};
use serde_json::Value as Json;

/// The structural depth a domain may reach, counted the way Odoo counts it
/// (`odoo/orm/domain/ast.py::MAX_DOMAIN_NESTING`): a run of the same n-ary
/// operator is ONE level, and a double negation is none. Anything deeper is
/// refused at parse time, before any recursive walk of the tree exists.
pub const MAX_DOMAIN_NESTING: usize = 100;

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

/// A parsed node with the structural depth it reached, so the cap is checked
/// as the tree is built and never by walking it afterwards.
struct Item {
    node: Node,
    depth: usize,
}

fn checked(depth: usize) -> Result<usize> {
    if depth > MAX_DOMAIN_NESTING {
        bail!(
            "domain nesting too deep (>{MAX_DOMAIN_NESTING} levels); refusing \
             to build it rather than recurse over it"
        );
    }
    Ok(depth)
}

/// Combine `operands` under `op`, folding an operand that is already the same
/// n-ary node into its parent. Odoo's `DomainNary` flattens the same way, so a
/// prefix chain of ten thousand `&` is one AND of ten thousand leaves, not a
/// ten-thousand-deep tree.
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
        bail!("domain must be a JSON array, got {domain}");
    };
    let mut stack: Vec<Item> = Vec::new();

    for item in items.iter().rev() {
        match item {
            Json::String(op) if op == "&" || op == "|" => {
                let a = stack.pop();
                let b = stack.pop();
                let (Some(a), Some(b)) = (a, b) else {
                    bail!("operator {op} missing operands");
                };
                stack.push(nary(op, vec![a, b])?);
            }
            Json::String(op) if op == "!" => {
                let Some(a) = stack.pop() else {
                    bail!("operator ! missing operand");
                };
                // `~~x` is `x`: Odoo's `DomainNot` collapses the pair, and
                // keeping it would let a flat run of `!` build a tree as deep
                // as the request body allows.
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
            other => bail!("invalid domain term {other}"),
        }
    }

    stack.reverse();
    Ok(match stack.len() {
        0 => Node::True,
        1 => stack.pop().unwrap().node,
        _ => nary("&", stack)?.node,
    })
}

fn parse_leaf(leaf: &[Json]) -> Result<Node> {
    let op = leaf[1]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("leaf operator must be a string"))?
        .to_string();

    if op == "any!" || op == "not any!" {
        bail!(
            "operator {op:?} is Odoo's internal spelling for a subquery that \
             skips the comodel's access rules; `Domain()` rejects it in an \
             incoming domain and so does this kernel"
        );
    }

    if let Some(n) = leaf[0].as_i64() {
        let v = leaf[2].as_i64().unwrap_or(-1);
        return Ok(match (n == v, op.as_str()) {
            (true, "=") | (false, "!=") => Node::True,
            _ => Node::False,
        });
    }
    let field = leaf[0]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("leaf field must be a string"))?
        .to_string();
    Ok(Node::Leaf(Leaf {
        field,
        op,
        value: leaf[2].clone(),
    }))
}

pub fn referenced_fields(node: &Node, out: &mut Vec<String>) {
    match node {
        Node::And(v) | Node::Or(v) => v.iter().for_each(|n| referenced_fields(n, out)),
        Node::Not(n) => referenced_fields(n, out),
        Node::Leaf(l) => out.push(l.field.split('.').next().unwrap_or(&l.field).to_string()),
        _ => {}
    }
}
