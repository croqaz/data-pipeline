//! Tokens for the expression language.

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Num(f64),
    Str(String),
    Ident(String),
    /// One of the fixed operators and punctuation marks.
    Op(&'static str),
}

const OPS: &[&str] = &["&&", "||", "==", "!=", "<=", ">=", "!", "<", ">", "+", "-", "*", "/", "%", "(", ")", "[", "]", ",", "."];

fn err(msg: impl Into<String>) -> Error {
    Error::Expr(msg.into())
}

pub fn lex(src: &str) -> Result<Vec<Token>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                i += 1;
                if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                    i += 1;
                }
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let text = &src[start..i];
            let n: f64 = text.parse().map_err(|_| err(format!("bad number `{text}`")))?;
            out.push(Token::Num(n));
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Token::Ident(src[start..i].to_string()));
            continue;
        }
        if c == b'"' || c == b'\'' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            loop {
                let Some(&d) = b.get(i) else {
                    return Err(err("unterminated string literal"));
                };
                i += 1;
                if d == quote {
                    break;
                }
                if d == b'\\' {
                    let Some(&e) = b.get(i) else {
                        return Err(err("unterminated string literal"));
                    };
                    i += 1;
                    match e {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'\\' => s.push('\\'),
                        b'"' => s.push('"'),
                        b'\'' => s.push('\''),
                        other => return Err(err(format!("unknown escape `\\{}`", other as char))),
                    }
                    continue;
                }
                // Copy one UTF-8 character verbatim.
                let ch_len = utf8_len(d);
                let end = (i - 1 + ch_len).min(b.len());
                s.push_str(&src[i - 1..end]);
                i = end;
            }
            out.push(Token::Str(s));
            continue;
        }
        if let Some(op) = OPS.iter().find(|op| src[i..].starts_with(**op)) {
            out.push(Token::Op(op));
            i += op.len();
            continue;
        }
        let ch = src[i..].chars().next().unwrap_or('?');
        return Err(err(format!("unexpected character `{ch}` at byte {i}")));
    }
    Ok(out)
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(src: &str) -> Vec<Token> {
        lex(src).unwrap()
    }

    #[test]
    fn numbers_idents_and_ops() {
        assert_eq!(
            ops("words >= 50 && .5 + 1e3 - 2.5E-1"),
            vec![
                Token::Ident("words".into()),
                Token::Op(">="),
                Token::Num(50.0),
                Token::Op("&&"),
                Token::Num(0.5),
                Token::Op("+"),
                Token::Num(1000.0),
                Token::Op("-"),
                Token::Num(0.25),
            ]
        );
        assert_eq!(
            ops("a.b(c,[1])"),
            vec![
                Token::Ident("a".into()),
                Token::Op("."),
                Token::Ident("b".into()),
                Token::Op("("),
                Token::Ident("c".into()),
                Token::Op(","),
                Token::Op("["),
                Token::Num(1.0),
                Token::Op("]"),
                Token::Op(")"),
            ]
        );
        assert_eq!(
            ops("!a != b < c <= d > e % f * g / h"),
            vec![
                Token::Op("!"),
                Token::Ident("a".into()),
                Token::Op("!="),
                Token::Ident("b".into()),
                Token::Op("<"),
                Token::Ident("c".into()),
                Token::Op("<="),
                Token::Ident("d".into()),
                Token::Op(">"),
                Token::Ident("e".into()),
                Token::Op("%"),
                Token::Ident("f".into()),
                Token::Op("*"),
                Token::Ident("g".into()),
                Token::Op("/"),
                Token::Ident("h".into()),
            ]
        );
    }

    #[test]
    fn strings_with_escapes_and_unicode() {
        assert_eq!(
            ops(r#""a\"b" 'c\'d' "tab\there\n\\""#),
            vec![Token::Str("a\"b".into()), Token::Str("c'd".into()), Token::Str("tab\there\n\\".into()),]
        );
        assert_eq!(ops("\"café 日本 😀\""), vec![Token::Str("café 日本 😀".into())]);
    }

    #[test]
    fn errors() {
        assert!(lex("\"open").is_err());
        assert!(lex("\"bad\\q\"").is_err());
        assert!(lex("\"trail\\").is_err());
        assert!(lex("a $ b").is_err());
        assert!(lex("1.2.3").is_err());
        assert!(lex("é").is_err());
    }
}
