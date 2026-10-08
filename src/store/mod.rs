//! The storage layer: one trait and one engine, fjall, chosen after the
//! 20 GB bake-off of 2026-10-05 (see BENCHMARKS.md). A marker file names
//! the engine so a directory is recognised as a pipeline store.
//!
//! Every document lives in three keyspaces under the same 16-byte key:
//! `meta` (CBOR features), `text` (raw UTF-8) and `extra` (raw JSON object of
//! the metadata fields that rode along, absent when there were none). Two
//! small keyspaces hold the interned source labels and the store-wide
//! settings (the tokenizer `tokens` is counted with).

pub mod fjall_store;
pub mod null_store;

pub use null_store::NullStore;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::hash::Key;

pub const MARKER_FILE: &str = "pipeline.backend";
/// What the marker file says. Stores written while other engines existed
/// carry the same word, so they keep opening.
pub const ENGINE: &str = "fjall";

/// One document ready to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: Key,
    pub meta: Vec<u8>,
    pub text: Vec<u8>,
    pub extra: Option<Vec<u8>>,
    /// This record overwrites a stored document, so a stale `extra` must be
    /// removed when the new one has none. A fresh document never writes to
    /// `extra` unless it has metadata: no tombstone per document.
    pub replace: bool,
}

/// Callback for scans; return `Ok(false)` to stop early.
pub type ScanFn<'a> = dyn FnMut(&Key, &[u8]) -> Result<bool> + 'a;

/// The three per-document keyspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyspace {
    Meta,
    Text,
    Extra,
}

impl Keyspace {
    pub const ALL: [Keyspace; 3] = [Keyspace::Meta, Keyspace::Text, Keyspace::Extra];

    pub fn as_str(self) -> &'static str {
        match self {
            Keyspace::Meta => "meta",
            Keyspace::Text => "text",
            Keyspace::Extra => "extra",
        }
    }
}

/// A pull iterator over one keyspace in key order.
pub type KvIter<'a> = Box<dyn Iterator<Item = Result<(Key, Vec<u8>)>> + 'a>;

pub trait Store: Send + Sync {
    /// Write a batch atomically across keyspaces.
    fn put_batch(&self, records: &[Record]) -> Result<()>;
    fn get_meta(&self, key: &Key) -> Result<Option<Vec<u8>>>;
    fn get_text(&self, key: &Key) -> Result<Option<Vec<u8>>>;
    fn get_extra(&self, key: &Key) -> Result<Option<Vec<u8>>>;
    /// Scan `meta` for keys in `[lo, hi)` (`hi == None` means to the end).
    fn scan_meta(&self, lo: &Key, hi: Option<&Key>, f: &mut ScanFn<'_>) -> Result<()>;
    /// Iterate one keyspace over `[lo, hi)`. With `values == false` every
    /// value is empty and the engine skips reading it where it can.
    fn iter<'a>(&'a self, ks: Keyspace, lo: &Key, hi: Option<&Key>, values: bool) -> Result<KvIter<'a>>;
    /// [`Store::iter`] walked backwards: the same `[lo, hi)`, highest key first.
    fn iter_rev<'a>(&'a self, ks: Keyspace, lo: &Key, hi: Option<&Key>, values: bool) -> Result<KvIter<'a>>;
    /// Write (`Some`) or delete (`None`) one value in one keyspace. A
    /// low-level primitive for tests and repairs; `put_batch` is the
    /// consistent way to add a document.
    fn put_raw(&self, ks: Keyspace, key: &Key, value: Option<&[u8]>) -> Result<()>;
    /// Replace the `meta` record of existing documents, atomically per
    /// call. What `recompute` writes; `text` and `extra` stay as they are.
    fn put_meta_batch(&self, items: &[(Key, Vec<u8>)]) -> Result<()>;
    /// Remove documents from every keyspace, atomically per call. Keys
    /// that do not exist are ignored.
    fn delete_batch(&self, keys: &[Key]) -> Result<()>;
    /// Rewrite every keyspace into its most compact form and wait for it.
    fn compact(&self) -> Result<()>;
    /// Engine facts for `info`, as (label, value) pairs.
    fn details(&self) -> Result<Vec<(String, String)>>;
    /// All stored source labels.
    fn sources(&self) -> Result<Vec<(u16, String)>>;
    fn put_source(&self, id: u16, label: &str) -> Result<()>;
    fn delete_source(&self, id: u16) -> Result<()>;
    /// A store-wide setting, by name.
    fn setting(&self, name: &str) -> Result<Option<Vec<u8>>>;
    /// Write (`Some`) or remove (`None`) a store-wide setting.
    fn put_setting(&self, name: &str, value: Option<&[u8]>) -> Result<()>;
    /// Make everything written so far durable.
    fn persist(&self) -> Result<()>;
    fn disk_size(&self) -> Result<u64>;
    /// Cheap estimate of the document count.
    fn approx_count(&self) -> Result<u64>;
}

