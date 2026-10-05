//! `pipeline show`: one document, by id or by unambiguous id prefix.

use crate::error::{Error, Result};
use crate::export::{Emitter, Fields};
use crate::hash::{Key, key_from_hex, key_to_hex};
use crate::meta::{FEATURES, Meta};
use crate::store::{Sources, Store};
use crate::util::{fmt_int, fmt_num, from_hex};

/// Resolve a full 32-hex id or an even-length prefix of at least 4 hex
/// characters to the one key it names.
pub fn resolve_id(store: &dyn Store, id: &str) -> Result<Key> {
    let id = id.trim();
    let usage = |m: String| Error::Usage(format!("id {id:?}: {m}"));
    if id.len() == 32 {
        let key = key_from_hex(id).ok_or_else(|| usage("not hexadecimal".into()))?;
        return if store.get_meta(&key)?.is_some() { Ok(key) } else { Err(usage("no such document".into())) };
    }
    if id.len() < 4 || id.len() > 32 || !id.len().is_multiple_of(2) {
        return Err(usage("give the full 32-character id or an even-length prefix of at least 4 characters".into()));
    }
    let prefix = from_hex(id).ok_or_else(|| usage("not hexadecimal".into()))?;
    let mut lo = [0u8; 16];
    lo[..prefix.len()].copy_from_slice(&prefix);
    let mut matches: Vec<Key> = Vec::new();
    store.scan_meta(&lo, None, &mut |k, _| {
        if !k.starts_with(&prefix) {
            return Ok(false);
        }
        matches.push(*k);
        Ok(matches.len() < 10)
    })?;
    match matches.len() {
        0 => Err(usage("no document starts with it".into())),
        1 => Ok(matches[0]),
        n => {
            let list: Vec<String> = matches.iter().map(key_to_hex).collect();
            Err(usage(format!("{}{} documents start with it: {}", if n >= 10 { "at least " } else { "" }, n, list.join(", "))))
        }
    }
}

#[derive(Debug)]
pub struct Document {
    pub key: Key,
    pub meta: Meta,
    pub text: String,
    pub extra: Option<Vec<u8>>,
}

pub fn fetch(store: &dyn Store, key: &Key) -> Result<Document> {
    let hex = key_to_hex(key);
    let meta = Meta::decode(&store.get_meta(key)?.ok_or_else(|| Error::Store(format!("meta missing for document {hex}")))?)?;
    let text = store.get_text(key)?.ok_or_else(|| Error::Store(format!("text missing for document {hex}")))?;
    let text = String::from_utf8(text).map_err(|_| Error::Store(format!("text of document {hex} is not UTF-8")))?;
    let extra = store.get_extra(key)?;
    Ok(Document { key: *key, meta, text, extra })
}

/// Human layout: header lines, then the text after a rule. `head` limits
/// the text to its first N characters.
pub fn render(doc: &Document, sources: &Sources, head: Option<usize>) -> String {
    let mut s = String::new();
    s.push_str(&format!("id:        {}\n", key_to_hex(&doc.key)));
    s.push_str(&format!("source:    {}\n", sources.name(doc.meta.source)));
    let feats: Vec<String> = FEATURES
        .iter()
        .filter_map(|f| (f.raw)(&doc.meta).map(|raw| format!("{} {}", f.name, fmt_num(f.present(raw as f64), f.decimals()))))
        .collect();
    s.push_str(&format!("features:  {}\n", feats.join("  ")));
    if let Some(e) = &doc.extra {
        s.push_str(&format!("extra:     {}\n", String::from_utf8_lossy(e)));
    }
    let chars = doc.text.chars().count();
    s.push_str(&format!("--- text ({} chars) ---\n", fmt_int(chars as u64)));
    match head {
        Some(n) if n < chars => {
            let cut: String = doc.text.chars().take(n).collect();
            s.push_str(&cut);
            s.push_str(&format!("\n... [{} more chars]\n", fmt_int((chars - n) as u64)));
        }
        _ => {
            s.push_str(&doc.text);
            if !doc.text.ends_with('\n') {
                s.push('\n');
            }
        }
    }
    s
}

