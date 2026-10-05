//! Name resolution: turns the parser's AST into a tree of resolved slots so
//! evaluation never looks a name up by string, and records which keyspaces
//! the expression needs.

use regex::Regex;

use crate::error::{Error, Result};
use crate::meta::{feature_index, feature_names};

use super::parser::{Ast, BinOp, parse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    Feature(usize),
    Source,
    Id,
    Text,
    Extra(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Abs,
    Min,
    Max,
    Len,
    Num,
    Lower,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Contains,
    StartsWith,
    EndsWith,
    Lower,
}

/// A literal value in the tree (lists are `Node::List`).
#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Null,
    Num(f64),
    Bool(bool),
    Str(String),
}

#[derive(Debug)]
pub enum Node {
    Lit(Lit),
    Var(Slot),
    Has(Slot),
    Neg(Box<Node>),
    Not(Box<Node>),
    Bin(BinOp, Box<Node>, Box<Node>),
    In(Box<Node>, Vec<Node>),
    Call(Func, Vec<Node>),
    Method(Method, Box<Node>, Vec<Node>),
    Matches(Box<Node>, Regex),
    List(Vec<Node>),
}

/// A compiled expression plus what it needs from the store.
#[derive(Debug)]
pub struct Compiled {
    pub(crate) root: Node,
    pub needs_text: bool,
    pub needs_extra: bool,
    pub needs_id: bool,
    pub source: String,
}

fn err(msg: impl Into<String>) -> Error {
    Error::Expr(msg.into())
}

#[derive(Default)]
struct Needs {
    text: bool,
    extra: bool,
    id: bool,
}

fn resolve_slot(ast: &Ast, needs: &mut Needs) -> Result<Slot> {
    match ast {
        Ast::Ident(name) => {
            if let Some(i) = feature_index(name) {
                return Ok(Slot::Feature(i));
            }
            match name.as_str() {
                "source" => Ok(Slot::Source),
                "id" => {
                    needs.id = true;
                    Ok(Slot::Id)
                }
                "text" => {
                    needs.text = true;
                    Ok(Slot::Text)
                }
                "extra" => Err(err("`extra` must be followed by a key, like `extra.year`")),
                _ => Err(err(format!("unknown name `{name}`; valid names: {}, source, id, text, extra.<key>", feature_names().join(", ")))),
            }
        }
        Ast::Field(recv, key) => match recv.as_ref() {
            Ast::Ident(r) if r == "extra" => {
                needs.extra = true;
                Ok(Slot::Extra(key.clone()))
            }
            _ => Err(err(format!("`.{key}` is not a field; only `extra.<key>` and string methods are supported"))),
        },
        _ => Err(err("expected a name")),
    }
}

fn arity(name: &str, args: &[Ast], n: usize) -> Result<()> {
    if args.len() == n {
        Ok(())
    } else {
        Err(err(format!("`{name}` takes {n} argument{} but got {}", if n == 1 { "" } else { "s" }, args.len())))
    }
}

