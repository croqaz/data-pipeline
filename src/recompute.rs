//! `pipeline recompute`: re-read every text and rewrite its features with
//! this build's scanners.
//!
//! Adding a feature or changing a scanner is never a re-import. The walk
//! runs one key range per thread; each range drives on `meta` and merges a
//! pull iterator over `text` against it in key order, computes the
//! features the way `import` does, and rewrites `meta` only where the
//! result differs. `text`, `extra`, the key, the source and the flags are
//! never touched. A document that today's prefilter would reject keeps its
//! old record and is counted; `delete` is the tool for removing it.
//! Records with a problem (no text, a text that is not UTF-8, a meta that
//! does not decode, a text with no meta) are counted and skipped, so a
//! damaged store is reported by `check`, not silently rewritten.

use std::io::Write;
use std::iter::Peekable;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::error::Result;
use crate::features::{self, DropReason};
use crate::hash::Key;
use crate::meta::Meta;
use crate::stats::range_for;
use crate::store::{Keyspace, KvIter, Store};
use crate::util::{fmt_int, fmt_mb, fmt_num, fmt_rate};

pub const DEFAULT_BATCH: usize = 2000;
pub const PROGRESS_EVERY: u64 = 100_000;

#[derive(Debug, Clone)]
pub struct RecomputeOptions {
    pub threads: usize,
    /// Records per atomic `meta` write.
    pub batch_size: usize,
    /// Compute and count, write nothing.
    pub dry_run: bool,
    /// Rewrite every record, even one that is already up to date.
    pub force: bool,
    /// Print a progress line every N documents (0 = never).
    pub progress_every: u64,
}

impl Default for RecomputeOptions {
    fn default() -> Self {
        RecomputeOptions { threads: 4, batch_size: DEFAULT_BATCH, dry_run: false, force: false, progress_every: PROGRESS_EVERY }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// Documents with a `meta` record.
    pub docs: u64,
    /// Records rewritten (in a dry run: that would be). With `force`, every
    /// readable record counts here.
    pub updated: u64,
    /// Records already equal to a fresh computation and left alone.
    pub unchanged: u64,
    /// Documents today's prefilter would drop, by `DropReason` index; kept as they are.
    pub rejected: [u64; 7],
    /// `meta` without `text`.
    pub missing_text: u64,
    /// `text` without `meta`.
    pub orphan_text: u64,
    pub bad_utf8: u64,
    pub corrupt_meta: u64,
    /// Text bytes read.
    pub bytes: u64,
}

impl Counts {
    pub fn rejected_total(&self) -> u64 {
        self.rejected.iter().sum()
    }

    pub fn problems(&self) -> u64 {
        self.missing_text + self.orphan_text + self.bad_utf8 + self.corrupt_meta
    }

    /// Every document with a meta record lands in exactly one bucket.
    pub fn balanced(&self) -> bool {
        self.docs == self.updated + self.unchanged + self.rejected_total() + self.missing_text + self.bad_utf8 + self.corrupt_meta
    }

