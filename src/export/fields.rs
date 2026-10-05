//! Which fields an exported record carries, and the JSON line writer.
//!
//! Field names: `id`, `source`, `text`, `features` (every stored feature),
//! any single feature name, `extra` (the stored metadata as a nested
//! object), `extra.*` (metadata keys flattened to the top level) and
//! `extra.<key>` (one metadata key, flattened). The default is
//! `id,source,features,text,extra.*`. A flattened metadata key that
//! collides with a built-in field is skipped and counted.

use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use sonic_rs::JsonValueTrait;

use crate::error::{Error, Result};
use crate::expr::RecordRow;
use crate::hash::{Key, key_to_hex};
use crate::meta::{FEATURES, Kind, Meta, feature_index};
use crate::store::Sources;

use super::template::Template;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Id,
    Source,
    Text,
    AllFeatures,
    Feature(usize),
    ExtraNested,
    ExtraAll,
    ExtraKey(String),
}

#[derive(Debug, Clone)]
pub struct Fields {
    pub list: Vec<Field>,
    /// Top-level names the built-in fields can produce.
    reserved: HashSet<String>,
}

impl Default for Fields {
    fn default() -> Self {
        Fields::parse(&["id", "source", "features", "text", "extra.*"]).expect("default field list is valid")
    }
}

impl Fields {
    pub fn parse<S: AsRef<str>>(names: &[S]) -> Result<Fields> {
        let mut list = Vec::new();
        let mut reserved = HashSet::new();
        let mut seen = HashSet::new();
        let usage = |m: String| Error::Usage(format!("--fields: {m}"));
        for name in names {
            let name = name.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            if !seen.insert(name.to_string()) {
                return Err(usage(format!("{name} given twice")));
            }
            let field = match name {
                "id" => Field::Id,
                "source" => Field::Source,
                "text" => Field::Text,
                "features" => Field::AllFeatures,
                "extra" => Field::ExtraNested,
                "extra.*" => Field::ExtraAll,
                _ => {
                    if let Some(key) = name.strip_prefix("extra.") {
                        if key.is_empty() {
                            return Err(usage("extra. needs a key name".into()));
                        }
                        Field::ExtraKey(key.to_string())
                    } else if let Some(i) = feature_index(name) {
                        Field::Feature(i)
                    } else {
                        let mut valid = vec!["id", "source", "text", "features", "extra", "extra.*", "extra.<key>"];
                        valid.extend(FEATURES.iter().map(|f| f.name));
                        return Err(usage(format!("unknown field {name:?}; valid: {}", valid.join(", "))));
                    }
                }
            };
            match &field {
                Field::Id => reserved.insert("id".into()),
                Field::Source => reserved.insert("source".into()),
                Field::Text => reserved.insert("text".into()),
                Field::ExtraNested => reserved.insert("extra".into()),
                Field::AllFeatures => {
                    reserved.extend(FEATURES.iter().map(|f| f.name.to_string()));
                    true
                }
                Field::Feature(i) => reserved.insert(FEATURES[*i].name.into()),
                Field::ExtraAll | Field::ExtraKey(_) => true,
            };
            list.push(field);
        }
        if list.is_empty() {
            return Err(usage("no fields selected".into()));
        }
        for f in &list {
            if let Field::ExtraKey(k) = f
                && reserved.contains(k)
            {
                return Err(usage(format!("extra.{k} collides with the built-in field {k}")));
            }
        }
        Ok(Fields { list, reserved })
    }

    pub fn needs_text(&self) -> bool {
        self.list.contains(&Field::Text)
    }

    pub fn needs_extra(&self) -> bool {
        self.list.iter().any(|f| matches!(f, Field::ExtraNested | Field::ExtraAll | Field::ExtraKey(_)))
    }
}

/// Writes one JSON object per document.
pub struct Emitter<'a> {
    pub fields: &'a Fields,
    pub header: Option<&'a Template>,
    pub sep: &'a str,
    pub sources: &'a Sources,
    /// Flattened metadata keys skipped because a built-in field owns the name.
    pub shadowed: AtomicU64,
}

fn json_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    sonic_rs::to_writer(&mut *out, s).map_err(Error::store)
}

impl<'a> Emitter<'a> {
    pub fn new(fields: &'a Fields, header: Option<&'a Template>, sep: &'a str, sources: &'a Sources) -> Self {
        Emitter { fields, header, sep, sources, shadowed: AtomicU64::new(0) }
    }

    /// True when the text keyspace has to be read for this field list.
    pub fn needs_text(&self) -> bool {
        self.fields.needs_text()
    }

    pub fn needs_extra(&self) -> bool {
        self.fields.needs_extra() || (self.fields.needs_text() && self.header.is_some_and(|h| h.needs_extra))
    }