fn lower(ast: &Ast, needs: &mut Needs) -> Result<Node> {
    Ok(match ast {
        Ast::Num(n) => Node::Lit(Lit::Num(*n)),
        Ast::Str(s) => Node::Lit(Lit::Str(s.clone())),
        Ast::Bool(b) => Node::Lit(Lit::Bool(*b)),
        Ast::Null => Node::Lit(Lit::Null),
        Ast::Ident(_) | Ast::Field(_, _) => Node::Var(resolve_slot(ast, needs)?),
        Ast::Call(name, args) => match name.as_str() {
            "has" => {
                arity(name, args, 1)?;
                Node::Has(resolve_slot(&args[0], needs)?)
            }
            "abs" | "len" | "num" | "lower" => {
                arity(name, args, 1)?;
                let f = match name.as_str() {
                    "abs" => Func::Abs,
                    "len" => Func::Len,
                    "num" => Func::Num,
                    _ => Func::Lower,
                };
                Node::Call(f, vec![lower(&args[0], needs)?])
            }
            "min" | "max" => {
                arity(name, args, 2)?;
                let f = if name == "min" { Func::Min } else { Func::Max };
                Node::Call(f, vec![lower(&args[0], needs)?, lower(&args[1], needs)?])
            }
            _ => return Err(err(format!("unknown function `{name}`; functions: abs, min, max, len, num, lower, has"))),
        },
        Ast::Method(recv, name, args) => {
            let recv = lower(recv, needs)?;
            match name.as_str() {
                "matches" => {
                    arity(name, args, 1)?;
                    let Ast::Str(pat) = &args[0] else {
                        return Err(err("`matches` needs a string literal pattern"));
                    };
                    let re = Regex::new(pat).map_err(|e| err(format!("bad regex: {e}")))?;
                    Node::Matches(Box::new(recv), re)
                }
                "contains" | "startsWith" | "endsWith" => {
                    arity(name, args, 1)?;
                    let m = match name.as_str() {
                        "contains" => Method::Contains,
                        "startsWith" => Method::StartsWith,
                        _ => Method::EndsWith,
                    };
                    Node::Method(m, Box::new(recv), vec![lower(&args[0], needs)?])
                }
                "lower" => {
                    arity(name, args, 0)?;
                    Node::Method(Method::Lower, Box::new(recv), vec![])
                }
                _ => return Err(err(format!("unknown method `.{name}()`; methods: contains, startsWith, endsWith, matches, lower"))),
            }
        }
        Ast::List(items) => Node::List(items.iter().map(|i| lower(i, needs)).collect::<Result<_>>()?),
        Ast::Neg(x) => Node::Neg(Box::new(lower(x, needs)?)),
        Ast::Not(x) => Node::Not(Box::new(lower(x, needs)?)),
        Ast::Bin(op, l, r) => Node::Bin(*op, Box::new(lower(l, needs)?), Box::new(lower(r, needs)?)),
        Ast::In(l, r) => {
            let Ast::List(items) = r.as_ref() else {
                return Err(err("the right side of `in` must be a list literal like [1, 2]"));
            };
            Node::In(Box::new(lower(l, needs)?), items.iter().map(|i| lower(i, needs)).collect::<Result<_>>()?)
        }
    })
}

pub fn compile(src: &str) -> Result<Compiled> {
    let ast = parse(src)?;
    let mut needs = Needs::default();
    let root = lower(&ast, &mut needs)?;
    Ok(Compiled { root, needs_text: needs.text, needs_extra: needs.extra, needs_id: needs.id, source: src.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_names_and_records_needs() {
        let c = compile("words > 1 && source == 'x'").unwrap();
        assert!(!c.needs_text && !c.needs_extra && !c.needs_id);
        assert_eq!(c.source, "words > 1 && source == 'x'");
        let c = compile("text.contains('a') || extra.year < 1850 || id == 'ff'").unwrap();
        assert!(c.needs_text && c.needs_extra && c.needs_id);
        assert!(matches!(compile("has(dictHit)").unwrap().root, Node::Has(Slot::Feature(_))));
        assert!(matches!(compile("has(extra.k)").unwrap().root, Node::Has(Slot::Extra(_))));
        assert!(matches!(compile("source").unwrap().root, Node::Var(Slot::Source)));
        assert!(matches!(compile("-1").unwrap().root, Node::Neg(_)));
        assert!(matches!(compile("!true").unwrap().root, Node::Not(_)));
        assert!(matches!(compile("[1]").unwrap().root, Node::List(_)));
        assert!(matches!(compile("null").unwrap().root, Node::Lit(Lit::Null)));
        assert_eq!(Lit::Num(1.0).clone(), Lit::Num(1.0));
        assert!(compile("x in [1]").is_err());
    }

    #[test]
    fn rejects_bad_names_and_arity() {
        let cases = [
            ("nope > 1", "unknown name `nope`"),
            ("extra > 1", "`extra` must be followed"),
            ("source.x > 1", "`.x` is not a field"),
            ("has(1)", "expected a name"),
            ("has(a, b)", "`has` takes 1 argument"),
            ("abs()", "`abs` takes 1 argument"),
            ("min(1)", "`min` takes 2 arguments"),
            ("foo(1)", "unknown function `foo`"),
            ("text.matches(words)", "needs a string literal"),
            ("text.matches('(')", "bad regex"),
            ("text.matches()", "`matches` takes 1 argument"),
            ("text.contains()", "`contains` takes 1 argument"),
            ("text.lower(1)", "`lower` takes 0 arguments"),
            ("text.frob()", "unknown method `.frob()`"),
            ("words in words", "right side of `in`"),
            ("words in [nope]", "unknown name `nope`"),
            ("[nope]", "unknown name `nope`"),
            ("max(nope, 1)", "unknown name `nope`"),
        ];
        for (src, want) in cases {
            let msg = compile(src).unwrap_err().to_string();
            assert!(msg.contains(want), "{src:?}: {msg}");
        }
    }
}