    fn add(&mut self, o: &Counts) {
        self.docs += o.docs;
        self.updated += o.updated;
        self.unchanged += o.unchanged;
        for (a, b) in self.rejected.iter_mut().zip(o.rejected.iter()) {
            *a += b;
        }
        self.missing_text += o.missing_text;
        self.orphan_text += o.orphan_text;
        self.bad_utf8 += o.bad_utf8;
        self.corrupt_meta += o.corrupt_meta;
        self.bytes += o.bytes;
    }
}

#[derive(Debug, Clone)]
pub struct RecomputeReport {
    pub counts: Counts,
    pub secs: f64,
    pub dry_run: bool,
    pub disk: u64,
}

impl RecomputeReport {
    pub fn render(&self) -> String {
        let c = &self.counts;
        let mut s =
            format!("Recomputed {} documents in {} s ({} of text)\n", fmt_int(c.docs), fmt_num(self.secs, 1), fmt_rate(c.bytes, self.secs));
        let verb = if self.dry_run { "would be rewritten" } else { "features rewritten" };
        s.push_str(&format!("  Updated:    {:>12}  ({verb})\n", fmt_int(c.updated)));
        s.push_str(&format!("  Unchanged:  {:>12}  (already what this build computes)\n", fmt_int(c.unchanged)));
        let reasons: Vec<String> = DropReason::ALL
            .iter()
            .filter(|r| c.rejected[r.index()] > 0)
            .map(|r| format!("{} {}", r.as_str(), fmt_int(c.rejected[r.index()])))
            .collect();
        let why = if reasons.is_empty() {
            "none; today's prefilter accepts every document".to_string()
        } else {
            format!("today's prefilter would drop them: {}; kept with their old features", reasons.join(", "))
        };
        s.push_str(&format!("  Rejected:   {:>12}  ({why})\n", fmt_int(c.rejected_total())));
        if c.problems() > 0 {
            s.push_str(&format!(
                "  Problems:   {:>12}  (missing text {}, orphan text {}, bad UTF-8 {}, corrupt meta {}; skipped, see `check`)\n",
                fmt_int(c.problems()),
                fmt_int(c.missing_text),
                fmt_int(c.orphan_text),
                fmt_int(c.bad_utf8),
                fmt_int(c.corrupt_meta)
            ));
        }
        if self.dry_run {
            s.push_str("Dry run: nothing was written.\n");
        } else {
            // The engine's document estimate is not printed: right after a
            // rewrite it counts old and new versions until compaction merges them.
            let hint = if c.updated > 0 { "; run `compact` to merge the rewritten tables" } else { "" };
            s.push_str(&format!("Store: {} on disk{hint}\n", fmt_mb(self.disk)));
        }
        s
    }
}

/// What a fresh computation says about one stored record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Unchanged,
    Updated(Meta),
    Rejected(DropReason),
    BadUtf8,
}

/// Recompute `text` against its stored record. The source, the flags and
/// the schema version of the fresh record come from the old one, so only
/// the features can differ.
pub fn refresh(old: &Meta, text: &[u8], force: bool, scratch: &mut features::Scratch) -> Outcome {
    let Ok(s) = std::str::from_utf8(text) else {
        return Outcome::BadUtf8;
    };
    match features::compute(s, features::DEFAULT_MAX_LEN, scratch) {
        Err(reason) => Outcome::Rejected(reason),
        Ok(f) => {
            let fresh = Meta { flags: old.flags, ..f.to_meta(old.source, old.has_extra()) };
            if fresh == *old && !force { Outcome::Unchanged } else { Outcome::Updated(fresh) }
        }
    }
}

/// Advance the text iterator to `key`: texts with smaller keys have no
/// meta (orphans, counted); an equal key yields its text.
fn take_text(it: &mut Peekable<KvIter<'_>>, key: &Key, counts: &mut Counts) -> Result<Option<Vec<u8>>> {
    loop {
        match it.peek() {
            None => return Ok(None),
            Some(Err(_)) => return Err(it.next().expect("peeked").expect_err("peeked an error")),
            Some(Ok((k, _))) => {
                if k < key {
                    it.next();
                    counts.orphan_text += 1;
                } else if k == key {
                    let (_, text) = it.next().expect("peeked").expect("checked");
                    return Ok(Some(text));
                } else {
                    return Ok(None);
                }
            }
        }
    }
}

/// Totals every range thread adds to, and the progress log they share.
struct Shared<'l> {
    docs: AtomicU64,
    updated: AtomicU64,
    bytes: AtomicU64,
    every: u64,
    started: Instant,
    log: Mutex<&'l mut (dyn Write + Send)>,
}

impl Shared<'_> {
    fn tick(&self, updated: bool, bytes: u64) -> Result<()> {
        if updated {
            self.updated.fetch_add(1, Ordering::Relaxed);
        }
        let bytes = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let docs = self.docs.fetch_add(1, Ordering::Relaxed) + 1;
        if self.every > 0 && docs.is_multiple_of(self.every) {
            let secs = self.started.elapsed().as_secs_f64();
            let updated = self.updated.load(Ordering::Relaxed);
            let mut log = self.log.lock().expect("log lock");
            writeln!(log, "  {} documents, {} updated, {}", fmt_int(docs), fmt_int(updated), fmt_rate(bytes, secs))?;
        }
        Ok(())
    }
}

