//! Header templates: text with `{expression}` placeholders, rendered per
//! document and prepended to the exported text.
//!
//! Rules, so a template works for every document of a mixed corpus:
//!
//! - a placeholder is any expression the filter language accepts, so
//!   `{extra.title}`, `{source}`, `{words}` and `{lower(extra.author)}` all
//!   work;
//! - a line whose placeholders all evaluate to `null` is dropped, so a
//!   document without an author simply has no author line;
//! - a `null` next to a non-null placeholder on the same line renders as
//!   empty;
//! - when every placeholder line was dropped, the whole header is dropped,
//!   so framing lines such as `---` never appear on their own;
//! - `{{` and `}}` are literal braces.

use crate::error::{Error, Result};
use crate::expr::{self, Compiled, Row, Value};

#[derive(Debug)]
enum Piece {
    Text(String),
    Expr(Compiled),
}

#[derive(Debug)]
struct Line {
    pieces: Vec<Piece>,
    placeholders: usize,
}

#[derive(Debug)]
pub struct Template {
    lines: Vec<Line>,
    pub needs_text: bool,
    pub needs_extra: bool,
    placeholders: usize,
}

impl Template {
    pub fn parse(src: &str) -> Result<Template> {
        let mut lines = Vec::new();
        let mut needs_text = false;
        let mut needs_extra = false;
        let mut placeholders = 0;
        for (n, raw) in src.split('\n').enumerate() {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            let line = parse_line(raw).map_err(|e| Error::Usage(format!("header template line {}: {e}", n + 1)))?;
            for p in &line.pieces {
                if let Piece::Expr(c) = p {
                    needs_text |= c.needs_text;
                    needs_extra |= c.needs_extra;
                }
            }
            placeholders += line.placeholders;
            lines.push(line);
        }
        Ok(Template { lines, needs_text, needs_extra, placeholders })
    }

    /// Render the header for `row` into `out`. Returns `false` (and leaves
    /// `out` untouched) when nothing should be prepended.
    pub fn render(&self, row: &dyn Row, out: &mut String) -> bool {
        let mut buf = String::new();
        let mut kept_placeholder_lines = 0usize;
        let mut first = true;
        for line in &self.lines {
            let mut rendered = String::new();
            let mut non_null = 0usize;
            for piece in &line.pieces {
                match piece {
                    Piece::Text(t) => rendered.push_str(t),
                    Piece::Expr(c) => {
                        let v = c.eval(row);
                        if !v.is_null() {
                            non_null += 1;
                            format_value(&v, &mut rendered);
                        }
                    }
                }
            }
            if line.placeholders > 0 {
                if non_null == 0 {
                    continue;
                }
                kept_placeholder_lines += 1;
            }
            if !first {
                buf.push('\n');
            }
            first = false;
            buf.push_str(&rendered);
        }
        if self.placeholders > 0 && kept_placeholder_lines == 0 {
            return false;
        }
        out.push_str(&buf);
        true
    }
}

fn parse_line(raw: &str) -> std::result::Result<Line, String> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut placeholders = 0;
    let mut chars = raw.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '{' => {
                if chars.peek().is_some_and(|&(_, n)| n == '{') {
                    chars.next();
                    text.push('{');
                    continue;
                }
                let rest = &raw[i + 1..];
                let Some(end) = rest.find('}') else {
                    return Err(format!("unclosed placeholder at column {}", i + 1));
                };
                let src = rest[..end].trim();
                if src.is_empty() {
                    return Err(format!("empty placeholder at column {}", i + 1));
                }
                let compiled = expr::compile(src).map_err(|e| format!("placeholder {{{src}}}: {e}"))?;
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                pieces.push(Piece::Expr(compiled));
                placeholders += 1;
                for _ in 0..=end {
                    chars.next();
                }
            }
            '}' => {
                if chars.peek().is_some_and(|&(_, n)| n == '}') {
                    chars.next();
                    text.push('}');
                } else {
                    return Err(format!("stray '}}' at column {} (write '}}}}' for a literal brace)", i + 1));
                }
            }
            _ => text.push(c),
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(Line { pieces, placeholders })
}