/// The same line `export` writes by default.
pub fn render_json(doc: &Document, sources: &Sources) -> Result<String> {
    let fields = Fields::default();
    let emitter = Emitter::new(&fields, None, "", sources);
    let mut out = Vec::new();
    emitter.write(&mut out, &doc.key, &doc.meta, Some(&doc.text), doc.extra.as_deref())?;
    Ok(String::from_utf8(out).expect("JSON is UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, Keyspace, Record};

    fn store_with(keys: &[[u8; 16]]) -> (tempfile::TempDir, Box<dyn Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&dir.path().join("db"), true).unwrap();
        let recs: Vec<Record> = keys
            .iter()
            .map(|k| Record { key: *k, meta: Meta { words: 3, ..Default::default() }.encode(), text: b"Hello.\n".to_vec(), extra: None, replace: false })
            .collect();
        store.put_batch(&recs).unwrap();
        (dir, store)
    }

    #[test]
    fn resolves_full_ids_and_prefixes() {
        let mut a = [0u8; 16];
        a[0] = 0xab;
        a[1] = 0xcd;
        let mut b = a;
        b[2] = 0x01;
        let (_dir, store) = store_with(&[a, b]);
        assert_eq!(resolve_id(store.as_ref(), &key_to_hex(&a)).unwrap(), a);
        assert_eq!(resolve_id(store.as_ref(), " abcd00 ").unwrap(), a);
        assert_eq!(resolve_id(store.as_ref(), "abcd01").unwrap(), b);
        let e = |s: &str| resolve_id(store.as_ref(), s).unwrap_err().to_string();
        assert!(e("ab").contains("at least 4"), "{}", e("ab"));
        assert!(e("abc").contains("even-length"));
        assert!(e("zzzz").contains("not hexadecimal"));
        assert!(e(&"z".repeat(32)).contains("not hexadecimal"));
        assert!(e(&"00".repeat(16)).contains("no such document"));
        assert!(e("0000").contains("no document starts"));
        let amb = e("abcd");
        assert!(amb.contains("2 documents start with it") && amb.contains(&key_to_hex(&b)), "{amb}");
        // ten or more matches say "at least"
        let many: Vec<[u8; 16]> = (0..12u8)
            .map(|i| {
                let mut k = [0u8; 16];
                k[0] = 0x77;
                k[15] = i;
                k
            })
            .collect();
        let (_dir2, store2) = store_with(&many);
        let e = resolve_id(store2.as_ref(), "7700").unwrap_err().to_string();
        assert!(e.contains("at least 10 documents start with it"), "{e}");
        assert_eq!(resolve_id(store2.as_ref(), &key_to_hex(&many[3])).unwrap(), many[3]);
    }

    #[test]
    fn fetch_and_render() {
        let (_dir, store) = store_with(&[[1u8; 16]]);
        let mut sources = Sources::default();
        sources.intern("cli", store.as_ref()).unwrap();
        let doc = fetch(store.as_ref(), &[1u8; 16]).unwrap();
        let s = render(&doc, &sources, None);
        assert!(s.starts_with("id:        01010101010101010101010101010101\nsource:    cli\nfeatures:  len 0  bytes 0  words 3"), "{s}");
        assert!(s.ends_with("--- text (7 chars) ---\nHello.\n"), "{s}");
        assert!(!s.contains("extra:"));
        let s = render(&doc, &sources, Some(3));
        assert!(s.ends_with("--- text (7 chars) ---\nHel\n... [4 more chars]\n"), "{s}");
        assert!(render(&doc, &sources, Some(7)).ends_with("Hello.\n"));
        let json = render_json(&doc, &sources).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["words"], 3);
        assert_eq!(v["text"], "Hello.\n");

        store.put_raw(Keyspace::Extra, &[1u8; 16], Some(b"{\"k\":1}")).unwrap();
        store.put_raw(Keyspace::Text, &[2u8; 16], Some(b"no newline")).unwrap();
        store.put_raw(Keyspace::Meta, &[2u8; 16], Some(&Meta::default().encode())).unwrap();
        let doc = fetch(store.as_ref(), &[1u8; 16]).unwrap();
        assert!(render(&doc, &sources, None).contains("extra:     {\"k\":1}\n"));
        let doc = fetch(store.as_ref(), &[2u8; 16]).unwrap();
        assert!(render(&doc, &sources, None).ends_with("no newline\n"));

        let e = fetch(store.as_ref(), &[9u8; 16]).unwrap_err().to_string();
        assert!(e.contains("meta missing"), "{e}");
        store.put_raw(Keyspace::Meta, &[3u8; 16], Some(&Meta::default().encode())).unwrap();
        let e = fetch(store.as_ref(), &[3u8; 16]).unwrap_err().to_string();
        assert!(e.contains("text missing"), "{e}");
        store.put_raw(Keyspace::Text, &[3u8; 16], Some(b"\xff")).unwrap();
        let e = fetch(store.as_ref(), &[3u8; 16]).unwrap_err().to_string();
        assert!(e.contains("not UTF-8"), "{e}");
    }
}
