//! The storage layer: one trait, two backends (fjall and RocksDB), chosen
//! per store at creation and remembered in a marker file.
//!
//! Every document lives in three keyspaces under the same 16-byte key:
//! `meta` (CBOR features), `text` (raw UTF-8) and `extra` (raw JSON object of
//! the metadata fields that rode along, absent when there were none). Two
//! small keyspaces hold the interned source labels and the schema marker.

#[cfg(feature = "fjall")]
pub mod fjall_store;
pub mod null_store;
#[cfg(feature = "rocksdb")]
pub mod rocks_store;

pub use null_store::NullStore;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::hash::Key;

pub const MARKER_FILE: &str = "pipeline.backend";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    Fjall,
    Rocksdb,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Fjall => "fjall",
            Backend::Rocksdb => "rocksdb",
        }
    }

    pub fn parse(s: &str) -> Option<Backend> {
        match s.trim() {
            "fjall" => Some(Backend::Fjall),
            "rocksdb" => Some(Backend::Rocksdb),
            _ => None,
        }
    }

    pub fn available(self) -> bool {
        match self {
            Backend::Fjall => cfg!(feature = "fjall"),
            Backend::Rocksdb => cfg!(feature = "rocksdb"),
        }
    }
}

/// One document ready to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: Key,
    pub meta: Vec<u8>,
    pub text: Vec<u8>,
    pub extra: Option<Vec<u8>>,
}

/// Callback for scans; return `Ok(false)` to stop early.
pub type ScanFn<'a> = dyn FnMut(&Key, &[u8]) -> Result<bool> + 'a;