/// How a value prints inside a header: integers without decimals, other
/// numbers as short as possible, strings as they are.
pub fn format_value(v: &Value<'_>, out: &mut String) {
    match v {
        Value::Null => {}
        Value::Num(n) => {
            if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
                out.push_str(&format!("{n:.0}"));
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Str(s) => out.push_str(s),
        Value::List(items) => {
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_value(it, out);
            }
        }
    }
}

/// Turn the escapes a shell is likely to pass (`\n`, `\t`, `\r`, `\\`) into
/// the characters they name; anything else is kept verbatim.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::RecordRow;
    use crate::meta::Meta;

    fn row<'a>(meta: &'a Meta, extra: Option<&'a [u8]>) -> RecordRow<'a> {
        RecordRow { meta, key: &[7u8; 16], source: "gut", text: Some("Body."), extra }
    }

    fn render(t: &str, extra: Option<&str>) -> Option<String> {
        let meta = Meta { words: 42, vowel: 9512, ..Default::default() };
        let tpl = Template::parse(t).unwrap();
        let mut out = String::new();
        tpl.render(&row(&meta, extra.map(str::as_bytes)), &mut out).then_some(out)
    }

    #[test]
    fn front_matter_style() {
        let t = "---\nyear = {extra.year}\ntitle = {extra.title}\nauthor = {extra.author}\n---";
        let full = r#"{"year":1818,"title":"Frankenstein","author":"Mary Shelley"}"#;
        assert_eq!(render(t, Some(full)).unwrap(), "---\nyear = 1818\ntitle = Frankenstein\nauthor = Mary Shelley\n---");
        let no_author = r#"{"year":1818,"title":"Frankenstein"}"#;
        assert_eq!(render(t, Some(no_author)).unwrap(), "---\nyear = 1818\ntitle = Frankenstein\n---");
        // every placeholder line gone: no header at all, not a bare "---\n---"
        assert_eq!(render(t, None), None);
        assert_eq!(render(t, Some("{}")), None);
    }

    #[test]
    fn mixed_line_and_features_and_escapes() {
        let t = "{extra.title} ({extra.year}) by {extra.author}";
        assert_eq!(render(t, Some(r#"{"title":"T","year":1900}"#)).unwrap(), "T (1900) by ");
        assert_eq!(render("{words} words, {vowel}% vowels, {source}, {{braces}}", None).unwrap(), "42 words, 95.12% vowels, gut, {braces}");
        assert_eq!(render("{len(text)} chars, {words > 10}, {[1, extra.x, \"a\"]}", None).unwrap(), "5 chars, true, 1, , a");
        assert_eq!(render("{1.5 * 2}|{-0.25}|{1e16}", None).unwrap(), "3|-0.25|10000000000000000");
        // a template without placeholders is always rendered
        assert_eq!(render("static", None).unwrap(), "static");
        assert_eq!(render("", None).unwrap(), "");
        assert_eq!(render("a\r\nb", None).unwrap(), "a\nb");
        // a template with placeholders that all resolve keeps empty lines
        assert_eq!(render("{words}\n\nend", None).unwrap(), "42\n\nend");
    }

    #[test]
    fn needs_flags() {
        assert!(!Template::parse("x {words}").unwrap().needs_text);
        let t = Template::parse("{extra.a} {len(text)}").unwrap();
        assert!(t.needs_text && t.needs_extra);
        assert_eq!(Template::parse("plain").unwrap().placeholders, 0);
    }

    #[test]
    fn parse_errors_name_the_line() {
        let e = Template::parse("ok\n{extra.a").unwrap_err().to_string();
        assert!(e.contains("line 2") && e.contains("unclosed"), "{e}");
        let e = Template::parse("{}").unwrap_err().to_string();
        assert!(e.contains("empty placeholder"), "{e}");
        let e = Template::parse("a } b").unwrap_err().to_string();
        assert!(e.contains("stray"), "{e}");
        let e = Template::parse("{nosuch}").unwrap_err().to_string();
        assert!(e.contains("placeholder {nosuch}"), "{e}");
        assert_eq!(Template::parse("{a").unwrap_err().exit_code(), 2);
    }

    #[test]
    fn unescape_rules() {
        assert_eq!(unescape(r"a\nb\tc\\d\re"), "a\nb\tc\\d\re");
        assert_eq!(unescape(r"\x\"), "\\x\\");
        assert_eq!(unescape("plain"), "plain");
    }
}
