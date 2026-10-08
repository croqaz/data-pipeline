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
//!
//! `tokens` is counted with the store's tokenizer. `--tokenizer DIR` switches
//! it (see `src/tokenizer.rs`): the new one is recorded as pending, every
//! document is recounted, including the ones today's prefilter rejects
//! (their other features stay as they are), and only then does it become
//! the store's tokenizer. A plain `recompute` finishes a switch that stopped.

use std::io::Write;
use std::iter::Peekable;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::error::Result;
use crate::features::{self, DropReason};
use crate::hash::Key;
use crate::meta::Meta;
use crate::stats::range_for;
use crate::store::{Keyspace, KvIter, Store};
use crate::tokenizer::{self, Loaded, TokenizerSpec};
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
    /// `--tokenizer`: switch the store to this tokenizer folder.
    pub tokenizer: Option<PathBuf>,
}

impl Default for RecomputeOptions {
    fn default() -> Self {
        RecomputeOptions {
            threads: 4,
            batch_size: DEFAULT_BATCH,
            dry_run: false,
            force: false,
            progress_every: PROGRESS_EVERY,
            tokenizer: None,
        }
    }
}

/// What happened to the store's tokenizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenizerChange {
    /// No tokenizer: `tokens` stays empty.
    None,
    /// Counted with the store's tokenizer, which stays.
    Kept(String),
    /// Switched from the first (or from no tokenizer) to the second.
    Switched(Option<String>, String),
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
    pub tokenizer: TokenizerChange,
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
        match &self.tokenizer {
            TokenizerChange::None => {}
            TokenizerChange::Kept(t) => s.push_str(&format!("Tokens: counted with {t}, the store's tokenizer\n")),
            TokenizerChange::Switched(from, to) => {
                let from = from.as_deref().unwrap_or("no tokenizer");
                let verb = if self.dry_run { "would switch" } else { "switched" };
                s.push_str(&format!("Tokenizer: {verb} from {from} to {to}; every document recounted\n"));
            }
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
    /// Rejected, but `tokens` is recounted with the store's tokenizer; every
    /// other feature stays as it was.
    RejectedRecounted(DropReason, Meta),
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
        Err(reason) => match features::count_tokens(s, scratch) {
            Some(n) if old.tokens != Some(n) => Outcome::RejectedRecounted(reason, Meta { tokens: Some(n), ..old.clone() }),
            _ => Outcome::Rejected(reason),
        },
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

fn recompute_range(
    store: &dyn Store,
    opts: &RecomputeOptions,
    tok: Option<&Loaded>,
    lo: &Key,
    hi: Option<&Key>,
    shared: &Shared<'_>,
) -> Result<Counts> {
    let mut counts = Counts::default();
    let mut text_it = store.iter(Keyspace::Text, lo, hi, true)?.peekable();
    let mut scratch = features::Scratch { tokenizer: tok.map(|t| t.tokenizer.clone()), ..features::Scratch::default() };
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
                Outcome::RejectedRecounted(r, fresh) => {
                    counts.rejected[r.index()] += 1;
                    if !opts.dry_run {
                        batch.push((*key, fresh.encode()));
                    }
                }
                Outcome::Updated(fresh) => {
                    counts.updated += 1;
                    updated = true;
                    if !opts.dry_run {
                        batch.push((*key, fresh.encode()));
                    }
                }
            },
        }
        if batch.len() >= opts.batch_size.max(1) {
            store.put_meta_batch(&batch)?;
            batch.clear();
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

/// Which tokenizer this recompute counts with, and whether that is a switch.
fn plan(state: &tokenizer::State, given: Option<Loaded>) -> Result<(Option<Loaded>, bool)> {
    Ok(match (given, &state.pending, &state.current) {
        // A switch, unless it names the tokenizer the store already has and
        // no other switch is half done.
        (Some(new), pending, current) => {
            let same = pending.is_none() && current.as_ref().is_some_and(|c| c.same_as(&new.spec));
            (Some(new), !same)
        }
        // Finish the switch that stopped.
        (None, Some(pending), _) => (Some(pending.load()?), true),
        (None, None, Some(current)) => (Some(current.load()?), false),
        (None, None, None) => (None, false),
    })
}

/// Recompute every document's features. Progress goes to `log`.
pub fn recompute(store: &dyn Store, opts: &RecomputeOptions, log: &mut (dyn Write + Send)) -> Result<RecomputeReport> {
    let started = Instant::now();
    let threads = opts.threads.clamp(1, 256);
    let state = tokenizer::State::read(store)?;
    let given = opts.tokenizer.as_deref().map(TokenizerSpec::from_path).transpose()?;
    let (tok, switching) = plan(&state, given)?;
    let change = match (&tok, switching) {
        (None, _) => TokenizerChange::None,
        (Some(t), false) => TokenizerChange::Kept(t.spec.label()),
        (Some(t), true) => TokenizerChange::Switched(state.current.as_ref().map(TokenizerSpec::label), t.spec.label()),
    };
    if let TokenizerChange::Switched(from, to) = &change {
        let resume = if state.pending.is_some() { " (finishing a switch that stopped)" } else { "" };
        let from = from.as_deref().unwrap_or("no tokenizer");
        writeln!(log, "Switching the tokenizer from {from} to {to}{resume}: every document is recounted")?;
        if !opts.dry_run {
            // Recorded before the first rewrite: if this run stops, the store
            // knows its counts are mixed and which tokenizer to finish with.
            let t = tok.as_ref().expect("a switch has a target");
            store.put_setting(tokenizer::PENDING, Some(&t.spec.encode()))?;
            store.persist()?;
        }
    }
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
                let tok = tok.as_ref();
                s.spawn(move || recompute_range(store, opts, tok, &lo, hi.as_ref(), shared))
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
        if switching {
            // Every document is recounted and durable: the switch is done.
            let t = tok.as_ref().expect("a switch has a target");
            store.put_setting(tokenizer::CURRENT, Some(&t.spec.encode()))?;
            store.put_setting(tokenizer::PENDING, None)?;
            store.persist()?;
        }
    }
    Ok(RecomputeReport {
        counts,
        secs: started.elapsed().as_secs_f64(),
        dry_run: opts.dry_run,
        disk: store.disk_size()?,
        tokenizer: change,
    })
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
        let opts = RecomputeOptions { threads: 3, batch_size: 1, dry_run: true, force: false, progress_every: 2, tokenizer: None };
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
        let opts = RecomputeOptions { threads: 3, batch_size: 1, dry_run: false, force: false, progress_every: 0, tokenizer: None };
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
        let clean = RecomputeReport {
            counts: Counts { docs: 2, unchanged: 2, ..Default::default() },
            secs: 0.5,
            dry_run: false,
            disk: 1000,
            tokenizer: TokenizerChange::None,
        };
        let s = clean.render();
        assert!(s.contains("Rejected:              0  (none; today's prefilter accepts every document)"), "{s}");
        assert!(!s.contains("Problems:"), "{s}");
        assert!(s.contains("Store: 0.0 MB on disk\n"), "{s}");
        let empty =
            RecomputeReport { counts: Counts::default(), secs: 0.0, dry_run: true, disk: 0, tokenizer: TokenizerChange::None }.render();
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

    // -----------------------------------------------------------------------
    // Tokenizer switches
    // -----------------------------------------------------------------------

    use crate::check::{self, CheckOptions, Problem};
    use crate::store::Sources;
    use crate::tokenizer::tests::{fixture, loaded};
    use crate::tokenizer::{CURRENT, PENDING, State};

    const TEXTS: [&str; 3] = ["A ", "B ", "[1] "];
    const SHORT: &str = "short text, under the prefilter's minimum";
    // HF `tokenizers` counts, as in `tokenizer::tests`.
    const COUNTS_A: [u32; 3] = [139, 139, 141];
    const COUNTS_B: [u32; 3] = [129, 129, 131];
    const SHORT_A: u32 = 31;
    const SHORT_B: u32 = 28;

    fn text(i: usize) -> String {
        format!("{}{PROSE}", TEXTS[i])
    }

    fn counted_with(text: &str, tok: Option<&str>) -> Meta {
        let mut sc = features::Scratch { tokenizer: tok.map(|t| loaded(t).tokenizer), ..features::Scratch::default() };
        features::compute(text, features::DEFAULT_MAX_LEN, &mut sc).unwrap().to_meta(0, false)
    }

    /// Three documents and one the prefilter rejects, all counted with `tok`,
    /// which is the store's tokenizer.
    fn tokenized_store(dir: &std::path::Path, tok: Option<&str>) -> Box<dyn Store> {
        let store = crate::store::open(dir, true).unwrap();
        let mut recs: Vec<Record> = (0..3).map(|i| record(&text(i), counted_with(&text(i), tok), None)).collect();
        let short_tokens = tok.map(|t| if t == "tok-a" { SHORT_A } else { SHORT_B });
        recs.push(record(SHORT, Meta { words: 9, tokens: short_tokens, ..Default::default() }, None));
        store.put_batch(&recs).unwrap();
        store.put_source(0, "cli").unwrap();
        if let Some(t) = tok {
            store.put_setting(CURRENT, Some(&loaded(t).spec.encode())).unwrap();
        }
        store.persist().unwrap();
        store
    }

    fn tokens(store: &dyn Store) -> (Vec<Option<u32>>, Option<u32>) {
        let get = |t: &str| Meta::decode(&store.get_meta(&doc_key(t, &mut Vec::new())).unwrap().unwrap()).unwrap();
        ((0..3).map(|i| get(&text(i)).tokens).collect(), get(SHORT).tokens)
    }

    fn some(c: [u32; 3]) -> Vec<Option<u32>> {
        c.iter().map(|&n| Some(n)).collect()
    }

    fn names(store: &dyn Store) -> (Option<String>, Option<String>) {
        let s = State::read(store).unwrap();
        (s.current.map(|t| t.name), s.pending.map(|t| t.name))
    }

    fn switch_to(tok: Option<&str>, dry_run: bool) -> RecomputeOptions {
        RecomputeOptions {
            threads: 2,
            batch_size: 1,
            dry_run,
            progress_every: 0,
            tokenizer: tok.map(fixture),
            ..RecomputeOptions::default()
        }
    }

    fn deep_check(store: &dyn Store) -> check::CheckReport {
        let opts = CheckOptions { deep: true, threads: 2, examples: 5 };
        check::check(store, &Sources::load(store).unwrap(), &opts).unwrap()
    }

    #[test]
    fn switch_recounts_every_document_then_records_the_tokenizer() {
        let dir = tempfile::tempdir().unwrap();
        let store = tokenized_store(dir.path(), Some("tok-a"));
        let (a, b) = (loaded("tok-a").spec.label(), loaded("tok-b").spec.label());

        // a dry run says what would happen and touches nothing, settings included
        let mut log = Vec::new();
        let rep = recompute(store.as_ref(), &switch_to(Some("tok-b"), true), &mut log).unwrap();
        assert_eq!(rep.tokenizer, TokenizerChange::Switched(Some(a.clone()), b.clone()));
        assert_eq!((rep.counts.updated, rep.counts.rejected_total()), (3, 1));
        assert!(rep.render().contains(&format!("Tokenizer: would switch from {a} to {b}; every document recounted")));
        assert!(String::from_utf8(log).unwrap().contains(&format!("Switching the tokenizer from {a} to {b}: every document is recounted")));
        assert_eq!(tokens(store.as_ref()), (some(COUNTS_A), Some(SHORT_A)));
        assert_eq!(names(store.as_ref()), (Some("tok-a".into()), None));

        // the switch: every document recounted, the rejected one too (its
        // other features kept), then the store's tokenizer replaced
        let rep = recompute(store.as_ref(), &switch_to(Some("tok-b"), false), &mut Vec::new()).unwrap();
        assert_eq!(rep.tokenizer, TokenizerChange::Switched(Some(a.clone()), b.clone()));
        assert!(rep.render().contains(&format!("Tokenizer: switched from {a} to {b}; every document recounted")));
        assert!(rep.counts.balanced());
        assert_eq!((rep.counts.updated, rep.counts.rejected_total()), (3, 1));
        assert_eq!(tokens(store.as_ref()), (some(COUNTS_B), Some(SHORT_B)));
        let short = Meta::decode(&store.get_meta(&doc_key(SHORT, &mut Vec::new())).unwrap().unwrap()).unwrap();
        assert_eq!(short, Meta { words: 9, tokens: Some(SHORT_B), ..Default::default() });
        assert_eq!(names(store.as_ref()), (Some("tok-b".into()), None));

        // afterwards a plain recompute keeps it and has nothing to do
        let rep = recompute(store.as_ref(), &switch_to(None, false), &mut Vec::new()).unwrap();
        assert_eq!(rep.tokenizer, TokenizerChange::Kept(b.clone()));
        assert_eq!((rep.counts.updated, rep.counts.unchanged), (0, 3));
        assert!(rep.render().contains(&format!("Tokens: counted with {b}, the store's tokenizer")));
        let deep = deep_check(store.as_ref());
        // the rejected document is the only stale one, as before tokenizers
        assert_eq!((deep.unfinished_switch.as_deref(), deep.problems()), (None, 1), "{}", deep.render());
    }

    #[test]
    fn the_same_tokenizer_is_not_a_switch() {
        let dir = tempfile::tempdir().unwrap();
        let store = tokenized_store(dir.path(), Some("tok-a"));
        let rep = recompute(store.as_ref(), &switch_to(Some("tok-a"), false), &mut Vec::new()).unwrap();
        assert_eq!(rep.tokenizer, TokenizerChange::Kept(loaded("tok-a").spec.label()));
        assert_eq!((rep.counts.updated, rep.counts.unchanged), (0, 3));
        assert_eq!(store.setting(PENDING).unwrap(), None, "no pending marker was ever needed");
        assert_eq!(names(store.as_ref()), (Some("tok-a".into()), None));
    }

    #[test]
    fn stores_without_a_tokenizer_stay_that_way_until_asked() {
        let dir = tempfile::tempdir().unwrap();
        let store = tokenized_store(dir.path(), None);
        let rep = recompute(store.as_ref(), &switch_to(None, false), &mut Vec::new()).unwrap();
        assert_eq!((rep.tokenizer.clone(), rep.counts.updated), (TokenizerChange::None, 0));
        assert!(!rep.render().contains("Token"), "{}", rep.render());
        assert_eq!(tokens(store.as_ref()), (vec![None; 3], None));
        assert_eq!(names(store.as_ref()), (None, None));

        // switching from none counts everything and sets it
        let rep = recompute(store.as_ref(), &switch_to(Some("tok-a"), false), &mut Vec::new()).unwrap();
        assert_eq!(rep.tokenizer, TokenizerChange::Switched(None, loaded("tok-a").spec.label()));
        assert!(rep.render().contains("switched from no tokenizer to tok-a"), "{}", rep.render());
        assert_eq!(tokens(store.as_ref()), (some(COUNTS_A), Some(SHORT_A)));
        assert_eq!(names(store.as_ref()), (Some("tok-a".into()), None));
    }

    /// A switch to tok-b that stopped after rewriting one document.
    fn interrupted_store(dir: &std::path::Path) -> Box<dyn Store> {
        let store = tokenized_store(dir, Some("tok-a"));
        store.put_setting(PENDING, Some(&loaded("tok-b").spec.encode())).unwrap();
        let t = text(0);
        store.put_meta_batch(&[(doc_key(&t, &mut Vec::new()), counted_with(&t, Some("tok-b")).encode())]).unwrap();
        store.persist().unwrap();
        store
    }

    #[test]
    fn an_interrupted_switch_is_reported_then_finished() {
        let dir = tempfile::tempdir().unwrap();
        let store = interrupted_store(dir.path());
        assert_eq!(tokens(store.as_ref()).0, vec![Some(COUNTS_B[0]), Some(COUNTS_A[1]), Some(COUNTS_A[2])]);

        // check names the unfinished switch; the documents it did not reach are stale
        let deep = deep_check(store.as_ref());
        let b = loaded("tok-b").spec.label();
        assert_eq!(deep.unfinished_switch.as_deref(), Some(b.as_str()));
        assert_eq!(deep.acc.counts[Problem::ALL.iter().position(|p| *p == Problem::StaleFeatures).unwrap()], 3);
        assert!(deep.render().contains(&format!("unfinished tokenizer switch to {b}: some documents are counted with each tokenizer")));
        let shallow = check::check(store.as_ref(), &Sources::load(store.as_ref()).unwrap(), &CheckOptions::default()).unwrap();
        assert_eq!(shallow.problems(), 1, "reported without --deep too");
        // stats warns that the total mixes two tokenizers
        let cfg = crate::stats::ScanConfig { filter: None, threads: 1, limit: None };
        let rep = crate::stats::collect(store.as_ref(), &Sources::load(store.as_ref()).unwrap(), &cfg).unwrap();
        assert!(rep.mixed_tokens);
        assert!(crate::stats::render(&rep, &Sources::default(), None).contains("(mixed: a tokenizer switch did not finish"));

        // a plain recompute finishes it
        let mut log = Vec::new();
        let rep = recompute(store.as_ref(), &switch_to(None, false), &mut log).unwrap();
        assert_eq!(rep.tokenizer, TokenizerChange::Switched(Some(loaded("tok-a").spec.label()), b.clone()));
        assert!(String::from_utf8(log).unwrap().contains("(finishing a switch that stopped)"));
        assert_eq!(tokens(store.as_ref()), (some(COUNTS_B), Some(SHORT_B)));
        assert_eq!(names(store.as_ref()), (Some("tok-b".into()), None));
        let deep = deep_check(store.as_ref());
        assert_eq!((deep.unfinished_switch.clone(), deep.problems()), (None, 1), "only the rejected document, as always");
    }

    #[test]
    fn an_interrupted_switch_can_go_back_instead() {
        let dir = tempfile::tempdir().unwrap();
        let store = interrupted_store(dir.path());
        let rep = recompute(store.as_ref(), &switch_to(Some("tok-a"), false), &mut Vec::new()).unwrap();
        assert!(matches!(rep.tokenizer, TokenizerChange::Switched(..)), "a pending switch makes even the old tokenizer a switch");
        assert_eq!(tokens(store.as_ref()), (some(COUNTS_A), Some(SHORT_A)));
        assert_eq!(names(store.as_ref()), (Some("tok-a".into()), None));
    }

    #[test]
    fn a_dry_run_leaves_an_interrupted_switch_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let store = interrupted_store(dir.path());
        recompute(store.as_ref(), &switch_to(None, true), &mut Vec::new()).unwrap();
        assert_eq!(names(store.as_ref()), (Some("tok-a".into()), Some("tok-b".into())));
        assert_eq!(tokens(store.as_ref()).0[1], Some(COUNTS_A[1]));
    }

    /// A store whose meta rewrites fail after `ok` batches.
    struct Failing {
        inner: Box<dyn Store>,
        ok: std::sync::atomic::AtomicUsize,
    }

    impl Store for Failing {
        fn put_meta_batch(&self, items: &[(Key, Vec<u8>)]) -> Result<()> {
            if self.ok.fetch_sub(1, Ordering::SeqCst) == 0 {
                return Err(crate::error::Error::Store("disk full".into()));
            }
            self.inner.put_meta_batch(items)
        }
        fn put_batch(&self, r: &[Record]) -> Result<()> {
            self.inner.put_batch(r)
        }
        fn get_meta(&self, k: &Key) -> Result<Option<Vec<u8>>> {
            self.inner.get_meta(k)
        }
        fn get_text(&self, k: &Key) -> Result<Option<Vec<u8>>> {
            self.inner.get_text(k)
        }
        fn get_extra(&self, k: &Key) -> Result<Option<Vec<u8>>> {
            self.inner.get_extra(k)
        }
        fn scan_meta(&self, lo: &Key, hi: Option<&Key>, f: &mut crate::store::ScanFn<'_>) -> Result<()> {
            self.inner.scan_meta(lo, hi, f)
        }
        fn iter<'a>(&'a self, ks: Keyspace, lo: &Key, hi: Option<&Key>, v: bool) -> Result<KvIter<'a>> {
            self.inner.iter(ks, lo, hi, v)
        }
        fn iter_rev<'a>(&'a self, ks: Keyspace, lo: &Key, hi: Option<&Key>, v: bool) -> Result<KvIter<'a>> {
            self.inner.iter_rev(ks, lo, hi, v)
        }
        fn put_raw(&self, ks: Keyspace, k: &Key, v: Option<&[u8]>) -> Result<()> {
            self.inner.put_raw(ks, k, v)
        }
        fn delete_batch(&self, k: &[Key]) -> Result<()> {
            self.inner.delete_batch(k)
        }
        fn compact(&self) -> Result<()> {
            self.inner.compact()
        }
        fn details(&self) -> Result<Vec<(String, String)>> {
            self.inner.details()
        }
        fn sources(&self) -> Result<Vec<(u16, String)>> {
            self.inner.sources()
        }
        fn put_source(&self, id: u16, l: &str) -> Result<()> {
            self.inner.put_source(id, l)
        }
        fn delete_source(&self, id: u16) -> Result<()> {
            self.inner.delete_source(id)
        }
        fn setting(&self, n: &str) -> Result<Option<Vec<u8>>> {
            self.inner.setting(n)
        }
        fn put_setting(&self, n: &str, v: Option<&[u8]>) -> Result<()> {
            self.inner.put_setting(n, v)
        }
        fn persist(&self) -> Result<()> {
            self.inner.persist()
        }
        fn disk_size(&self) -> Result<u64> {
            self.inner.disk_size()
        }
        fn approx_count(&self) -> Result<u64> {
            self.inner.approx_count()
        }
    }

    #[test]
    fn a_switch_that_fails_midway_stays_pending_and_can_be_finished() {
        let dir = tempfile::tempdir().unwrap();
        let store = Failing { inner: tokenized_store(dir.path(), Some("tok-a")), ok: std::sync::atomic::AtomicUsize::new(1) };
        let opts = RecomputeOptions { threads: 1, ..switch_to(Some("tok-b"), false) };
        let e = recompute(&store, &opts, &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("disk full"), "{e}");
        // one document was rewritten, the switch is on record as unfinished
        assert_eq!(names(&store), (Some("tok-a".into()), Some("tok-b".into())));
        let (counts, _) = tokens(&store);
        assert_eq!(counts.iter().filter(|c| COUNTS_B.contains(&c.unwrap()) && !COUNTS_A.contains(&c.unwrap())).count(), 1, "{counts:?}");
        // imports refuse to add to a store with mixed counts
        let state = State::read(&store).unwrap();
        assert!(crate::tokenizer::for_import(std::path::Path::new("db"), &state, None, 4).is_err());
        // once writes work again, a plain recompute finishes the switch
        store.ok.store(usize::MAX / 2, Ordering::SeqCst);
        recompute(&store, &switch_to(None, false), &mut Vec::new()).unwrap();
        assert_eq!(tokens(&store), (some(COUNTS_B), Some(SHORT_B)));
        assert_eq!(names(&store), (Some("tok-b".into()), None));
    }
}
