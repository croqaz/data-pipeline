//! Evaluation of a compiled expression against one record.

use sonic_rs::{JsonType, JsonValueTrait};

use crate::hash::{Key, key_to_hex};
use crate::meta::{FEATURES, Meta};

use super::compile::{Compiled, Func, Lit, Method, Node, Slot};
use super::parser::BinOp;
use super::value::Value;

/// What an expression can read from a record.
pub trait Row {
    fn feature(&self, idx: usize) -> Option<f64>;
    fn source(&self) -> &str;
    fn id_hex(&self) -> String;
    fn text(&self) -> Option<&str>;
    fn extra(&self, key: &str) -> Value<'_>;
}

/// A stored record as the expression language sees it. `text` and `extra`
/// are only fetched by the caller when the expression needs them.
pub struct RecordRow<'a> {
    pub meta: &'a Meta,
    pub key: &'a Key,
    pub source: &'a str,
    pub text: Option<&'a str>,
    pub extra: Option<&'a [u8]>,
}

impl Row for RecordRow<'_> {
    fn feature(&self, idx: usize) -> Option<f64> {
        (FEATURES[idx].get)(self.meta)
    }

    fn source(&self) -> &str {
        self.source
    }

    fn id_hex(&self) -> String {
        key_to_hex(self.key)
    }

    fn text(&self) -> Option<&str> {
        self.text
    }

    fn extra(&self, key: &str) -> Value<'_> {
        let Some(bytes) = self.extra else {
            return Value::Null;
        };
        match sonic_rs::get(bytes, &[key]) {
            Ok(v) => lazy_to_value(&v),
            Err(_) => Value::Null,
        }
    }
}

fn lazy_to_value(v: &sonic_rs::LazyValue<'_>) -> Value<'static> {
    match v.get_type() {
        JsonType::Null => Value::Null,
        JsonType::Boolean => v.as_bool().map_or(Value::Null, Value::Bool),
        JsonType::Number => v.as_f64().map_or(Value::Null, Value::Num),
        JsonType::String => match sonic_rs::from_str::<String>(v.as_raw_str()) {
            Ok(s) => Value::owned(s),
            Err(_) => Value::Null,
        },
        JsonType::Array | JsonType::Object => Value::owned(v.as_raw_str().to_string()),
    }
}

impl Compiled {
    /// True when the expression evaluates to `true` for this row.
    pub fn matches(&self, row: &dyn Row) -> bool {
        eval(&self.root, row).truthy()
    }

    pub fn eval<'a>(&'a self, row: &'a dyn Row) -> Value<'a> {
        eval(&self.root, row)
    }
}

fn borrow_lit(v: &Lit) -> Value<'_> {
    match v {
        Lit::Str(s) => Value::str(s),
        Lit::Num(n) => Value::Num(*n),
        Lit::Bool(b) => Value::Bool(*b),
        Lit::Null => Value::Null,
    }
}

fn var<'a>(slot: &'a Slot, row: &'a dyn Row) -> Value<'a> {
    match slot {
        Slot::Feature(i) => row.feature(*i).map_or(Value::Null, Value::Num),
        Slot::Source => Value::str(row.source()),
        Slot::Id => Value::owned(row.id_hex()),
        Slot::Text => row.text().map_or(Value::Null, Value::str),
        Slot::Extra(key) => row.extra(key),
    }
}

fn eval<'a>(node: &'a Node, row: &'a dyn Row) -> Value<'a> {
    match node {
        Node::Lit(v) => borrow_lit(v),
        Node::Var(slot) => var(slot, row),
        Node::Has(slot) => Value::Bool(!var(slot, row).is_null()),
        Node::Neg(x) => match eval(x, row) {
            Value::Num(n) => Value::Num(-n),
            _ => Value::Null,
        },
        Node::Not(x) => match eval(x, row) {
            Value::Bool(b) => Value::Bool(!b),
            _ => Value::Null,
        },
        Node::Bin(BinOp::Or, l, r) => Value::Bool(eval(l, row).truthy() || eval(r, row).truthy()),
        Node::Bin(BinOp::And, l, r) => Value::Bool(eval(l, row).truthy() && eval(r, row).truthy()),
        Node::Bin(op, l, r) => binary(*op, eval(l, row), eval(r, row)),
        Node::In(l, items) => {
            let lv = eval(l, row);
            if lv.is_null() {
                return Value::Null;
            }
            Value::Bool(items.iter().any(|it| lv.equals(&eval(it, row)).truthy()))
        }
        Node::Call(f, args) => call(*f, args.iter().map(|a| eval(a, row)).collect()),
        Node::Method(m, recv, args) => method(*m, eval(recv, row), args.iter().map(|a| eval(a, row)).collect()),
        Node::Matches(recv, re) => match eval(recv, row) {
            Value::Str(s) => Value::Bool(re.is_match(&s)),
            _ => Value::Null,
        },
        Node::List(items) => Value::List(items.iter().map(|i| eval(i, row)).collect()),
    }
}