fn recompute_range(store: &dyn Store, opts: &RecomputeOptions, lo: &Key, hi: Option<&Key>, shared: &Shared<'_>) -> Result<Counts> {
    let mut counts = Counts::default();
    let mut text_it = store.iter(Keyspace::Text, lo, hi, true)?.peekable();
    let mut scratch = features::Scratch::default();
    let mut batch: Vec<(Key, Vec<u8>)> = Vec::with_capacity(opts.batch_size.max(1));
    store.scan_meta(lo, hi, &mut |key, bytes| {
        counts.docs += 1;
        let Some(text) = take_text(&mut text_it, key, &mut counts)? else {
            counts.missing_text += 1;
            shared.tick(false, 0)?;
            return Ok(true);
        };
        counts.bytes += text.len() as u64;
        let mut updated = false;
        match Meta::decode(bytes) {
            Err(_) => counts.corrupt_meta += 1,
            Ok(old) => match refresh(&old, &text, opts.force, &mut scratch) {
                Outcome::Unchanged => counts.unchanged += 1,
                Outcome::Rejected(r) => counts.rejected[r.index()] += 1,
                Outcome::BadUtf8 => counts.bad_utf8 += 1,
                Outcome::Updated(fresh) => {
                    counts.updated += 1;
                    updated = true;
                    if !opts.dry_run {
                        batch.push((*key, fresh.encode()));
                        if batch.len() >= opts.batch_size.max(1) {
                            store.put_meta_batch(&batch)?;
                            batch.clear();
                        }
                    }
                }
            },
        }
        shared.tick(updated, text.len() as u64)?;
        Ok(true)
    })?;
    for item in text_it {
        item?;
        counts.orphan_text += 1;
    }
    if !batch.is_empty() {
        store.put_meta_batch(&batch)?;
    }
    Ok(counts)
}

