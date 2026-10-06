//! `pipeline query`: the matching documents themselves, where `count` gives
//! only their number.
//!
//! Without `--sort` the store is read in key order and the scan stops at
//! the limit, so the first matches come back at once. Keys are hashes of
//! the text, so key order is unrelated to source, length or content: the
//! first N matches are a fair sample. `--random` starts at a random key and
//! wraps around, for a different sample each time. With `--sort FEATURE`
//! every record is scanned in parallel and the N highest (or lowest) are
//! kept.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::error::{Error, Result};
use crate::expr::Compiled;
use crate::hash::{Key, key_to_hex};
use crate::meta::{FEATURES, FeatureDef, Meta, feature_index};
use crate::show::{self, Clip};
use crate::stats::{ScanConfig, matches, scan};
use crate::store::{Sources, Store};
use crate::util::{fmt_int, fmt_num};

pub const DEFAULT_LIMIT: usize = 20;
/// Texts longer than twice this show their first and last this many
/// characters, as query.ts did.
pub const DEFAULT_ENDS: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortBy {
    pub feature: usize,
    pub ascending: bool,
}

impl SortBy {
    pub fn parse(name: &str, ascending: bool) -> Result<SortBy> {
        let feature = feature_index(name).ok_or_else(|| {
            let names: Vec<&str> = FEATURES.iter().map(|f| f.name).collect();
            Error::Usage(format!("--sort {name:?}: not a feature; one of {}", names.join(", ")))
        })?;
        Ok(SortBy { feature, ascending })
    }

    fn def(&self) -> &'static FeatureDef {
        &FEATURES[self.feature]
    }
}

pub struct QueryOptions {
    pub limit: usize,
    pub sort: Option<SortBy>,
    /// Start the key-order scan here and wrap around.
    pub start: Option<Key>,
    pub threads: usize,
}

#[derive(Debug, Default)]
pub struct Selection {
    pub keys: Vec<Key>,
    /// For a key-order scan: how many documents had been read when each
    /// match was found (1-based, counted from the start key). Empty when
    /// sorted.
    pub positions: Vec<u64>,
    pub scanned: u64,
    /// Every match in the store; known only after a full scan (`--sort`).
    pub total: Option<u64>,
    /// Matches of a sorted scan that lack the sort feature.
    pub without_feature: u64,
}

/// The first `limit` matches in key order from `start` (wrapping), or the
/// `limit` best by the sort feature.
pub fn select(store: &dyn Store, sources: &Sources, filter: Option<&Compiled>, opts: &QueryOptions) -> Result<Selection> {
    match opts.sort {
        None => select_in_order(store, sources, filter, opts),
        Some(sort) => select_sorted(store, sources, filter, opts, sort),
    }
}

fn select_in_order(store: &dyn Store, sources: &Sources, filter: Option<&Compiled>, opts: &QueryOptions) -> Result<Selection> {
    let mut sel = Selection::default();
    if opts.limit == 0 {
        return Ok(sel);
    }
    let zero = [0u8; 16];
    let start = opts.start.unwrap_or(zero);
    let ranges: [(Key, Option<Key>); 2] = [(start, None), (zero, Some(start))];
    for (lo, hi) in ranges.iter().take(if start == zero { 1 } else { 2 }) {
        store.scan_meta(lo, hi.as_ref(), &mut |key, bytes| {
            sel.scanned += 1;
            let meta = Meta::decode(bytes)?;
            let keep = match filter {
                None => true,
                Some(f) => matches(store, sources, f, key, &meta)?,
            };
            if keep {
                sel.keys.push(*key);
                sel.positions.push(sel.scanned);
            }
            Ok(sel.keys.len() < opts.limit)
        })?;
        if sel.keys.len() >= opts.limit {
            break;
        }
    }
    Ok(sel)
}