fn binary<'a>(op: BinOp, l: Value<'a>, r: Value<'a>) -> Value<'a> {
    use Value::*;
    match (op, &l, &r) {
        (BinOp::Eq, _, _) => l.equals(&r),
        (BinOp::Ne, _, _) => match l.equals(&r) {
            Bool(b) => Bool(!b),
            _ => Null,
        },
        (BinOp::Lt, Num(a), Num(b)) => Bool(a < b),
        (BinOp::Le, Num(a), Num(b)) => Bool(a <= b),
        (BinOp::Gt, Num(a), Num(b)) => Bool(a > b),
        (BinOp::Ge, Num(a), Num(b)) => Bool(a >= b),
        (BinOp::Lt, Str(a), Str(b)) => Bool(a < b),
        (BinOp::Le, Str(a), Str(b)) => Bool(a <= b),
        (BinOp::Gt, Str(a), Str(b)) => Bool(a > b),
        (BinOp::Ge, Str(a), Str(b)) => Bool(a >= b),
        (BinOp::Add, Num(a), Num(b)) => Num(a + b),
        (BinOp::Sub, Num(a), Num(b)) => Num(a - b),
        (BinOp::Mul, Num(a), Num(b)) => Num(a * b),
        (BinOp::Div, Num(a), Num(b)) => {
            if *b == 0.0 {
                Null
            } else {
                Num(a / b)
            }
        }
        (BinOp::Mod, Num(a), Num(b)) => {
            if *b == 0.0 {
                Null
            } else {
                Num(a % b)
            }
        }
        _ => Null,
    }
}

fn call<'a>(f: Func, mut args: Vec<Value<'a>>) -> Value<'a> {
    use Value::*;
    match f {
        Func::Abs => match args.pop() {
            Some(Num(n)) => Num(n.abs()),
            _ => Null,
        },
        Func::Min | Func::Max => match (&args[0], &args[1]) {
            (Num(a), Num(b)) => Num(if f == Func::Min { a.min(*b) } else { a.max(*b) }),
            _ => Null,
        },
        Func::Len => match args.pop() {
            Some(Str(s)) => Num(s.chars().count() as f64),
            Some(List(items)) => Num(items.len() as f64),
            _ => Null,
        },
        Func::Num => match args.pop() {
            Some(Num(n)) => Num(n),
            Some(Bool(b)) => Num(if b { 1.0 } else { 0.0 }),
            Some(Str(s)) => s.trim().parse::<f64>().map_or(Null, Num),
            _ => Null,
        },
        Func::Lower => match args.pop() {
            Some(Str(s)) => Value::owned(s.to_lowercase()),
            _ => Null,
        },
    }
}