    /// Append one record as a JSON line. `text` may be `None` only when the
    /// field list does not include it.
    pub fn write(&self, out: &mut Vec<u8>, key: &Key, meta: &Meta, text: Option<&str>, extra: Option<&[u8]>) -> Result<()> {
        out.push(b'{');
        let mut first = true;
        let mut comma = |out: &mut Vec<u8>| {
            if !first {
                out.push(b',');
            }
            first = false;
        };
        for field in &self.fields.list {
            match field {
                Field::Id => {
                    comma(out);
                    out.extend_from_slice(b"\"id\":\"");
                    out.extend_from_slice(key_to_hex(key).as_bytes());
                    out.push(b'"');
                }
                Field::Source => {
                    comma(out);
                    out.extend_from_slice(b"\"source\":");
                    json_str(out, self.sources.name(meta.source))?;
                }
                Field::Text => {
                    let text = text.ok_or_else(|| Error::Store("export: text not fetched".into()))?;
                    comma(out);
                    out.extend_from_slice(b"\"text\":");
                    self.write_text(out, key, meta, text, extra)?;
                }
                Field::AllFeatures => {
                    for (i, def) in FEATURES.iter().enumerate() {
                        if let Some(raw) = (def.raw)(meta) {
                            comma(out);
                            write_feature(out, i, raw);
                        }
                    }
                }
                Field::Feature(i) => {
                    if let Some(raw) = (FEATURES[*i].raw)(meta) {
                        comma(out);
                        write_feature(out, *i, raw);
                    }
                }
                Field::ExtraNested => {
                    if let Some(bytes) = extra {
                        comma(out);
                        out.extend_from_slice(b"\"extra\":");
                        out.extend_from_slice(bytes);
                    }
                }
                Field::ExtraAll => {
                    if let Some(bytes) = extra {
                        for item in sonic_rs::to_object_iter(bytes) {
                            let (k, v) = item.map_err(|e| Error::Store(format!("stored extra is not a JSON object: {e}")))?;
                            if self.fields.reserved.contains(&*k) {
                                self.shadowed.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            comma(out);
                            json_str(out, &k)?;
                            out.push(b':');
                            out.extend_from_slice(v.as_raw_str().as_bytes());
                        }
                    }
                }
                Field::ExtraKey(k) => {
                    if let Some(bytes) = extra
                        && let Ok(v) = sonic_rs::get(bytes, &[k.as_str()])
                        && !v.is_null()
                    {
                        comma(out);
                        json_str(out, k)?;
                        out.push(b':');
                        out.extend_from_slice(v.as_raw_str().as_bytes());
                    }
                }
            }
        }
        out.extend_from_slice(b"}\n");
        Ok(())
    }

    fn write_text(&self, out: &mut Vec<u8>, key: &Key, meta: &Meta, text: &str, extra: Option<&[u8]>) -> Result<()> {
        if let Some(tpl) = self.header {
            let row = RecordRow { meta, key, source: self.sources.name(meta.source), text: Some(text), extra };
            let mut head = String::new();
            if tpl.render(&row, &mut head) {
                head.reserve(self.sep.len() + text.len());
                head.push_str(self.sep);
                head.push_str(text);
                return json_str(out, &head);
            }
        }
        json_str(out, text)
    }
}

fn write_feature(out: &mut Vec<u8>, idx: usize, raw: u64) {
    let def = &FEATURES[idx];
    out.push(b'"');
    out.extend_from_slice(def.name.as_bytes());
    out.extend_from_slice(b"\":");
    match def.kind {
        Kind::Int => {
            let _ = write!(out, "{raw}");
        }
        Kind::Pct | Kind::Bits => {
            let _ = write!(out, "{:.2}", def.present(raw as f64));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Sources {
        let mut s = Sources::default();
        s.intern("gut", &crate::store::NullStore::default()).unwrap();
        s
    }

    fn meta() -> Meta {
        Meta { schema: 1, source: 0, len: 5, bytes: 5, words: 1, uniq_words: 1, sentences: 1, uniq_chars: 4, vowel: 10_000, alpha: 5000, flags: 1, dict_hit: None, entropy: Some(412), compress: Some(6150) }
    }

    fn emit(fields: &Fields, header: Option<&Template>, text: Option<&str>, extra: Option<&str>) -> String {
        let s = sources();
        let e = Emitter::new(fields, header, "\n\n", &s);
        let mut out = Vec::new();
        e.write(&mut out, &[1u8; 16], &meta(), text, extra.map(str::as_bytes)).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn default_layout_is_valid_json_in_order() {
        let line = emit(&Fields::default(), None, Some("Bo\"dy\n"), Some(r#"{"year":1818,"title":"F","text":"shadow","words":9}"#));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], "01010101010101010101010101010101");
        assert_eq!(v["source"], "gut");
        assert_eq!(v["len"], 5);
        assert_eq!(v["vowel"], 100.0);
        assert_eq!(v["alpha"], 50.0);
        assert!(v.get("dictHit").is_none());
        assert_eq!(v["text"], "Bo\"dy\n");
        assert_eq!(v["year"], 1818);
        assert_eq!(v["title"], "F");
        assert_eq!(v["words"], 1, "the feature wins over the metadata key");
        // order: id, source, features, text, then the flattened metadata
        let pos = |k: &str| line.find(&format!("\"{k}\":")).unwrap();
        assert!(pos("id") < pos("source") && pos("source") < pos("len") && pos("alpha") < pos("text") && pos("text") < pos("year") && pos("year") < pos("title"));
        assert!(line.starts_with("{\"id\":") && line.ends_with("}\n"));
    }

    #[test]
    fn shadowed_keys_are_counted() {
        let f = Fields::default();
        let s = sources();
        let e = Emitter::new(&f, None, "", &s);
        let mut out = Vec::new();
        e.write(&mut out, &[0u8; 16], &meta(), Some("t"), Some(br#"{"id":1,"text":2,"ok":3}"#)).unwrap();
        assert_eq!(e.shadowed.load(Ordering::Relaxed), 2);
        assert!(String::from_utf8(out).unwrap().contains(r#""ok":3"#));
    }

    #[test]
    fn subsets_nested_and_single_keys() {
        let f = Fields::parse(&["text", "extra.title", "words", "extra"]).unwrap();
        let line = emit(&f, None, Some("T"), Some(r#"{"title":"F","year":1}"#));
        assert_eq!(line, "{\"text\":\"T\",\"title\":\"F\",\"words\":1,\"extra\":{\"title\":\"F\",\"year\":1}}\n");
        // no extra stored: the keys are simply absent
        let line = emit(&f, None, Some("T"), None);
        assert_eq!(line, "{\"text\":\"T\",\"words\":1}\n");
        // a null metadata value is treated as absent
        let line = emit(&Fields::parse(&["extra.title"]).unwrap(), None, None, Some(r#"{"title":null}"#));
        assert_eq!(line, "{}\n");
        let f = Fields::parse(&["id", "dictHit", "source"]).unwrap();
        assert!(!f.needs_text() && !f.needs_extra());
        assert_eq!(emit(&f, None, None, None), "{\"id\":\"01010101010101010101010101010101\",\"source\":\"gut\"}\n");
        let line = emit(&Fields::parse(&["vowel", "alpha"]).unwrap(), None, None, None);
        assert_eq!(line, "{\"vowel\":100.00,\"alpha\":50.00}\n");
    }

    #[test]
    fn header_is_prepended_only_when_rendered() {
        let tpl = Template::parse("---\ntitle = {extra.title}\n---").unwrap();
        let f = Fields::parse(&["text"]).unwrap();
        let line = emit(&f, Some(&tpl), Some("Body"), Some(r#"{"title":"F"}"#));
        assert_eq!(line, "{\"text\":\"---\\ntitle = F\\n---\\n\\nBody\"}\n");
        let line = emit(&f, Some(&tpl), Some("Body"), None);
        assert_eq!(line, "{\"text\":\"Body\"}\n");
        let s = sources();
        let e = Emitter::new(&f, Some(&tpl), "", &s);
        assert!(e.needs_text() && e.needs_extra());
        let plain = Fields::parse(&["id"]).unwrap();
        let e = Emitter::new(&plain, Some(&tpl), "", &s);
        assert!(!e.needs_extra(), "a header never forces extra reads when text is not exported");
    }

    #[test]
    fn missing_text_is_an_error() {
        let s = sources();
        let f = Fields::parse(&["text"]).unwrap();
        let e = Emitter::new(&f, None, "", &s);
        let err = e.write(&mut Vec::new(), &[0u8; 16], &meta(), None, None).unwrap_err();
        assert!(err.to_string().contains("text not fetched"));
        let f = Fields::parse(&["extra.*"]).unwrap();
        let e = Emitter::new(&f, None, "", &s);
        let err = e.write(&mut Vec::new(), &[0u8; 16], &meta(), None, Some(b"[1]")).unwrap_err();
        assert!(err.to_string().contains("not a JSON object"), "{err}");
    }

    #[test]
    fn parse_errors() {
        let msg = |names: &[&str]| Fields::parse(names).unwrap_err().to_string();
        assert!(msg(&["nope"]).contains("unknown field \"nope\"; valid: id, source"));
        assert!(msg(&["id", "id"]).contains("given twice"));
        assert!(msg(&["extra."]).contains("needs a key name"));
        assert!(msg(&["words", "extra.words"]).contains("collides"));
        assert!(msg(&["features", "extra.len"]).contains("collides"));
        assert!(msg(&[""]).contains("no fields"));
        assert_eq!(Fields::parse(&["extra.words"]).unwrap().list, vec![Field::ExtraKey("words".into())]);
        assert_eq!(Fields::parse(&[" text ", ""]).unwrap().list, vec![Field::Text]);
    }
}