/// In-memory mirror of the `sources` keyspace with interning.
#[derive(Debug, Default, Clone)]
pub struct Sources {
    by_name: HashMap<String, u16>,
    names: Vec<Option<String>>,
}

impl Sources {
    pub fn load(store: &dyn Store) -> Result<Sources> {
        let mut s = Sources::default();
        for (id, name) in store.sources()? {
            s.set(id, name);
        }
        Ok(s)
    }

    fn set(&mut self, id: u16, name: String) {
        let idx = id as usize;
        if self.names.len() <= idx {
            self.names.resize(idx + 1, None);
        }
        self.by_name.insert(name.clone(), id);
        self.names[idx] = Some(name);
    }

    pub fn name(&self, id: u16) -> &str {
        self.names.get(id as usize).and_then(|n| n.as_deref()).unwrap_or("?")
    }

    pub fn id(&self, name: &str) -> Option<u16> {
        self.by_name.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Id for `name`, allocating and persisting a new one when needed.
    pub fn intern(&mut self, name: &str, store: &dyn Store) -> Result<u16> {
        if let Some(id) = self.by_name.get(name) {
            return Ok(*id);
        }
        let id = u16::try_from(self.names.len()).map_err(|_| Error::Store("more than 65535 distinct sources".into()))?;
        store.put_source(id, name)?;
        self.set(id, name.to_string());
        Ok(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u16, &str)> {
        self.names.iter().enumerate().filter_map(|(i, n)| n.as_deref().map(|n| (i as u16, n)))
    }
}

/// Remove source names that no stored document uses any more (after a
/// relabel or a delete) and return them, sorted. Reads every meta record.
pub fn prune_sources(store: &dyn Store) -> Result<Vec<String>> {
    if store.sources()?.is_empty() {
        return Ok(Vec::new());
    }
    let mut used = Vec::new();
    store.scan_meta(&[0; 16], None, &mut |_, v| {
        mark_used(&mut used, crate::meta::Meta::decode(v)?.source);
        Ok(true)
    })?;
    remove_unused_sources(store, &used)
}

/// Record that a document uses source `id`; `used` grows as needed.
pub fn mark_used(used: &mut Vec<bool>, id: u16) {
    let i = id as usize;
    if used.len() <= i {
        used.resize(i + 1, false);
    }
    used[i] = true;
}

/// Remove every source name whose id is not marked in `used`; return the
/// removed names, sorted.
pub fn remove_unused_sources(store: &dyn Store, used: &[bool]) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for (id, name) in store.sources()? {
        if !used.get(id as usize).copied().unwrap_or(false) {
            store.delete_source(id)?;
            removed.push(name);
        }
    }
    if !removed.is_empty() {
        store.persist()?;
    }
    removed.sort();
    Ok(removed)
}

/// Is `dir` a pipeline store, or a place one may be created?
/// A missing or empty directory is new (allowed only with `create`); an
/// existing store must carry the marker with the right engine name.
pub fn check_dir(dir: &Path, create: bool) -> Result<()> {
    let path = dir.join(MARKER_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) if s.trim() == ENGINE => Ok(()),
        Ok(s) => {
            Err(Error::Store(format!("{} names an unknown engine {:?}; this build only reads {ENGINE} stores", path.display(), s.trim())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let empty = !dir.is_dir() || std::fs::read_dir(dir)?.next().is_none();
            if !empty {
                return Err(Error::Usage(format!("{} exists but is not a pipeline store (no {MARKER_FILE})", dir.display())));
            }
            if !create {
                return Err(Error::Usage(format!("{} is not a pipeline store", dir.display())));
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Open (or create, when `create` is set) the store at `dir`.
pub fn open(dir: &Path, create: bool) -> Result<Box<dyn Store>> {
    check_dir(dir, create)?;
    if create {
        std::fs::create_dir_all(dir)?;
    }
    let store: Box<dyn Store> = Box::new(fjall_store::FjallStore::open(dir)?);
    let marker = dir.join(MARKER_FILE);
    if !marker.exists() {
        std::fs::write(&marker, format!("{ENGINE}\n"))?;
    }
    Ok(store)
}

/// Path helper used by tests and benches.
pub fn marker_path(dir: &Path) -> PathBuf {
    dir.join(MARKER_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn rec(byte: u8, words: u32, extra: Option<&str>) -> Record {
        let meta = crate::meta::Meta { words, ..Default::default() }.encode();
        Record {
            key: [byte; 16],
            meta,
            text: format!("text {byte}").into_bytes(),
            extra: extra.map(|s| s.as_bytes().to_vec()),
            replace: false,
        }
    }

    /// The behavioural contract the store must satisfy.
    pub(crate) fn exercise(store: &dyn Store) {
        assert_eq!(store.approx_count().unwrap(), 0);
        assert!(store.get_meta(&[1; 16]).unwrap().is_none());
        store.put_batch(&[rec(1, 10, None), rec(3, 30, Some("{\"a\":1}")), rec(2, 20, None)]).unwrap();
        store.put_batch(&[]).unwrap();
        assert_eq!(store.get_text(&[2; 16]).unwrap().unwrap(), b"text 2");
        assert_eq!(store.get_extra(&[3; 16]).unwrap().unwrap(), b"{\"a\":1}");
        assert!(store.get_extra(&[1; 16]).unwrap().is_none());
        assert!(store.get_text(&[9; 16]).unwrap().is_none());
        let meta = crate::meta::Meta::decode(&store.get_meta(&[3; 16]).unwrap().unwrap()).unwrap();
        assert_eq!(meta.words, 30);

        // full scan in key order
        let mut seen = Vec::new();
        store
            .scan_meta(&[0; 16], None, &mut |k, v| {
                seen.push((k[0], crate::meta::Meta::decode(v).unwrap().words));
                Ok(true)
            })
            .unwrap();
        assert_eq!(seen, vec![(1, 10), (2, 20), (3, 30)]);
        // bounded scan [2, 3)
        let mut seen = Vec::new();
        store
            .scan_meta(&[2; 16], Some(&[3; 16]), &mut |k, _| {
                seen.push(k[0]);
                Ok(true)
            })
            .unwrap();
        assert_eq!(seen, vec![2]);
        // early stop
        let mut n = 0;
        store
            .scan_meta(&[0; 16], None, &mut |_, _| {
                n += 1;
                Ok(false)
            })
            .unwrap();
        assert_eq!(n, 1);
        // callback errors propagate
        assert!(store.scan_meta(&[0; 16], None, &mut |_, _| Err(Error::Store("stop".into()))).is_err());

        // overwrite keeps the latest
        store.put_batch(&[rec(2, 99, Some("{}"))]).unwrap();
        assert_eq!(crate::meta::Meta::decode(&store.get_meta(&[2; 16]).unwrap().unwrap()).unwrap().words, 99);
        assert_eq!(store.get_extra(&[2; 16]).unwrap().unwrap(), b"{}");
        // without `replace`, a record with no extra writes nothing to `extra`
        store.put_batch(&[rec(2, 100, None)]).unwrap();
        assert_eq!(store.get_extra(&[2; 16]).unwrap().unwrap(), b"{}");
        // with it, the stale extra goes
        store.put_batch(&[Record { replace: true, ..rec(2, 101, None) }]).unwrap();
        assert!(store.get_extra(&[2; 16]).unwrap().is_none());
        assert_eq!(crate::meta::Meta::decode(&store.get_meta(&[2; 16]).unwrap().unwrap()).unwrap().words, 101);

        // sources
        assert!(store.sources().unwrap().is_empty());
        store.put_source(0, "cli").unwrap();
        store.put_source(1, "gutenberg").unwrap();
        assert_eq!(store.sources().unwrap(), vec![(0, "cli".to_string()), (1, "gutenberg".to_string())]);

        store.persist().unwrap();
        assert!(store.disk_size().unwrap() > 0);
        let approx = store.approx_count().unwrap();
        assert!((1..=10).contains(&approx), "approx {approx}");

        // pull iterators, with and without values, bounded and unbounded
        let keys = |ks: Keyspace, lo: u8, hi: Option<u8>, values: bool| -> Vec<(u8, Vec<u8>)> {
            let hi = hi.map(|h| [h; 16]);
            store.iter(ks, &[lo; 16], hi.as_ref(), values).unwrap().map(|r| r.unwrap()).map(|(k, v)| (k[0], v)).collect()
        };
        assert_eq!(keys(Keyspace::Text, 0, None, true), vec![(1, b"text 1".to_vec()), (2, b"text 2".to_vec()), (3, b"text 3".to_vec())]);
        assert_eq!(keys(Keyspace::Text, 0, None, false), vec![(1, vec![]), (2, vec![]), (3, vec![])]);
        assert_eq!(
            keys(Keyspace::Extra, 0, None, true),
            vec![(3, b"{\"a\":1}".to_vec())],
            "key 2's extra was removed by the replace above"
        );
        assert_eq!(keys(Keyspace::Meta, 2, Some(3), false), vec![(2, vec![])]);
        assert_eq!(keys(Keyspace::Meta, 4, None, false), vec![]);
        let rkeys = |ks: Keyspace, lo: u8, hi: Option<u8>, values: bool| -> Vec<(u8, Vec<u8>)> {
            let hi = hi.map(|h| [h; 16]);
            store.iter_rev(ks, &[lo; 16], hi.as_ref(), values).unwrap().map(|r| r.unwrap()).map(|(k, v)| (k[0], v)).collect()
        };
        assert_eq!(rkeys(Keyspace::Text, 0, None, true), vec![(3, b"text 3".to_vec()), (2, b"text 2".to_vec()), (1, b"text 1".to_vec())]);
        assert_eq!(rkeys(Keyspace::Meta, 0, Some(3), false), vec![(2, vec![]), (1, vec![])], "hi stays exclusive going backwards");
        assert_eq!(rkeys(Keyspace::Meta, 2, None, false), vec![(3, vec![]), (2, vec![])]);
        assert_eq!(rkeys(Keyspace::Meta, 4, None, false), vec![]);

        // raw writes and deletes
        store.put_raw(Keyspace::Extra, &[1; 16], Some(b"{\"raw\":true}")).unwrap();
        assert_eq!(store.get_extra(&[1; 16]).unwrap().unwrap(), b"{\"raw\":true}");
        store.put_raw(Keyspace::Extra, &[1; 16], None).unwrap();
        assert!(store.get_extra(&[1; 16]).unwrap().is_none());
        store.put_raw(Keyspace::Text, &[7; 16], Some(b"orphan")).unwrap();
        store.put_raw(Keyspace::Meta, &[7; 16], Some(b"\xff")).unwrap();
        assert_eq!(keys(Keyspace::Text, 7, None, true), vec![(7, b"orphan".to_vec())]);
        store.put_raw(Keyspace::Text, &[7; 16], None).unwrap();
        store.put_raw(Keyspace::Meta, &[7; 16], None).unwrap();
        assert!(store.get_text(&[7; 16]).unwrap().is_none());
        // meta rewrites touch nothing else
        let fresh = crate::meta::Meta { words: 77, ..Default::default() }.encode();
        store.put_meta_batch(&[([2; 16], fresh.clone()), ([3; 16], fresh)]).unwrap();
        store.put_meta_batch(&[]).unwrap();
        for b in [2u8, 3] {
            assert_eq!(crate::meta::Meta::decode(&store.get_meta(&[b; 16]).unwrap().unwrap()).unwrap().words, 77);
            assert_eq!(store.get_text(&[b; 16]).unwrap().unwrap(), format!("text {b}").as_bytes());
        }
        assert_eq!(store.get_extra(&[3; 16]).unwrap().unwrap(), b"{\"a\":1}");

        // a text above the blob threshold, so the engines that separate values have a blob file
        let big = Record { key: [8; 16], meta: crate::meta::Meta::default().encode(), text: vec![b'x'; 4096], extra: None, replace: false };
        store.put_batch(&[big]).unwrap();
        store.persist().unwrap();
        assert_eq!(store.get_text(&[8; 16]).unwrap().unwrap().len(), 4096);

        // compaction keeps everything readable
        store.compact().unwrap();
        assert_eq!(store.get_text(&[2; 16]).unwrap().unwrap(), b"text 2");
        assert_eq!(keys(Keyspace::Meta, 0, None, false).len(), 4);
        let details = store.details().unwrap();
        assert!(details.iter().any(|(k, _)| k == "meta"), "{details:?}");
        assert!(details.iter().all(|(_, v)| !v.is_empty()));

        // deletes remove every keyspace entry; unknown keys are ignored
        store.delete_batch(&[[1; 16], [3; 16], [9; 16]]).unwrap();
        store.delete_batch(&[]).unwrap();
        assert!(store.get_meta(&[1; 16]).unwrap().is_none());
        assert!(store.get_text(&[1; 16]).unwrap().is_none());
        assert!(store.get_extra(&[3; 16]).unwrap().is_none());
        assert_eq!(keys(Keyspace::Meta, 0, None, false).iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![2, 8]);
        assert_eq!(keys(Keyspace::Text, 0, None, false).len(), 2);
        assert_eq!(keys(Keyspace::Extra, 0, None, false).len(), 0);
    }

    use super::NullStore as Empty;

    #[test]
    fn empty_store_double_answers_every_call() {
        let e = Empty::default();
        e.put_batch(&[]).unwrap();
        assert!(e.get_meta(&[0; 16]).unwrap().is_none());
        assert!(e.get_text(&[0; 16]).unwrap().is_none());
        assert!(e.get_extra(&[0; 16]).unwrap().is_none());
        e.scan_meta(&[0; 16], None, &mut |_, _| Ok(true)).unwrap();
        e.persist().unwrap();
        assert_eq!(e.disk_size().unwrap(), 0);
        assert_eq!(e.approx_count().unwrap(), 0);
        assert_eq!(Keyspace::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>(), vec!["meta", "text", "extra"]);
    }

    #[test]
    fn sources_intern_and_persist() {
        let fake = Empty { sources: std::sync::Mutex::new(vec![(0, "cli".into())]), ..Default::default() };
        let mut s = Sources::load(&fake).unwrap();
        assert_eq!(s.len(), 1);
        assert!(!s.is_empty());
        assert_eq!(s.id("cli"), Some(0));
        assert_eq!(s.name(0), "cli");
        assert_eq!(s.name(7), "?");
        assert_eq!(s.intern("cli", &fake).unwrap(), 0);
        assert_eq!(s.intern("eebo", &fake).unwrap(), 1);
        assert_eq!(s.intern("eebo", &fake).unwrap(), 1);
        assert_eq!(s.name(1), "eebo");
        assert_eq!(fake.sources().unwrap().len(), 2);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![(0, "cli"), (1, "eebo")]);
        let empty = Sources::default();
        assert!(empty.is_empty());
        assert_eq!(empty.id("x"), None);
    }

    #[test]
    fn sources_overflow() {
        let fake = Empty::default();
        let mut s = Sources::default();
        for i in 0..=u16::MAX {
            s.set(i, format!("s{i}"));
        }
        assert!(s.intern("one more", &fake).is_err());
    }

    #[test]
    fn store_dir_rules() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("new");
        check_dir(&missing, true).unwrap();
        assert!(check_dir(&missing, false).unwrap_err().to_string().contains("not a pipeline store"));
        // empty dir counts as new
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        check_dir(&empty, true).unwrap();
        // non-empty dir without marker is refused
        let foreign = dir.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("x"), b"x").unwrap();
        assert!(check_dir(&foreign, true).unwrap_err().to_string().contains("not a pipeline store"));
        // the marker must name this engine
        let marked = dir.path().join("marked");
        std::fs::create_dir(&marked).unwrap();
        std::fs::write(marker_path(&marked), "fjall\n").unwrap();
        check_dir(&marked, false).unwrap();
        std::fs::write(marker_path(&marked), "rocksdb\n").unwrap();
        let e = check_dir(&marked, false).unwrap_err().to_string();
        assert!(e.contains("unknown engine \"rocksdb\"") && e.contains("only reads fjall"), "{e}");
        assert_eq!(check_dir(&marked, false).unwrap_err().exit_code(), 1);
        // unreadable marker (a directory) is an io error
        let weird = dir.path().join("weird");
        std::fs::create_dir_all(marker_path(&weird)).unwrap();
        assert!(check_dir(&weird, false).is_err());
    }

    #[test]
    fn open_creates_marker_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let store = open(&path, true).unwrap();
        store.put_batch(&[rec(1, 1, None)]).unwrap();
        drop(store);
        assert_eq!(std::fs::read_to_string(marker_path(&path)).unwrap().trim(), ENGINE);
        let store = open(&path, false).unwrap();
        assert!(store.get_text(&[1; 16]).unwrap().is_some());
        drop(store);
        std::fs::write(marker_path(&path), "redb\n").unwrap();
        assert!(open(&path, false).is_err());
        assert!(open(&dir.path().join("nope"), false).is_err());
    }
}