fn method<'a>(m: Method, recv: Value<'a>, mut args: Vec<Value<'a>>) -> Value<'a> {
    use Value::*;
    let Str(s) = recv else {
        return Null;
    };
    match m {
        Method::Lower => Value::owned(s.to_lowercase()),
        Method::Contains | Method::StartsWith | Method::EndsWith => match args.pop() {
            Some(Str(arg)) => Bool(match m {
                Method::Contains => s.contains(arg.as_ref()),
                Method::StartsWith => s.starts_with(arg.as_ref()),
                _ => s.ends_with(arg.as_ref()),
            }),
            _ => Null,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::compile;
    use crate::meta::{SCHEMA_VERSION, feature_index};

    struct Fake {
        text: Option<&'static str>,
    }

    impl Row for Fake {
        fn feature(&self, idx: usize) -> Option<f64> {
            match FEATURES[idx].name {
                "words" => Some(100.0),
                "uniqWords" => Some(25.0),
                "vowel" => Some(97.5),
                "dictHit" => None,
                _ => Some(1.0),
            }
        }
        fn source(&self) -> &str {
            "gutenberg"
        }
        fn id_hex(&self) -> String {
            "ff".repeat(16)
        }
        fn text(&self) -> Option<&str> {
            self.text
        }
        fn extra(&self, key: &str) -> Value<'_> {
            match key {
                "year" => Value::Num(1842.0),
                "title" => Value::str("The Raven"),
                "ok" => Value::Bool(true),
                _ => Value::Null,
            }
        }
    }

    fn to_owned_value(v: Value<'_>) -> Value<'static> {
        match v {
            Value::Null => Value::Null,
            Value::Num(n) => Value::Num(n),
            Value::Bool(b) => Value::Bool(b),
            Value::Str(s) => Value::owned(s.into_owned()),
            Value::List(items) => Value::List(items.into_iter().map(to_owned_value).collect()),
        }
    }

    fn ev(src: &str) -> Value<'static> {
        let row = Fake { text: Some("Lorem ipsum dolor") };
        let c = compile(src).unwrap();
        to_owned_value(c.eval(&row))
    }

    fn m(src: &str) -> bool {
        let row = Fake { text: Some("Lorem ipsum dolor") };
        compile(src).unwrap().matches(&row)
    }

    #[test]
    fn arithmetic_and_comparison() {
        assert_eq!(ev("words + 1"), Value::Num(101.0));
        assert_eq!(ev("words - 1"), Value::Num(99.0));
        assert_eq!(ev("words * 2"), Value::Num(200.0));
        assert_eq!(ev("words / 4"), Value::Num(25.0));
        assert_eq!(ev("words % 3"), Value::Num(1.0));
        assert_eq!(ev("words / 0"), Value::Null);
        assert_eq!(ev("words % 0"), Value::Null);
        assert_eq!(ev("-words"), Value::Num(-100.0));
        assert_eq!(ev("-source"), Value::Null);
        assert_eq!(ev("uniqWords / words"), Value::Num(0.25));
        assert_eq!(ev("len + bytes"), Value::Num(2.0));
        assert!(m("words == 100") && m("words != 99") && m("words < 101") && m("words <= 100") && m("words > 99") && m("words >= 100"));
        assert!(!m("words < 100") && !m("words > 100"));
        assert!(m("source < 'h'") && m("source <= 'gutenberg'") && m("source > 'a'") && m("source >= 'gutenberg'"));
        assert!(m("vowel > 97 && vowel < 98"));
        assert_eq!(ev("words + source"), Value::Null);
        assert_eq!(ev("words < source"), Value::Null);
    }

    #[test]
    fn null_semantics() {
        assert_eq!(ev("dictHit"), Value::Null);
        assert_eq!(ev("dictHit < 50"), Value::Null);
        assert_eq!(ev("dictHit == null"), Value::Null);
        assert_eq!(ev("dictHit != 1"), Value::Null);
        assert!(!m("dictHit < 50"));
        assert!(!m("has(dictHit)") && m("has(words)") && m("has(extra.year)") && !m("has(extra.missing)"));
        assert!(m("!has(dictHit)"));
        assert_eq!(ev("!dictHit"), Value::Null);
        assert_eq!(ev("!words"), Value::Null);
        assert!(m("dictHit < 50 || words > 1"));
        assert!(!m("dictHit < 50 && words > 1"));
        assert!(!m("null") && !m("null || null") && m("true || null"));
        assert_eq!(ev("dictHit in [1, 2]"), Value::Null);
    }

    #[test]
    fn strings_lists_and_functions() {
        assert!(m("source == \"gutenberg\"") && m("source in ['EEBO', 'gutenberg']") && !m("source in ['EEBO']") && !m("words in ['x']"));
        assert!(m("text.contains('ipsum')") && m("text.startsWith('Lorem')") && m("text.endsWith('dolor')") && !m("text.contains('zzz')"));
        assert!(m("text.matches('^Lorem.*dolor$')") && !m("text.matches('(?i)^IPSUM')"));
        assert_eq!(ev("text.lower()"), Value::owned("lorem ipsum dolor".into()));
        assert_eq!(ev("lower(text)"), Value::owned("lorem ipsum dolor".into()));
        assert_eq!(ev("words.lower()"), Value::Null);
        assert_eq!(ev("words.contains('x')"), Value::Null);
        assert_eq!(ev("text.contains(1)"), Value::Null);
        assert_eq!(ev("words.matches('x')"), Value::Null);
        assert_eq!(ev("abs(-3)"), Value::Num(3.0));
        assert_eq!(ev("abs('x')"), Value::Null);
        assert_eq!(ev("min(words, 5)"), Value::Num(5.0));
        assert_eq!(ev("max(words, 5)"), Value::Num(100.0));
        assert_eq!(ev("max(words, 'x')"), Value::Null);
        assert_eq!(ev("len(source)"), Value::Num(9.0));
        assert_eq!(ev("len([1, 2, 3])"), Value::Num(3.0));
        assert_eq!(ev("len(words)"), Value::Null);
        assert_eq!(ev("num('42 ')"), Value::Num(42.0));
        assert_eq!(ev("num('x')"), Value::Null);
        assert_eq!(ev("num(words)"), Value::Num(100.0));
        assert_eq!(ev("num(true)"), Value::Num(1.0));
        assert_eq!(ev("num(false)"), Value::Num(0.0));
        assert_eq!(ev("num(dictHit)"), Value::Null);
        assert_eq!(ev("lower(1)"), Value::Null);
        assert_eq!(ev("id"), Value::owned("ff".repeat(16)));
        assert!(m("id.startsWith('ffff')"));
        assert!(m("extra.year < 1850 && extra.title == 'The Raven' && extra.ok == true && extra.ok != false"));
        assert!(!m("extra.missing == 1"));
        assert_eq!(ev("[1, 'a', true]"), Value::List(vec![Value::Num(1.0), Value::owned("a".into()), Value::Bool(true)]));
    }

    #[test]
    fn text_may_be_absent() {
        let row = Fake { text: None };
        assert!(!compile("text.contains('a')").unwrap().matches(&row));
        assert!(!compile("has(text)").unwrap().matches(&row));
    }

    #[test]
    fn record_row_reads_meta_and_extra_json() {
        let meta = Meta {
            schema: SCHEMA_VERSION,
            source: 1,
            len: 500,
            bytes: 510,
            words: 80,
            uniq_words: 60,
            lines: None,
            sentences: 5,
            uniq_chars: 30,
            vowel: 9900,
            alpha: 9500,
            flags: 1,
            dict_hit: Some(8000),
            entropy: Some(9660),
            dup5gram: None,
            mean_word_len: None,
            stop_words: None,
            quality: Some(9_950),
        };
        let key = [0xabu8; 16];
        let extra = br#"{"year": 1842, "title": "The \"Raven\"", "ok": true, "none": null, "tags": ["a", 1], "obj": {"k": 2}}"#;
        let row = RecordRow { meta: &meta, key: &key, source: "src", text: Some("hello world"), extra: Some(extra) };
        assert_eq!(row.feature(feature_index("words").unwrap()), Some(80.0));
        assert_eq!(row.feature(feature_index("vowel").unwrap()), Some(99.0));
        assert_eq!(row.feature(feature_index("dictHit").unwrap()), Some(80.0));
        assert_eq!(row.source(), "src");
        assert_eq!(row.id_hex(), "ab".repeat(16));
        assert_eq!(row.text(), Some("hello world"));
        assert_eq!(row.extra("year"), Value::Num(1842.0));
        assert_eq!(row.extra("title"), Value::owned("The \"Raven\"".into()));
        assert_eq!(row.extra("ok"), Value::Bool(true));
        assert_eq!(row.extra("none"), Value::Null);
        assert_eq!(row.extra("missing"), Value::Null);
        assert_eq!(row.extra("tags"), Value::owned("[\"a\", 1]".into()));
        assert_eq!(row.extra("obj"), Value::owned("{\"k\": 2}".into()));
        let c = compile("extra.year < 1850 && words == 80 && text.contains('world') && id.startsWith('abab')").unwrap();
        assert!(c.matches(&row));
        let no_extra = RecordRow { meta: &meta, key: &key, source: "src", text: None, extra: None };
        assert_eq!(no_extra.extra("year"), Value::Null);
        for bad in [b"{not json".as_slice(), b"[1, 2]", b"\x00\xff", b"", b"{\"year\": }"] {
            let bad_extra = RecordRow { meta: &meta, key: &key, source: "src", text: None, extra: Some(bad) };
            assert_eq!(bad_extra.extra("year"), Value::Null, "{bad:?}");
        }
    }
}
