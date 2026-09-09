use anyhow::{Result, bail};
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
        bail!("domain must be a JSON array, got {domain}");
    };
    let mut stack: Vec<Node> = Vec::new();

    for item in items.iter().rev() {
        match item {
            Json::String(op) if op == "&" || op == "|" => {
                let a = stack.pop();
                let b = stack.pop();
                let (Some(a), Some(b)) = (a, b) else {
                    bail!("operator {op} missing operands");
                };
                stack.push(if op == "&" {
                    Node::And(vec![a, b])
                } else {
                    Node::Or(vec![a, b])
                });
            }
            Json::String(op) if op == "!" => {
                let Some(a) = stack.pop() else {
                    bail!("operator ! missing operand");
                };
                stack.push(Node::Not(Box::new(a)));
            }
            Json::Array(leaf) if leaf.len() == 3 => {
                stack.push(parse_leaf(leaf)?);
            }
            other => bail!("invalid domain term {other}"),
        }
    }

    stack.reverse();
    Ok(match stack.len() {
        0 => Node::True,
        1 => stack.pop().unwrap(),
        _ => Node::And(stack),
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