/// Recompute every document's features. Progress goes to `log`.
pub fn recompute(store: &dyn Store, opts: &RecomputeOptions, log: &mut (dyn Write + Send)) -> Result<RecomputeReport> {
    let started = Instant::now();
    let threads = opts.threads.clamp(1, 256);
    let shared = Shared {
        docs: AtomicU64::new(0),
        updated: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        every: opts.progress_every,
        started,
        log: Mutex::new(log),
    };
    let results: Vec<Result<Counts>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .filter_map(|t| range_for(t, threads))
            .map(|(lo, hi)| {
                let shared = &shared;
                s.spawn(move || recompute_range(store, opts, &lo, hi.as_ref(), shared))
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("recompute thread panicked")).collect()
    });
    let mut counts = Counts::default();
    for r in results {
        counts.add(&r?);
    }
    if !opts.dry_run {
        store.persist()?;
    }
    Ok(RecomputeReport { counts, secs: started.elapsed().as_secs_f64(), dry_run: opts.dry_run, disk: store.disk_size()? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::doc_key;
    use crate::meta::FLAG_HAS_EXTRA;
    use crate::store::Record;

    const PROSE: &str = "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! \
How vexingly quick daft zebras jump. The five boxing wizards jump quickly.";

    fn current(text: &str, source: u16, has_extra: bool) -> Meta {
        features::compute(text, features::DEFAULT_MAX_LEN, &mut features::Scratch::default()).unwrap().to_meta(source, has_extra)
    }

    fn record(text: &str, meta: Meta, extra: Option<&str>) -> Record {
        Record {
            key: doc_key(text, &mut Vec::new()),
            meta: meta.encode(),
            text: text.as_bytes().to_vec(),
            extra: extra.map(|e| e.as_bytes().to_vec()),
            replace: false,
        }
    }

    /// A store with one document per outcome, plus the problem cases.
    fn damaged_store(dir: &std::path::Path) -> (Box<dyn Store>, Vec<(String, Key)>) {
        let store = crate::store::open(dir, true).unwrap();
        let stale_text = format!("A {PROSE}");
        let fine_text = format!("B {PROSE}");
        let short_text = "short text, under the prefilter's minimum".to_string();
        // stale: imported by an older build that had no entropy or quality, and counted words wrong
        let stale = Meta { words: 1, entropy: None, quality: None, flags: FLAG_HAS_EXTRA | 0b1000_0000, ..current(&stale_text, 3, true) };
        // short: today's prefilter rejects it; whatever it has stays
        let short_meta = Meta { words: 9, ..Default::default() };
        store
            .put_batch(&[
                record(&stale_text, stale, Some("{\"year\":1}")),
                record(&fine_text, current(&fine_text, 0, false), None),
                record(&short_text, short_meta, None),
            ])
            .unwrap();
        let keys = vec![
            ("stale".to_string(), doc_key(&stale_text, &mut Vec::new())),
            ("fine".to_string(), doc_key(&fine_text, &mut Vec::new())),
            ("short".to_string(), doc_key(&short_text, &mut Vec::new())),
        ];
        // problems: a meta without text, a text without meta, a text that is not UTF-8, a meta that does not decode
        store.put_raw(Keyspace::Meta, &[0x10; 16], Some(&Meta::default().encode())).unwrap();
        store.put_raw(Keyspace::Text, &[0x20; 16], Some(b"orphan text with no meta record at all")).unwrap();
        store.put_raw(Keyspace::Meta, &[0x30; 16], Some(&Meta::default().encode())).unwrap();
        store.put_raw(Keyspace::Text, &[0x30; 16], Some(b"\xff\xfe not utf-8")).unwrap();
        store.put_raw(Keyspace::Meta, &[0x40; 16], Some(b"\xff")).unwrap();
        store.put_raw(Keyspace::Text, &[0x40; 16], Some(PROSE.as_bytes())).unwrap();
        store.persist().unwrap();
        (store, keys)
    }

    #[test]
    fn rewrites_only_what_changed_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let (store, keys) = damaged_store(dir.path());
        let key = |name: &str| keys.iter().find(|(n, _)| n == name).unwrap().1;
        let meta_of = |k: &Key| Meta::decode(&store.get_meta(k).unwrap().unwrap()).unwrap();
        let before_short = meta_of(&key("short"));

        // dry run: counts everything, writes nothing
        let mut log = Vec::new();
        let opts = RecomputeOptions { threads: 3, batch_size: 1, dry_run: true, force: false, progress_every: 2 };
        let rep = recompute(store.as_ref(), &opts, &mut log).unwrap();
        let c = &rep.counts;
        assert!(c.balanced(), "{c:?}");
        assert_eq!((c.docs, c.updated, c.unchanged, c.rejected_total()), (6, 1, 1, 1), "{c:?}");
        assert_eq!((c.missing_text, c.orphan_text, c.bad_utf8, c.corrupt_meta), (1, 1, 1, 1), "{c:?}");
        assert_eq!(c.rejected[DropReason::TooShort.index()], 1);
        assert_eq!(meta_of(&key("stale")).words, 1, "a dry run writes nothing");
        let s = rep.render();
        assert!(s.contains("Recomputed 6 documents in"), "{s}");
        assert!(s.contains("Updated:               1  (would be rewritten)"), "{s}");
        assert!(s.contains("Rejected:              1  (today's prefilter would drop them: too_short 1; kept"), "{s}");
        assert!(s.contains("Problems:              4  (missing text 1, orphan text 1, bad UTF-8 1, corrupt meta 1"), "{s}");
        assert!(s.ends_with("Dry run: nothing was written.\n"), "{s}");
        let log = String::from_utf8(log).unwrap();
        assert!(log.contains(" documents, ") && log.contains(" updated, "), "progress every 2 documents: {log:?}");

        // the real thing, with single-record batches so writes land mid-scan
        let opts = RecomputeOptions { threads: 3, batch_size: 1, dry_run: false, force: false, progress_every: 0 };
        let mut log = Vec::new();
        let rep = recompute(store.as_ref(), &opts, &mut log).unwrap();
        assert_eq!((rep.counts.updated, rep.counts.unchanged), (1, 1));
        assert!(log.is_empty(), "progress_every 0 prints nothing");
        let s = rep.render();
        assert!(s.contains("Updated:               1  (features rewritten)"), "{s}");
        assert!(s.contains("Store: ") && s.contains(" on disk; run `compact` to merge the rewritten tables"), "{s}");
        let fixed = meta_of(&key("stale"));
        let expect = Meta { flags: FLAG_HAS_EXTRA | 0b1000_0000, ..current(&format!("A {PROSE}"), 3, true) };
        assert_eq!(fixed, expect, "features fresh, source and every flag bit kept");
        assert!(fixed.entropy.is_some() && fixed.words == 30);
        assert_eq!(store.get_extra(&key("stale")).unwrap().unwrap(), b"{\"year\":1}");
        assert_eq!(meta_of(&key("short")), before_short, "a rejected document keeps its record");
        assert_eq!(store.get_meta(&[0x40; 16]).unwrap().unwrap(), b"\xff", "a corrupt meta is left for check");

        // a second pass finds nothing to do
        let rep = recompute(store.as_ref(), &opts, &mut Vec::new()).unwrap();
        assert_eq!((rep.counts.updated, rep.counts.unchanged), (0, 2));
        assert!(!rep.render().contains("run `compact`"), "{}", rep.render());
        assert!(rep.render().contains("Rejected:              1"), "{}", rep.render());
        // unless forced, which rewrites the two readable records with the same values
        let forced = RecomputeOptions { force: true, ..opts };
        let rep = recompute(store.as_ref(), &forced, &mut Vec::new()).unwrap();
        assert_eq!((rep.counts.updated, rep.counts.unchanged, rep.counts.rejected_total()), (2, 0, 1));
        assert_eq!(meta_of(&key("stale")), expect);
        assert_eq!(meta_of(&key("short")), before_short);
    }

    #[test]
    fn refresh_outcomes() {
        let mut sc = features::Scratch::default();
        let old = current(PROSE, 2, false);
        assert_eq!(refresh(&old, PROSE.as_bytes(), false, &mut sc), Outcome::Unchanged);
        assert_eq!(refresh(&old, PROSE.as_bytes(), true, &mut sc), Outcome::Updated(old.clone()));
        let stale = Meta { sentences: 0, ..old.clone() };
        assert_eq!(refresh(&stale, PROSE.as_bytes(), false, &mut sc), Outcome::Updated(old.clone()));
        assert_eq!(refresh(&old, b"\xff", true, &mut sc), Outcome::BadUtf8);
        assert_eq!(refresh(&old, b"tiny", true, &mut sc), Outcome::Rejected(DropReason::TooShort));
        // a changed source or flags on the old record are kept, never "fixed"
        let moved = Meta { source: 9, flags: 0b0000_0010, ..old.clone() };
        assert_eq!(refresh(&moved, PROSE.as_bytes(), false, &mut sc), Outcome::Unchanged);
    }

    #[test]
    fn counts_and_report_edges() {
        let mut c = Counts { docs: 3, updated: 1, unchanged: 1, ..Default::default() };
        c.rejected[DropReason::FewWords.index()] = 1;
        assert!(c.balanced());
        let mut total = Counts::default();
        total.add(&c);
        total.add(&c);
        assert_eq!((total.docs, total.rejected_total()), (6, 2));
        c.missing_text = 1;
        assert!(!c.balanced());
        let clean =
            RecomputeReport { counts: Counts { docs: 2, unchanged: 2, ..Default::default() }, secs: 0.5, dry_run: false, disk: 1000 };
        let s = clean.render();
        assert!(s.contains("Rejected:              0  (none; today's prefilter accepts every document)"), "{s}");
        assert!(!s.contains("Problems:"), "{s}");
        assert!(s.contains("Store: 0.0 MB on disk\n"), "{s}");
        let empty = RecomputeReport { counts: Counts::default(), secs: 0.0, dry_run: true, disk: 0 }.render();
        assert!(empty.starts_with("Recomputed 0 documents in 0.0 s (n/a of text)"), "{empty}");
        assert_eq!(RecomputeOptions::default().batch_size, DEFAULT_BATCH);
    }

    #[test]
    fn empty_store_and_null_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::open(dir.path(), true).unwrap();
        let rep = recompute(store.as_ref(), &RecomputeOptions::default(), &mut Vec::new()).unwrap();
        assert_eq!(rep.counts, Counts::default());
        let null = crate::store::NullStore::default();
        let rep = recompute(&null, &RecomputeOptions { threads: 300, ..RecomputeOptions::default() }, &mut Vec::new()).unwrap();
        assert_eq!(rep.counts.docs, 0);
    }
}
