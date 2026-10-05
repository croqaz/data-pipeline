//! `pipeline check`: is the store internally consistent?
//!
//! One pass per key range, in parallel: the `meta` scan drives, and pull
//! iterators over `text` and `extra` are merged against it in key order,
//! so no point reads happen. It finds metas that fail to decode, texts or
//! extras that are missing or have no meta (orphans), extras present or
//! absent against the `has_extra` flag, and source ids without a label.
//! `--deep` also reads every text and checks that it is UTF-8, that its
//! BLAKE3 key matches, and that the stored features equal a fresh
//! computation (so a changed scanner shows up as "stale").

use std::iter::Peekable;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::features;
use crate::hash::{Key, doc_key, key_to_hex};
use crate::meta::Meta;
use crate::stats::range_for;
use crate::store::{Keyspace, KvIter, Sources, Store};
use crate::util::{fmt_int, fmt_num};

#[derive(Debug, Clone)]
pub struct CheckOptions {
    pub deep: bool,
    pub threads: usize,
    /// Example ids to keep per problem kind.
    pub examples: usize,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions { deep: false, threads: 4, examples: 5 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    CorruptMeta,
    MissingText,
    OrphanText,
    MissingExtra,
    UnexpectedExtra,
    OrphanExtra,
    UnknownSource,
    BadUtf8,
    KeyMismatch,
    StaleFeatures,
}

impl Problem {
    pub const ALL: [Problem; 10] = [
        Problem::CorruptMeta,
        Problem::MissingText,
        Problem::OrphanText,
        Problem::MissingExtra,
        Problem::UnexpectedExtra,
        Problem::OrphanExtra,
        Problem::UnknownSource,
        Problem::BadUtf8,
        Problem::KeyMismatch,
        Problem::StaleFeatures,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Problem::CorruptMeta => "corrupt meta",
            Problem::MissingText => "missing text",
            Problem::OrphanText => "orphan text",
            Problem::MissingExtra => "missing extra",
            Problem::UnexpectedExtra => "unexpected extra",
            Problem::OrphanExtra => "orphan extra",
            Problem::UnknownSource => "unknown source",
            Problem::BadUtf8 => "text not UTF-8",
            Problem::KeyMismatch => "key does not match text",
            Problem::StaleFeatures => "stale features",
        }
    }

    pub fn is_deep(self) -> bool {
        matches!(self, Problem::BadUtf8 | Problem::KeyMismatch | Problem::StaleFeatures)
    }

    fn index(self) -> usize {
        Problem::ALL.iter().position(|p| *p == self).expect("listed")
    }
}

#[derive(Debug, Clone, Default)]
pub struct Acc {
    pub meta: u64,
    pub text: u64,
    pub extra: u64,
    pub counts: [u64; 10],
    pub examples: Vec<(Problem, Key)>,
    max_examples: usize,
}

impl Acc {
    fn new(max_examples: usize) -> Self {
        Acc { max_examples, ..Default::default() }
    }

    fn problem(&mut self, p: Problem, key: &Key) {
        self.counts[p.index()] += 1;
        if self.examples.iter().filter(|(q, _)| *q == p).count() < self.max_examples {
            self.examples.push((p, *key));
        }
    }

    pub fn problems(&self) -> u64 {
        self.counts.iter().sum()
    }