pub trait Store: Send + Sync {
    fn backend(&self) -> Backend;
    /// Write a batch atomically across keyspaces.
    fn put_batch(&self, records: &[Record]) -> Result<()>;
    fn get_meta(&self, key: &Key) -> Result<Option<Vec<u8>>>;
    fn get_text(&self, key: &Key) -> Result<Option<Vec<u8>>>;
    fn get_extra(&self, key: &Key) -> Result<Option<Vec<u8>>>;
    /// Scan `meta` for keys in `[lo, hi)` (`hi == None` means to the end).
    fn scan_meta(&self, lo: &Key, hi: Option<&Key>, f: &mut ScanFn<'_>) -> Result<()>;
    /// All stored source labels.
    fn sources(&self) -> Result<Vec<(u16, String)>>;
    fn put_source(&self, id: u16, label: &str) -> Result<()>;
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

fn read_marker(dir: &Path) -> Result<Option<Backend>> {
    let path = dir.join(MARKER_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => Backend::parse(&s)
            .map(Some)
            .ok_or_else(|| Error::Store(format!("{} names an unknown backend {:?}", path.display(), s.trim()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Decide which backend a store directory uses. A new (missing or empty)
/// directory takes `requested` or fjall; an existing store must match.
pub fn resolve_backend(dir: &Path, requested: Option<Backend>, create: bool) -> Result<Backend> {
    let exists = dir.is_dir();
    let has_marker = read_marker(dir)?;
    match has_marker {
        Some(found) => {
            if let Some(req) = requested
                && req != found
            {
                return Err(Error::Usage(format!(
                    "{} is a {} store; `--backend {}` does not match",
                    dir.display(),
                    found.as_str(),
                    req.as_str()
                )));
            }
            Ok(found)
        }
        None => {
            let empty = !exists || std::fs::read_dir(dir)?.next().is_none();
            if !empty {
                return Err(Error::Usage(format!("{} exists but is not a pipeline store (no {MARKER_FILE})", dir.display())));
            }
            if !create {
                return Err(Error::Usage(format!("{} is not a pipeline store", dir.display())));
            }
            Ok(requested.unwrap_or(Backend::Fjall))
        }
    }
}

/// Open (or create, when `create` is set) the store at `dir`.
pub fn open(dir: &Path, requested: Option<Backend>, create: bool) -> Result<Box<dyn Store>> {
    let backend = resolve_backend(dir, requested, create)?;
    if !backend.available() {
        return Err(Error::Usage(format!("this binary was built without the {} backend", backend.as_str())));
    }
    if create {
        std::fs::create_dir_all(dir)?;
    }
    let store: Box<dyn Store> = match backend {
        #[cfg(feature = "fjall")]
        Backend::Fjall => Box::new(fjall_store::FjallStore::open(dir)?),
        #[cfg(feature = "rocksdb")]
        Backend::Rocksdb => Box::new(rocks_store::RocksStore::open(dir)?),
        #[allow(unreachable_patterns)]
        _ => unreachable!("availability checked above"),
    };
    let marker = dir.join(MARKER_FILE);
    if !marker.exists() {
        std::fs::write(&marker, format!("{}\n", backend.as_str()))?;
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
        Record { key: [byte; 16], meta, text: format!("text {byte}").into_bytes(), extra: extra.map(|s| s.as_bytes().to_vec()) }
    }

    /// The behavioural contract every backend must satisfy.
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
        store.scan_meta(&[0; 16], None, &mut |k, v| {
            seen.push((k[0], crate::meta::Meta::decode(v).unwrap().words));
            Ok(true)
        }).unwrap();
        assert_eq!(seen, vec![(1, 10), (2, 20), (3, 30)]);
        // bounded scan [2, 3)
        let mut seen = Vec::new();
        store.scan_meta(&[2; 16], Some(&[3; 16]), &mut |k, _| {
            seen.push(k[0]);
            Ok(true)
        }).unwrap();
        assert_eq!(seen, vec![2]);
        // early stop
        let mut n = 0;
        store.scan_meta(&[0; 16], None, &mut |_, _| {
            n += 1;
            Ok(false)
        }).unwrap();
        assert_eq!(n, 1);
        // callback errors propagate
        assert!(store.scan_meta(&[0; 16], None, &mut |_, _| Err(Error::Store("stop".into()))).is_err());

        // overwrite keeps the latest
        store.put_batch(&[rec(2, 99, Some("{}"))]).unwrap();
        assert_eq!(crate::meta::Meta::decode(&store.get_meta(&[2; 16]).unwrap().unwrap()).unwrap().words, 99);
        assert_eq!(store.get_extra(&[2; 16]).unwrap().unwrap(), b"{}");

        // sources
        assert!(store.sources().unwrap().is_empty());
        store.put_source(0, "cli").unwrap();
        store.put_source(1, "gutenberg").unwrap();
        assert_eq!(store.sources().unwrap(), vec![(0, "cli".to_string()), (1, "gutenberg".to_string())]);

        store.persist().unwrap();
        assert!(store.disk_size().unwrap() > 0);
        let approx = store.approx_count().unwrap();
        assert!((1..=10).contains(&approx), "approx {approx}");
    }

    use super::NullStore as Empty;

    #[test]
    fn empty_store_double_answers_every_call() {
        let e = Empty::default();
        assert_eq!(e.backend(), Backend::Fjall);
        e.put_batch(&[]).unwrap();
        assert!(e.get_meta(&[0; 16]).unwrap().is_none());
        assert!(e.get_text(&[0; 16]).unwrap().is_none());
        assert!(e.get_extra(&[0; 16]).unwrap().is_none());
        e.scan_meta(&[0; 16], None, &mut |_, _| Ok(true)).unwrap();
        e.persist().unwrap();
        assert_eq!(e.disk_size().unwrap(), 0);
        assert_eq!(e.approx_count().unwrap(), 0);
    }

    #[test]
    fn sources_intern_and_persist() {
        let fake = Empty { sources: std::sync::Mutex::new(vec![(0, "cli".into())]) };
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
    fn backend_names() {
        assert_eq!(Backend::parse("fjall"), Some(Backend::Fjall));
        assert_eq!(Backend::parse(" rocksdb\n"), Some(Backend::Rocksdb));
        assert_eq!(Backend::parse("sled"), None);
        assert_eq!(Backend::Fjall.as_str(), "fjall");
        assert_eq!(Backend::Rocksdb.as_str(), "rocksdb");
        assert!(Backend::Fjall.available());
    }

    #[test]
    fn resolve_backend_rules() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("new");
        assert_eq!(resolve_backend(&missing, None, true).unwrap(), Backend::Fjall);
        assert_eq!(resolve_backend(&missing, Some(Backend::Rocksdb), true).unwrap(), Backend::Rocksdb);
        assert!(resolve_backend(&missing, None, false).is_err());
        // empty dir counts as new
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(resolve_backend(&empty, None, true).unwrap(), Backend::Fjall);
        // non-empty dir without marker is refused
        let foreign = dir.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("x"), b"x").unwrap();
        assert!(resolve_backend(&foreign, None, true).unwrap_err().to_string().contains("not a pipeline store"));
        // marker wins, mismatch is an error
        let marked = dir.path().join("marked");
        std::fs::create_dir(&marked).unwrap();
        std::fs::write(marker_path(&marked), "rocksdb\n").unwrap();
        assert_eq!(resolve_backend(&marked, None, false).unwrap(), Backend::Rocksdb);
        assert_eq!(resolve_backend(&marked, Some(Backend::Rocksdb), false).unwrap(), Backend::Rocksdb);
        assert!(resolve_backend(&marked, Some(Backend::Fjall), false).unwrap_err().to_string().contains("does not match"));
        std::fs::write(marker_path(&marked), "sled\n").unwrap();
        assert!(resolve_backend(&marked, None, false).unwrap_err().to_string().contains("unknown backend"));
        // unreadable marker (a directory) is an io error
        let weird = dir.path().join("weird");
        std::fs::create_dir_all(marker_path(&weird)).unwrap();
        assert!(resolve_backend(&weird, None, false).is_err());
    }

    #[test]
    fn open_creates_marker_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let store = open(&path, None, true).unwrap();
        assert_eq!(store.backend(), Backend::Fjall);
        drop(store);
        assert_eq!(std::fs::read_to_string(marker_path(&path)).unwrap().trim(), "fjall");
        let store = open(&path, None, false).unwrap();
        assert_eq!(store.backend(), Backend::Fjall);
        drop(store);
        assert!(open(&path, Some(Backend::Rocksdb), false).is_err());
        assert!(open(&dir.path().join("nope"), None, false).is_err());
    }
}
