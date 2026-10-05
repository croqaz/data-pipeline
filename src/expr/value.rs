//! Runtime values of the expression language.

use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    Null,
    Num(f64),
    Bool(bool),
    Str(Cow<'a, str>),
    List(Vec<Value<'a>>),
}

impl<'a> Value<'a> {
    /// Only `true` is truthy. Numbers and strings are not booleans here.
    pub fn truthy(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Num(_) => "number",
            Value::Bool(_) => "bool",
            Value::Str(_) => "string",
            Value::List(_) => "list",
        }
    }

    pub fn str(s: &'a str) -> Self {
        Value::Str(Cow::Borrowed(s))
    }

    pub fn owned(s: String) -> Self {
        Value::Str(Cow::Owned(s))
    }

    /// Equality with the language's rules: same type compares, `null` or a
    /// type mismatch is `null`.
    pub fn equals(&self, other: &Value) -> Value<'static> {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => Value::Bool(a == b),
            (Value::Str(a), Value::Str(b)) => Value::Bool(a == b),
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(a == b),
            _ => Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_is_strict() {
        assert!(Value::Bool(true).truthy());
        assert!(!Value::Bool(false).truthy());
        assert!(!Value::Num(1.0).truthy());
        assert!(!Value::str("x").truthy());
        assert!(!Value::Null.truthy());
        assert!(Value::Null.is_null());
    }

    #[test]
    fn names_and_equality() {
        assert_eq!(Value::Null.type_name(), "null");
        assert_eq!(Value::Num(1.0).type_name(), "number");
        assert_eq!(Value::Bool(true).type_name(), "bool");
        assert_eq!(Value::str("a").type_name(), "string");
        assert_eq!(Value::List(vec![]).type_name(), "list");
        assert_eq!(Value::Num(1.0).equals(&Value::Num(1.0)), Value::Bool(true));
        assert_eq!(Value::str("a").equals(&Value::owned("a".into())), Value::Bool(true));
        assert_eq!(Value::Bool(true).equals(&Value::Bool(false)), Value::Bool(false));
        assert_eq!(Value::Num(1.0).equals(&Value::str("1")), Value::Null);
        assert_eq!(Value::Null.equals(&Value::Null), Value::Null);
    }
}