    fn merge(&mut self, o: &Acc) {
        self.meta += o.meta;
        self.text += o.text;
        self.extra += o.extra;
        for (a, b) in self.counts.iter_mut().zip(&o.counts) {
            *a += b;
        }
        for (p, k) in &o.examples {
            if self.examples.iter().filter(|(q, _)| q == p).count() < self.max_examples {
                self.examples.push((*p, *k));
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckReport {
    pub acc: Acc,
    pub sources: usize,
    pub deep: bool,
    pub secs: f64,
}

impl CheckReport {
    pub fn problems(&self) -> u64 {
        self.acc.problems()
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        let n = self.problems();
        let scope = if self.deep { "deep check, every text re-read" } else { "structure only, add --deep to re-read every text" };
        if n == 0 {
            s.push_str(&format!("Check: OK, {} documents ({scope}), {} s\n", fmt_int(self.acc.meta), fmt_num(self.secs, 2)));
        } else {
            s.push_str(&format!(
                "Check: {} problem{} in {} documents ({scope}), {} s\n",
                fmt_int(n),
                if n == 1 { "" } else { "s" },
                fmt_int(self.acc.meta),
                fmt_num(self.secs, 2)
            ));
        }
        s.push_str(&format!(
            "  Keyspaces: meta {}, text {}, extra {}; {} source label{}\n",
            fmt_int(self.acc.meta),
            fmt_int(self.acc.text),
            fmt_int(self.acc.extra),
            self.sources,
            if self.sources == 1 { "" } else { "s" }
        ));
        for p in Problem::ALL {
            let c = self.acc.counts[p.index()];
            if c == 0 && (p.is_deep() && !self.deep) {
                continue;
            }
            let ex: Vec<String> = self.acc.examples.iter().filter(|(q, _)| *q == p).map(|(_, k)| key_to_hex(k)).collect();
            let tail = if ex.is_empty() { String::new() } else { format!("  e.g. {}", ex.join(", ")) };
            s.push_str(&format!("  {:<24}{:>12}{tail}\n", format!("{}:", p.as_str()), fmt_int(c)));
        }
        s
    }
}

/// Advance `it` to `target`: keys before it are orphans, a key equal to it
/// is returned with its value.
fn advance(it: &mut Peekable<KvIter<'_>>, target: &Key, acc: &mut Acc, orphan: Problem, total: &mut u64) -> Result<Option<Vec<u8>>> {
    loop {
        match it.peek() {
            None => return Ok(None),
            Some(Err(_)) => return Err(it.next().expect("peeked").expect_err("peeked an error")),
            Some(Ok((k, _))) => {
                if k < target {
                    let (k, _) = it.next().expect("peeked").expect("checked");
                    *total += 1;
                    acc.problem(orphan, &k);
                } else if k == target {
                    let (_, v) = it.next().expect("peeked").expect("checked");
                    *total += 1;
                    return Ok(Some(v));
                } else {
                    return Ok(None);
                }
            }
        }
    }
}

fn drain(it: &mut Peekable<KvIter<'_>>, acc: &mut Acc, orphan: Problem, total: &mut u64) -> Result<()> {
    for item in it {
        let (k, _) = item?;
        *total += 1;
        acc.problem(orphan, &k);
    }
    Ok(())
}

fn deep_check(key: &Key, text: &[u8], meta: Option<&Meta>, acc: &mut Acc, scratch: &mut Vec<u8>, fscratch: &mut features::Scratch) {
    let Ok(s) = std::str::from_utf8(text) else {
        acc.problem(Problem::BadUtf8, key);
        return;
    };
    if doc_key(s, scratch) != *key {
        acc.problem(Problem::KeyMismatch, key);
    }
    let Some(m) = meta else { return };
    let fresh = match features::compute(s, features::DEFAULT_MAX_LEN, fscratch) {
        Ok(f) => f.to_meta(m.source, m.has_extra()),
        Err(_) => {
            acc.problem(Problem::StaleFeatures, key);
            return;
        }
    };
    let stored = Meta { schema: fresh.schema, dict_hit: None, ..m.clone() };
    let fresh = Meta { dict_hit: None, ..fresh };
    if stored != fresh {
        acc.problem(Problem::StaleFeatures, key);
    }
}

fn check_range(store: &dyn Store, sources: &Sources, opts: &CheckOptions, lo: &Key, hi: Option<&Key>) -> Result<Acc> {
    let mut acc = Acc::new(opts.examples);
    let mut text_it = store.iter(Keyspace::Text, lo, hi, opts.deep)?.peekable();
    let mut extra_it = store.iter(Keyspace::Extra, lo, hi, false)?.peekable();
    let mut text_total = 0u64;
    let mut extra_total = 0u64;
    let mut scratch = Vec::new();
    let mut fscratch = features::Scratch::default();
    store.scan_meta(lo, hi, &mut |key, bytes| {
        acc.meta += 1;
        let meta = match Meta::decode(bytes) {
            Ok(m) => Some(m),
            Err(_) => {
                acc.problem(Problem::CorruptMeta, key);
                None
            }
        };
        match advance(&mut text_it, key, &mut acc, Problem::OrphanText, &mut text_total)? {
            Some(text) => {
                if opts.deep {
                    deep_check(key, &text, meta.as_ref(), &mut acc, &mut scratch, &mut fscratch);
                }
            }
            None => acc.problem(Problem::MissingText, key),
        }
        let extra = advance(&mut extra_it, key, &mut acc, Problem::OrphanExtra, &mut extra_total)?;
        if let Some(m) = &meta {
            match (extra.is_some(), m.has_extra()) {
                (true, false) => acc.problem(Problem::UnexpectedExtra, key),
                (false, true) => acc.problem(Problem::MissingExtra, key),
                _ => {}
            }
            if sources.name(m.source) == "?" {
                acc.problem(Problem::UnknownSource, key);
            }
        }
        Ok(true)
    })?;
    drain(&mut text_it, &mut acc, Problem::OrphanText, &mut text_total)?;
    drain(&mut extra_it, &mut acc, Problem::OrphanExtra, &mut extra_total)?;
    acc.text = text_total;
    acc.extra = extra_total;
    Ok(acc)
}

pub fn check(store: &dyn Store, sources: &Sources, opts: &CheckOptions) -> Result<CheckReport> {
    let t0 = Instant::now();
    let threads = opts.threads.clamp(1, 256);
    let results: Vec<Result<Acc>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .filter_map(|t| range_for(t, threads))
            .map(|(lo, hi)| s.spawn(move || check_range(store, sources, opts, &lo, hi.as_ref())))
            .collect();
        handles.into_iter().map(|h| h.join().expect("check thread panicked")).collect()
    });
    let mut acc = Acc::new(opts.examples);
    for r in results {
        acc.merge(&r?);
    }
    Ok(CheckReport { acc, sources: sources.len(), deep: opts.deep, secs: t0.elapsed().as_secs_f64() })
}

/// The error a failed check turns into, after the report is printed.
pub fn failure(report: &CheckReport) -> Error {
    Error::Store(format!("check found {} problem{}", fmt_int(report.problems()), if report.problems() == 1 { "" } else { "s" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, Record};

    const TEXT: &str = "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! \
How vexingly quick daft zebras jump. The five boxing wizards jump quickly.";

    fn doc(n: u32, extra: Option<&str>, source: u16) -> Record {
        let text = format!("[{n}] {TEXT}");
        let mut scratch = features::Scratch::default();
        let f = features::compute(&text, features::DEFAULT_MAX_LEN, &mut scratch).unwrap();
        let meta = f.to_meta(source, extra.is_some());
        let mut key_scratch = Vec::new();
        Record {
            key: doc_key(&text, &mut key_scratch),
            meta: meta.encode(),
            text: text.into_bytes(),
            extra: extra.map(|e| e.as_bytes().to_vec()),
            replace: false,
        }
    }

    fn fresh_store() -> (tempfile::TempDir, Box<dyn Store>, Sources) {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&dir.path().join("db"), true).unwrap();
        let mut sources = Sources::default();
        sources.intern("cli", store.as_ref()).unwrap();
        (dir, store, sources)
    }

    #[test]
    fn clean_store_passes_both_modes() {
        {
            let (_dir, store, sources) = fresh_store();
            let docs: Vec<Record> = (0..50).map(|i| doc(i, if i % 3 == 0 { Some("{\"i\":1}") } else { None }, 0)).collect();
            store.put_batch(&docs).unwrap();
            store.persist().unwrap();
            for deep in [false, true] {
                let opts = CheckOptions { deep, threads: 3, examples: 2 };
                let rep = check(store.as_ref(), &sources, &opts).unwrap();
                assert_eq!(rep.problems(), 0, "deep={deep}: {}", rep.render());
                assert_eq!((rep.acc.meta, rep.acc.text, rep.acc.extra), (50, 50, 17));
                let text = rep.render();
                assert!(text.starts_with("Check: OK, 50 documents"), "{text}");
                assert!(text.contains("meta 50, text 50, extra 17; 1 source label\n"), "{text}");
                assert_eq!(text.contains("key does not match text:"), deep, "{text}");
            }
        }
    }

    #[test]
    fn every_problem_kind_is_found_and_reported() {
        let (_dir, store, sources) = fresh_store();
        let good = doc(1, None, 0);
        let with_extra = doc(2, Some("{}"), 0);
        store.put_batch(&[good.clone(), with_extra.clone()]).unwrap();
        // corrupt meta
        store.put_raw(Keyspace::Meta, &[0x10; 16], Some(b"\xff\xff")).unwrap();
        store.put_raw(Keyspace::Text, &[0x10; 16], Some(b"t")).unwrap();
        // missing text
        store.put_raw(Keyspace::Meta, &[0x20; 16], Some(&Meta::default().encode())).unwrap();
        // orphan text and orphan extra
        store.put_raw(Keyspace::Text, &[0x30; 16], Some(b"orphan")).unwrap();
        store.put_raw(Keyspace::Extra, &[0x31; 16], Some(b"{}")).unwrap();
        // missing extra: flag set, nothing stored
        let mut m = Meta::decode(&good.meta).unwrap();
        m.flags = crate::meta::FLAG_HAS_EXTRA;
        store.put_raw(Keyspace::Meta, &[0x40; 16], Some(&m.encode())).unwrap();
        store.put_raw(Keyspace::Text, &[0x40; 16], Some(&good.text)).unwrap();
        // unexpected extra: flag clear, extra stored
        store.put_raw(Keyspace::Extra, &good.key, Some(b"{}")).unwrap();
        // unknown source
        let mut m = Meta::decode(&good.meta).unwrap();
        m.source = 9;
        store.put_raw(Keyspace::Meta, &[0x50; 16], Some(&m.encode())).unwrap();
        store.put_raw(Keyspace::Text, &[0x50; 16], Some(&good.text)).unwrap();
        // bad utf-8, key mismatch and stale features
        store.put_raw(Keyspace::Meta, &[0x60; 16], Some(&good.meta)).unwrap();
        store.put_raw(Keyspace::Text, &[0x60; 16], Some(b"\xff\xfe")).unwrap();
        store.put_raw(Keyspace::Meta, &[0x70; 16], Some(&good.meta)).unwrap();
        store.put_raw(Keyspace::Text, &[0x70; 16], Some(&good.text)).unwrap();
        let mut m = Meta::decode(&with_extra.meta).unwrap();
        m.words += 1;
        store.put_raw(Keyspace::Meta, &with_extra.key, Some(&m.encode())).unwrap();
        // a sound meta over a text that no longer passes the prefilter
        store.put_raw(Keyspace::Meta, &[0x80; 16], Some(&good.meta)).unwrap();
        store.put_raw(Keyspace::Text, &[0x80; 16], Some(b"t")).unwrap();
        // orphans after the last meta key of the range
        store.put_raw(Keyspace::Text, &[0xff; 16], Some(b"tail orphan")).unwrap();
        store.put_raw(Keyspace::Extra, &[0xfe; 16], Some(b"{}")).unwrap();
        store.persist().unwrap();

        let shallow = check(store.as_ref(), &sources, &CheckOptions { deep: false, threads: 2, examples: 5 }).unwrap();
        let count = |r: &CheckReport, p: Problem| r.acc.counts[p.index()];
        assert_eq!(count(&shallow, Problem::CorruptMeta), 1);
        assert_eq!(count(&shallow, Problem::MissingText), 1);
        assert_eq!(count(&shallow, Problem::OrphanText), 2);
        assert_eq!(count(&shallow, Problem::OrphanExtra), 2);
        assert_eq!(count(&shallow, Problem::MissingExtra), 1);
        assert_eq!(count(&shallow, Problem::UnexpectedExtra), 1);
        assert_eq!(count(&shallow, Problem::UnknownSource), 1);
        assert_eq!(count(&shallow, Problem::KeyMismatch), 0);
        assert_eq!(shallow.problems(), 9);
        assert_eq!((shallow.acc.meta, shallow.acc.text, shallow.acc.extra), (9, 10, 4));
        let text = shallow.render();
        assert!(text.starts_with("Check: 9 problems in 9 documents (structure only"), "{text}");
        assert!(
            text.contains("orphan text:                       2  e.g. 30303030303030303030303030303030, ffffffffffffffffffffffffffffffff"),
            "{text}"
        );
        assert!(!text.contains("stale features"), "{text}");
        assert_eq!(failure(&shallow).to_string(), "store error: check found 9 problems");

        let deep = check(store.as_ref(), &sources, &CheckOptions { deep: true, threads: 1, examples: 5 }).unwrap();
        assert_eq!(count(&deep, Problem::BadUtf8), 1);
        // 0x10 ("t"), 0x40, 0x50, 0x70, 0x80 all hold a text whose key is not its hash
        assert_eq!(count(&deep, Problem::KeyMismatch), 5);
        // with_extra's stored words were bumped; 0x80's text fails the prefilter; 0x10's meta is corrupt, so not compared
        assert_eq!(count(&deep, Problem::StaleFeatures), 2);
        assert!(deep.render().contains("stale features:                    2"), "{}", deep.render());
        assert_eq!(failure(&deep).to_string(), "store error: check found 17 problems");
        let one = CheckReport { acc: Acc { counts: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0], ..Acc::new(1) }, sources: 1, deep: false, secs: 0.0 };
        assert!(one.render().starts_with("Check: 1 problem in 0 documents"), "{}", one.render());
        assert_eq!(failure(&one).to_string(), "store error: check found 1 problem");
    }

    #[test]
    fn examples_are_capped_and_merged() {
        let mut a = Acc::new(2);
        for i in 0..5u8 {
            a.problem(Problem::OrphanText, &[i; 16]);
        }
        assert_eq!(a.examples.len(), 2);
        let mut b = Acc::new(2);
        b.problem(Problem::OrphanText, &[9; 16]);
        b.problem(Problem::MissingText, &[8; 16]);
        a.merge(&b);
        assert_eq!(a.examples.len(), 3);
        assert_eq!(a.problems(), 7);
        assert_eq!(Problem::ALL.len(), 10);
        assert!(Problem::StaleFeatures.is_deep() && !Problem::OrphanExtra.is_deep());
    }

    #[test]
    fn iterator_errors_propagate() {
        let (_dir, store, sources) = fresh_store();
        store.put_batch(&[doc(1, None, 0)]).unwrap();
        store.put_raw(Keyspace::Meta, &[0xaa; 16], Some(b"\x00")).unwrap();
        let rep = check(store.as_ref(), &sources, &CheckOptions::default()).unwrap();
        assert_eq!(rep.acc.counts[Problem::CorruptMeta.index()], 1);
        assert_eq!(rep.acc.counts[Problem::MissingText.index()], 1);
    }
}
