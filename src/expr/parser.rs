//! Recursive-descent parser producing a name-based AST.
//!
//! ```text
//! expr    := or
//! or      := and ( "||" and )*
//! and     := not ( "&&" not )*
//! not     := "!" not | cmp
//! cmp     := sum ( ("==" | "!=" | "<" | "<=" | ">" | ">=" | "in") sum )?
//! sum     := prod ( ("+" | "-") prod )*
//! prod    := unary ( ("*" | "/" | "%") unary )*
//! unary   := "-" unary | postfix
//! postfix := atom ( "." ident [ "(" args ")" ] )*
//! atom    := number | string | "true" | "false" | "null"
//!          | ident | ident "(" args ")" | "(" expr ")" | "[" args "]"
//! ```

use crate::error::{Error, Result};

use super::lexer::{Token, lex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ast {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Ident(String),
    /// `receiver.name`
    Field(Box<Ast>, String),
    /// `name(args)`
    Call(String, Vec<Ast>),
    /// `receiver.name(args)`
    Method(Box<Ast>, String, Vec<Ast>),
    List(Vec<Ast>),
    Neg(Box<Ast>),
    Not(Box<Ast>),
    Bin(BinOp, Box<Ast>, Box<Ast>),
    In(Box<Ast>, Box<Ast>),
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

fn err(msg: impl Into<String>) -> Error {
    Error::Expr(msg.into())
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn peek_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Token::Op(o)) if *o == op)
    }

    fn peek_ident(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(i)) if i == name)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expect_op(&mut self, op: &str) -> Result<()> {
        if self.peek_op(op) {
            self.pos += 1;
            Ok(())
        } else {
            Err(err(format!("expected `{op}` but found {}", self.describe())))
        }
    }

    fn describe(&self) -> String {
        match self.peek() {
            None => "end of expression".to_string(),
            Some(Token::Num(n)) => format!("number {n}"),
            Some(Token::Str(s)) => format!("string {s:?}"),
            Some(Token::Ident(i)) => format!("`{i}`"),
            Some(Token::Op(o)) => format!("`{o}`"),
        }
    }

    fn or(&mut self) -> Result<Ast> {
        let mut left = self.and()?;
        while self.peek_op("||") {
            self.pos += 1;
            let right = self.and()?;
            left = Ast::Bin(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Ast> {
        let mut left = self.not()?;
        while self.peek_op("&&") {
            self.pos += 1;
            let right = self.not()?;
            left = Ast::Bin(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Ast> {
        if self.peek_op("!") {
            self.pos += 1;
            return Ok(Ast::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Ast> {
        let left = self.sum()?;
        let op = match self.peek() {
            Some(Token::Op("==")) => Some(BinOp::Eq),
            Some(Token::Op("!=")) => Some(BinOp::Ne),
            Some(Token::Op("<")) => Some(BinOp::Lt),
            Some(Token::Op("<=")) => Some(BinOp::Le),
            Some(Token::Op(">")) => Some(BinOp::Gt),
            Some(Token::Op(">=")) => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.pos += 1;
            let right = self.sum()?;
            return Ok(Ast::Bin(op, Box::new(left), Box::new(right)));
        }
        if self.peek_ident("in") {
            self.pos += 1;
            let right = self.sum()?;
            return Ok(Ast::In(Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn sum(&mut self) -> Result<Ast> {
        let mut left = self.prod()?;
        loop {
            let op = if self.peek_op("+") {
                BinOp::Add
            } else if self.peek_op("-") {
                BinOp::Sub
            } else {
                break;
            };
            self.pos += 1;
            let right = self.prod()?;
            left = Ast::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn prod(&mut self) -> Result<Ast> {
        let mut left = self.unary()?;
        loop {
            let op = if self.peek_op("*") {
                BinOp::Mul
            } else if self.peek_op("/") {
                BinOp::Div
            } else if self.peek_op("%") {
                BinOp::Mod
            } else {
                break;
            };
            self.pos += 1;
            let right = self.unary()?;
            left = Ast::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Ast> {
        if self.peek_op("-") {
            self.pos += 1;
            return Ok(Ast::Neg(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Ast> {
        let mut node = self.atom()?;
        while self.peek_op(".") {
            self.pos += 1;
            let name = match self.bump() {
                Some(Token::Ident(name)) => name,
                _ => return Err(err("expected a name after `.`")),
            };
            if self.peek_op("(") {
                self.pos += 1;
                let args = self.args(")")?;
                node = Ast::Method(Box::new(node), name, args);
            } else {
                node = Ast::Field(Box::new(node), name);
            }
        }
        Ok(node)
    }

    /// Parse a comma-separated list up to and including `close`.
    fn args(&mut self, close: &str) -> Result<Vec<Ast>> {
        let mut out = Vec::new();
        if self.peek_op(close) {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            out.push(self.or()?);
            if self.peek_op(",") {
                self.pos += 1;
                continue;
            }
            self.expect_op(close)?;
            return Ok(out);
        }
    }

    fn atom(&mut self) -> Result<Ast> {
        match self.bump() {
            Some(Token::Num(n)) => Ok(Ast::Num(n)),
            Some(Token::Str(s)) => Ok(Ast::Str(s)),
            Some(Token::Ident(name)) => match name.as_str() {
                "true" => Ok(Ast::Bool(true)),
                "false" => Ok(Ast::Bool(false)),
                "null" => Ok(Ast::Null),
                "in" => Err(err("`in` is an operator, not a value")),
                _ => {
                    if self.peek_op("(") {
                        self.pos += 1;
                        let args = self.args(")")?;
                        Ok(Ast::Call(name, args))
                    } else {
                        Ok(Ast::Ident(name))
                    }
                }
            },
            Some(Token::Op("(")) => {
                let inner = self.or()?;
                self.expect_op(")")?;
                Ok(inner)
            }
            Some(Token::Op("[")) => Ok(Ast::List(self.args("]")?)),
            Some(Token::Op(op)) => Err(err(format!("unexpected `{op}`"))),
            None => Err(err("unexpected end of expression")),
        }
    }
}

pub fn parse(src: &str) -> Result<Ast> {
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err(err("empty expression"));
    }
    let mut p = Parser { toks, pos: 0 };
    let ast = p.or()?;
    if p.pos < p.toks.len() {
        return Err(err(format!("unexpected {} after the end of the expression", p.describe())));
    }
    Ok(ast)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(a: Ast) -> Box<Ast> {
        Box::new(a)
    }

    fn id(s: &str) -> Ast {
        Ast::Ident(s.into())
    }

    #[test]
    fn precedence_and_associativity() {
        // a || b && !c == d + e * -f
        let ast = parse("a || b && !c == d + e * -f").unwrap();
        let expected = Ast::Bin(
            BinOp::Or,
            b(id("a")),
            b(Ast::Bin(
                BinOp::And,
                b(id("b")),
                b(Ast::Not(b(Ast::Bin(
                    BinOp::Eq,
                    b(id("c")),
                    b(Ast::Bin(BinOp::Add, b(id("d")), b(Ast::Bin(BinOp::Mul, b(id("e")), b(Ast::Neg(b(id("f")))))))),
                )))),
            )),
        );
        assert_eq!(ast, expected);
        // left associative sums and products
        assert_eq!(
            parse("1 - 2 - 3").unwrap(),
            Ast::Bin(BinOp::Sub, b(Ast::Bin(BinOp::Sub, b(Ast::Num(1.0)), b(Ast::Num(2.0)))), b(Ast::Num(3.0)))
        );
        assert_eq!(
            parse("8 / 2 % 3").unwrap(),
            Ast::Bin(BinOp::Mod, b(Ast::Bin(BinOp::Div, b(Ast::Num(8.0)), b(Ast::Num(2.0)))), b(Ast::Num(3.0)))
        );
        assert_eq!(parse("(1 + 2) * 3").unwrap(), Ast::Bin(BinOp::Mul, b(Ast::Bin(BinOp::Add, b(Ast::Num(1.0)), b(Ast::Num(2.0)))), b(Ast::Num(3.0))));
    }

    #[test]
    fn comparisons_in_lists_calls_methods() {
        for (src, op) in [("a != b", BinOp::Ne), ("a < b", BinOp::Lt), ("a <= b", BinOp::Le), ("a > b", BinOp::Gt), ("a >= b", BinOp::Ge)] {
            assert_eq!(parse(src).unwrap(), Ast::Bin(op, b(id("a")), b(id("b"))));
        }
        assert_eq!(
            parse("source in [\"a\", 'b']").unwrap(),
            Ast::In(b(id("source")), b(Ast::List(vec![Ast::Str("a".into()), Ast::Str("b".into())])))
        );
        assert_eq!(parse("has(dictHit)").unwrap(), Ast::Call("has".into(), vec![id("dictHit")]));
        assert_eq!(parse("f()").unwrap(), Ast::Call("f".into(), vec![]));
        assert_eq!(
            parse("extra.title.matches(\"x\")").unwrap(),
            Ast::Method(b(Ast::Field(b(id("extra")), "title".into())), "matches".into(), vec![Ast::Str("x".into())])
        );
        assert_eq!(parse("text.lower()").unwrap(), Ast::Method(b(id("text")), "lower".into(), vec![]));
        assert_eq!(parse("[]").unwrap(), Ast::List(vec![]));
        assert_eq!(parse("true && false || null").unwrap(), Ast::Bin(BinOp::Or, b(Ast::Bin(BinOp::And, b(Ast::Bool(true)), b(Ast::Bool(false)))), b(Ast::Null)));
        assert_eq!(parse("!!a").unwrap(), Ast::Not(b(Ast::Not(b(id("a"))))));
        assert_eq!(parse("--1").unwrap(), Ast::Neg(b(Ast::Neg(b(Ast::Num(1.0))))));
    }

    #[test]
    fn parse_errors_name_the_problem() {
        let cases = [
            ("", "empty expression"),
            ("a b", "unexpected `b` after the end"),
            ("(a", "expected `)`"),
            ("a.", "expected a name after `.`"),
            ("a.(", "expected a name after `.`"),
            ("f(a", "expected `)`"),
            ("[1, 2", "expected `]`"),
            ("in", "`in` is an operator"),
            ("a +", "unexpected end of expression"),
            (")", "unexpected `)`"),
            ("1 2", "unexpected number 2"),
            ("a \"s\"", "unexpected string \"s\""),
            ("a ==", "unexpected end"),
            ("a )", "unexpected `)` after the end"),
        ];
        for (src, want) in cases {
            let msg = parse(src).unwrap_err().to_string();
            assert!(msg.contains(want), "{src:?}: {msg}");
        }
    }
}