fn select_sorted(store: &dyn Store, sources: &Sources, filter: Option<&Compiled>, opts: &QueryOptions, sort: SortBy) -> Result<Selection> {
    let limit = opts.limit;
    let raw = sort.def().raw;
    // Each thread keeps its `limit` best in a heap whose top is the worst
    // kept entry. Ties go to the smaller key, so every run picks the same.
    // Descending: min-heap on (value, Reverse(key)) drops the smallest value,
    // then the larger key. Ascending: max-heap on (value, key) drops the
    // largest value, then the larger key.
    struct Acc {
        desc: BinaryHeap<Reverse<(u64, Reverse<Key>)>>,
        asc: BinaryHeap<(u64, Key)>,
        missing: u64,
    }
    let cfg = ScanConfig { filter, threads: opts.threads, limit: None };
    let (scanned, matched, accs) = scan(
        store,
        sources,
        &cfg,
        || Acc { desc: BinaryHeap::new(), asc: BinaryHeap::new(), missing: 0 },
        |acc, key, meta| {
            let Some(v) = raw(meta) else {
                acc.missing += 1;
                return true;
            };
            if sort.ascending {
                acc.asc.push((v, *key));
                if acc.asc.len() > limit {
                    acc.asc.pop();
                }
            } else {
                acc.desc.push(Reverse((v, Reverse(*key))));
                if acc.desc.len() > limit {
                    acc.desc.pop();
                }
            }
            true
        },
    )?;
    let mut all: Vec<(u64, Key)> = Vec::new();
    let mut missing = 0;
    for a in accs {
        missing += a.missing;
        all.extend(a.asc);
        all.extend(a.desc.into_iter().map(|Reverse((v, Reverse(k)))| (v, k)));
    }
    if sort.ascending {
        all.sort();
    } else {
        all.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    all.truncate(limit);
    Ok(Selection {
        keys: all.into_iter().map(|(_, k)| k).collect(),
        positions: Vec::new(),
        scanned,
        total: Some(matched),
        without_feature: missing,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The `show` layout, one block per match.
    Readable(Clip),
    /// The export JSON line per match.
    Json,
    /// One id per line.
    Ids,
}

/// Print the selected documents.
pub fn print(
    store: &dyn Store,
    sources: &Sources,
    sel: &Selection,
    sort: Option<SortBy>,
    output: Output,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let n = sel.keys.len();
    for (i, key) in sel.keys.iter().enumerate() {
        match output {
            Output::Ids => writeln!(out, "{}", key_to_hex(key))?,
            Output::Json => write!(out, "{}", show::render_json(&show::fetch(store, key)?, sources)?)?,
            Output::Readable(clip) => {
                let doc = show::fetch(store, key)?;
                let by = match sort {
                    Some(s) => {
                        let d = s.def();
                        let v = (d.raw)(&doc.meta).map_or("-".to_string(), |r| fmt_num(d.present(r as f64), d.decimals()));
                        format!("  {} {v}", d.name)
                    }
                    None => String::new(),
                };
                let at = sel.positions.get(i).map_or(String::new(), |p| format!(", scanned {}", fmt_int(*p)));
                if i > 0 {
                    writeln!(out)?;
                }
                writeln!(out, "=== Match {} of {}{at}{by} ===", fmt_int(i as u64 + 1), fmt_int(n as u64))?;
                write!(out, "{}", show::render_clipped(&doc, sources, clip))?;
            }
        }
    }
    Ok(())
}

/// One or two lines for stderr after the matches.
pub fn summary(sel: &Selection, opts: &QueryOptions) -> String {
    let shown = sel.keys.len();
    let mut s = match (opts.sort, sel.total) {
        (Some(sort), Some(total)) => format!(
            "Showed {} of {} matches, {} {} first; scanned {} documents.\n",
            fmt_int(shown as u64),
            fmt_int(total),
            if sort.ascending { "lowest" } else { "highest" },
            sort.def().name,
            fmt_int(sel.scanned),
        ),
        _ if shown >= opts.limit && opts.limit > 0 => format!(
            "Showed the first {} matches in key order after scanning {} documents; `count` gives the total, --limit shows more.\n",
            fmt_int(shown as u64),
            fmt_int(sel.scanned)
        ),
        _ => format!("Showed all {} matches; scanned {} documents.\n", fmt_int(shown as u64), fmt_int(sel.scanned)),
    };
    if sel.without_feature > 0 {
        let name = opts.sort.map_or("", |s| s.def().name);
        s.push_str(&format!("{} matches have no {name} and were left out; run `recompute` to fill it.\n", fmt_int(sel.without_feature)));
    }
    s
}

/// A start key derived from `seed`, for `--random`.
pub fn start_key(seed: u64) -> Key {
    let h = blake3::hash(&seed.to_le_bytes());
    let mut k = [0u8; 16];
    k.copy_from_slice(&h.as_bytes()[..16]);
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr;
    use crate::store::{self, Record};

    /// Keys [i; 16] with words = `words[i]`; record 4 has no dup10gram.
    fn store_with(words: &[u32]) -> (tempfile::TempDir, Box<dyn Store>, Sources) {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&dir.path().join("db"), true).unwrap();
        let recs: Vec<Record> = words
            .iter()
            .enumerate()
            .map(|(i, &w)| Record {
                key: [i as u8 * 16; 16],
                meta: Meta { words: w, dup10gram: (i != 4).then_some(w as u16 * 100), ..Default::default() }.encode(),
                text: format!("doc {i}").into_bytes(),
                extra: None,
                replace: false,
            })
            .collect();
        store.put_batch(&recs).unwrap();
        let mut sources = Sources::default();
        sources.intern("t", store.as_ref()).unwrap();
        (dir, store, sources)
    }

    fn opts(limit: usize, sort: Option<SortBy>, start: Option<Key>) -> QueryOptions {
        QueryOptions { limit, sort, start, threads: 3 }
    }

    fn firsts(sel: &Selection) -> Vec<u8> {
        sel.keys.iter().map(|k| k[0] / 16).collect()
    }

    #[test]
    fn key_order_stops_at_the_limit_and_wraps_from_a_start() {
        let (_d, store, sources) = store_with(&[5, 1, 7, 3, 9, 2]);
        let s = store.as_ref();
        let sel = select(s, &sources, None, &opts(2, None, None)).unwrap();
        assert_eq!((firsts(&sel), sel.scanned, sel.total), (vec![0, 1], 2, None));
        let f = expr::compile("words >= 3").unwrap();
        let sel = select(s, &sources, Some(&f), &opts(10, None, None)).unwrap();
        assert_eq!((firsts(&sel), sel.scanned), (vec![0, 2, 3, 4], 6));
        assert_eq!(sel.positions, vec![1, 3, 4, 5]);
        // start in the middle: 3, 4, then wrap to 0, 2
        let sel = select(s, &sources, Some(&f), &opts(10, None, Some([3 * 16; 16]))).unwrap();
        assert_eq!((firsts(&sel), sel.scanned), (vec![3, 4, 0, 2], 6));
        assert_eq!(sel.positions, vec![1, 2, 4, 6], "counted from the start key");
        let sel = select(s, &sources, Some(&f), &opts(3, None, Some([3 * 16; 16]))).unwrap();
        assert_eq!(firsts(&sel), vec![3, 4, 0]);
        assert!(select(s, &sources, None, &opts(0, None, None)).unwrap().keys.is_empty());
        // a filter on the text reads it
        let t = expr::compile("text.contains(\"doc 5\")").unwrap();
        assert_eq!(firsts(&select(s, &sources, Some(&t), &opts(5, None, None)).unwrap()), vec![5]);
    }

    #[test]
    fn sorted_keeps_the_best_and_breaks_ties_by_key() {
        let (_d, store, sources) = store_with(&[5, 1, 7, 7, 9, 2, 7]);
        let s = store.as_ref();
        let sort = |name: &str, asc| Some(SortBy::parse(name, asc).unwrap());
        let sel = select(s, &sources, None, &opts(3, sort("words", false), None)).unwrap();
        assert_eq!((firsts(&sel), sel.total, sel.scanned), (vec![4, 2, 3], Some(7), 7));
        let sel = select(s, &sources, None, &opts(2, sort("words", true), None)).unwrap();
        assert_eq!(firsts(&sel), vec![1, 5]);
        let sel = select(s, &sources, None, &opts(4, sort("words", true), None)).unwrap();
        assert_eq!(firsts(&sel), vec![1, 5, 0, 2], "7 ties: the smallest key first");
        // record 4 has no dup10gram: left out and counted
        let sel = select(s, &sources, None, &opts(10, sort("dup10gram", false), None)).unwrap();
        assert_eq!((firsts(&sel), sel.without_feature), (vec![2, 3, 6, 0, 5, 1], 1));
        let f = expr::compile("words < 7").unwrap();
        let sel = select(s, &sources, Some(&f), &opts(10, sort("words", false), None)).unwrap();
        assert_eq!((firsts(&sel), sel.total), (vec![0, 5, 1], Some(3)));
        let sel = select(s, &sources, None, &opts(0, sort("words", false), None)).unwrap();
        assert_eq!((sel.keys.len(), sel.total), (0, Some(7)));
        let e = SortBy::parse("nope", false).unwrap_err().to_string();
        assert!(e.contains("not a feature") && e.contains("dup10gram"), "{e}");
    }

    #[test]
    fn printing_and_summaries() {
        let (_d, store, sources) = store_with(&[5, 1, 7]);
        let s = store.as_ref();
        let o = opts(2, Some(SortBy::parse("words", false).unwrap()), None);
        let sel = select(s, &sources, None, &o).unwrap();
        let text = |output| {
            let mut out = Vec::new();
            print(s, &sources, &sel, o.sort, output, &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        let r = text(Output::Readable(Clip::Full));
        assert!(r.starts_with("=== Match 1 of 2  words 7 ===\nid:        20202020"), "{r}");
        assert!(r.contains("\n\n=== Match 2 of 2  words 5 ===\n"), "{r}");
        assert!(r.contains("--- text (5 chars) ---\ndoc 2\n"), "{r}");
        assert_eq!(text(Output::Ids), format!("{}\n{}\n", key_to_hex(&[32; 16]), key_to_hex(&[0; 16])));
        let j = text(Output::Json);
        let first: serde_json::Value = serde_json::from_str(j.lines().next().unwrap()).unwrap();
        assert_eq!((first["text"].as_str(), first["words"].as_u64()), (Some("doc 2"), Some(7)));
        assert_eq!(summary(&sel, &o), "Showed 2 of 3 matches, highest words first; scanned 3 documents.\n");
        let asc = opts(2, Some(SortBy::parse("dup10gram", true).unwrap()), None);
        let missing = Selection { keys: vec![], positions: vec![], scanned: 9, total: Some(4), without_feature: 2 };
        assert_eq!(
            summary(&missing, &asc),
            "Showed 0 of 4 matches, lowest dup10gram first; scanned 9 documents.\n2 matches have no dup10gram and were left out; run `recompute` to fill it.\n"
        );
        // a missing sort value prints as "-"
        let gap = Selection { keys: vec![[4 * 16; 16]], ..Default::default() };
        let (_d2, store2, sources2) = store_with(&[1, 1, 1, 1, 1]);
        let mut out = Vec::new();
        print(store2.as_ref(), &sources2, &gap, asc.sort, Output::Readable(Clip::Head(2)), &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("=== Match 1 of 1  dup10gram - ===\n"));
        let unsorted = opts(2, None, None);
        let full = Selection { keys: vec![[0; 16], [1; 16]], scanned: 40, ..Default::default() };
        let in_order = select(s, &sources, None, &unsorted).unwrap();
        let mut out = Vec::new();
        print(s, &sources, &in_order, None, Output::Readable(Clip::Full), &mut out).unwrap();
        let r = String::from_utf8(out).unwrap();
        assert!(r.starts_with("=== Match 1 of 2, scanned 1 ===\n") && r.contains("=== Match 2 of 2, scanned 2 ===\n"), "{r}");
        assert!(summary(&full, &unsorted).starts_with("Showed the first 2 matches in key order after scanning 40 documents;"));
        let few = Selection { keys: vec![[0; 16]], scanned: 40, ..Default::default() };
        assert_eq!(summary(&few, &unsorted), "Showed all 1 matches; scanned 40 documents.\n");
        assert_eq!(start_key(7), start_key(7));
        assert_ne!(start_key(7), start_key(8));
    }
}
