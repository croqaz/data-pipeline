//! The fjall engine: pure-Rust LSM tree with key-value separation for `text`.

use std::path::Path;

use fjall::config::CompressionPolicy;
use fjall::{CompressionType, Database, Keyspace, KeyspaceCreateOptions, KvSeparationOptions, PersistMode};

use crate::error::{Error, Result};
use crate::hash::Key;

use super::{Keyspace as Ks, KvIter, Record, ScanFn, Store};
use crate::util::{fmt_int, fmt_mb};

pub struct FjallStore {
    db: Database,
    meta: Keyspace,
    text: Keyspace,
    extra: Keyspace,
    sources: Keyspace,
}

/// Block and blob compression for new segments. `PIPELINE_FJALL_COMPRESSION=none`
/// turns it off, for stores on a filesystem that compresses by itself.
pub fn compression_from_env() -> CompressionType {
    match std::env::var("PIPELINE_FJALL_COMPRESSION").as_deref() {
        Ok("none") => CompressionType::None,
        _ => CompressionType::Lz4,
    }
}

impl FjallStore {
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with(dir, compression_from_env())
    }

    pub fn open_with(dir: &Path, compression: CompressionType) -> Result<Self> {
        let db = Database::builder(dir).open().map_err(Error::store)?;
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default).map_err(Error::store)?;
        let text = db
            .keyspace("text", || {
                KeyspaceCreateOptions::default()
                    .with_kv_separation(Some(KvSeparationOptions::default().compression(compression)))
                    .data_block_compression_policy(CompressionPolicy::all(compression))
            })
            .map_err(Error::store)?;
        let extra = db.keyspace("extra", KeyspaceCreateOptions::default).map_err(Error::store)?;
        let sources = db.keyspace("sources", KeyspaceCreateOptions::default).map_err(Error::store)?;
        Ok(Self { db, meta, text, extra, sources })
    }

    fn get(ks: &Keyspace, key: &Key) -> Result<Option<Vec<u8>>> {
        Ok(ks.get(key.as_slice()).map_err(Error::store)?.map(|v| v.to_vec()))
    }

    fn space(&self, ks: Ks) -> &Keyspace {
        match ks {
            Ks::Meta => &self.meta,
            Ks::Text => &self.text,
            Ks::Extra => &self.extra,
        }
    }

    fn range(ks: &Keyspace, lo: &Key, hi: Option<&Key>) -> fjall::Iter {
        match hi {
            Some(hi) => ks.range(lo.to_vec()..hi.to_vec()),
            None => ks.range(lo.to_vec()..),
        }
    }
}

/// Turn fjall guards into the trait's `(key, value)` items. Without
/// `values`, `key()` is used, which never touches a separated blob.
fn adapt<'a>(it: impl Iterator<Item = fjall::Guard> + 'a, values: bool) -> KvIter<'a> {
    Box::new(it.map(move |guard| {
        if values {
            let (k, v) = guard.into_inner().map_err(Error::store)?;
            Ok((to_key(&k)?, v.to_vec()))
        } else {
            let k = guard.key().map_err(Error::store)?;
            Ok((to_key(&k)?, Vec::new()))
        }
    }))
}

fn to_key(bytes: &[u8]) -> Result<Key> {
    bytes.try_into().map_err(|_| Error::Store(format!("key of length {} in meta keyspace", bytes.len())))
}

impl Store for FjallStore {
    fn put_batch(&self, records: &[Record]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut batch = self.db.batch();
        for r in records {
            batch.insert(&self.meta, r.key.as_slice(), r.meta.as_slice());
            batch.insert(&self.text, r.key.as_slice(), r.text.as_slice());
            match &r.extra {
                Some(e) => batch.insert(&self.extra, r.key.as_slice(), e.as_slice()),
                None if r.replace => batch.remove(&self.extra, r.key.as_slice()),
                None => {}
            }
        }
        batch.commit().map_err(Error::store)
    }

    fn get_meta(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        Self::get(&self.meta, key)
    }

    fn get_text(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        Self::get(&self.text, key)
    }

    fn get_extra(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        Self::get(&self.extra, key)
    }

    fn scan_meta(&self, lo: &Key, hi: Option<&Key>, f: &mut ScanFn<'_>) -> Result<()> {
        for guard in Self::range(&self.meta, lo, hi) {
            let (k, v) = guard.into_inner().map_err(Error::store)?;
            let key = to_key(&k)?;
            if !f(&key, &v)? {
                break;
            }
        }
        Ok(())
    }

    fn iter<'a>(&'a self, ks: Ks, lo: &Key, hi: Option<&Key>, values: bool) -> Result<KvIter<'a>> {
        Ok(adapt(Self::range(self.space(ks), lo, hi), values))
    }

    fn iter_rev<'a>(&'a self, ks: Ks, lo: &Key, hi: Option<&Key>, values: bool) -> Result<KvIter<'a>> {
        Ok(adapt(Self::range(self.space(ks), lo, hi).rev(), values))
    }

    fn put_raw(&self, ks: Ks, key: &Key, value: Option<&[u8]>) -> Result<()> {
        let space = self.space(ks);
        match value {
            Some(v) => space.insert(key.as_slice(), v).map_err(Error::store),
            None => space.remove(key.as_slice()).map_err(Error::store),
        }
    }

    fn put_meta_batch(&self, items: &[(Key, Vec<u8>)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut batch = self.db.batch();
        for (key, meta) in items {
            batch.insert(&self.meta, key.as_slice(), meta.as_slice());
        }
        batch.commit().map_err(Error::store)
    }

    fn delete_batch(&self, keys: &[Key]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut batch = self.db.batch();
        for key in keys {
            batch.remove(&self.meta, key.as_slice());
            batch.remove(&self.text, key.as_slice());
            batch.remove(&self.extra, key.as_slice());
        }
        batch.commit().map_err(Error::store)
    }

    fn compact(&self) -> Result<()> {
        for ks in [&self.meta, &self.text, &self.extra, &self.sources] {
            ks.rotate_memtable_and_wait().map_err(Error::store)?;
            ks.major_compact().map_err(Error::store)?;
        }
        Ok(())
    }

    fn details(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for (name, ks) in [("meta", &self.meta), ("text", &self.text), ("extra", &self.extra)] {
            let mut v = format!(
                "{}, about {} keys, {} tables ({} at L0)",
                fmt_mb(ks.disk_space()),
                fmt_int(ks.approximate_len() as u64),
                ks.table_count(),
                ks.l0_table_count()
            );
            if ks.is_kv_separated() {
                v.push_str(&format!(", {} blob files ({} fragmented)", ks.blob_file_count(), fmt_mb(ks.fragmented_blob_bytes())));
            }
            out.push((name.to_string(), v));
        }
        out.push(("cache".to_string(), format!("{} used of {}", fmt_mb(self.db.cache_size()), fmt_mb(self.db.cache_capacity()))));
        Ok(out)
    }

    fn sources(&self) -> Result<Vec<(u16, String)>> {
        let mut out = Vec::new();
        for guard in self.sources.iter() {
            let (k, v) = guard.into_inner().map_err(Error::store)?;
            let id = u16::from_be_bytes(k.as_ref().try_into().map_err(|_| Error::Store("bad source id".into()))?);
            out.push((id, String::from_utf8_lossy(&v).into_owned()));
        }
        Ok(out)
    }

    fn put_source(&self, id: u16, label: &str) -> Result<()> {
        self.sources.insert(id.to_be_bytes().as_slice(), label.as_bytes()).map_err(Error::store)
    }

    fn delete_source(&self, id: u16) -> Result<()> {
        self.sources.remove(id.to_be_bytes().as_slice()).map_err(Error::store)
    }

    /// Flush memtables to segments and sync the journal, so later scans read
    /// sorted blocks instead of the skip list.
    fn persist(&self) -> Result<()> {
        for ks in [&self.meta, &self.text, &self.extra, &self.sources] {
            ks.rotate_memtable_and_wait().map_err(Error::store)?;
        }
        self.db.persist(PersistMode::SyncAll).map_err(Error::store)
    }

    fn disk_size(&self) -> Result<u64> {
        self.db.disk_space().map_err(Error::store)
    }

    fn approx_count(&self) -> Result<u64> {
        Ok(self.meta.approximate_len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        super::super::tests::exercise(&store);
    }

    #[test]
    fn reopen_keeps_data_and_rejects_bad_keys() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = FjallStore::open(dir.path()).unwrap();
            store.put_batch(&[super::super::tests::rec(5, 1, None)]).unwrap();
            store.persist().unwrap();
        }
        let store = FjallStore::open(dir.path()).unwrap();
        assert!(store.get_text(&[5; 16]).unwrap().is_some());
        // A foreign key length in the meta keyspace is reported, not ignored.
        store.meta.insert(b"short".as_slice(), b"x".as_slice()).unwrap();
        assert!(store.scan_meta(&[0; 16], None, &mut |_, _| Ok(true)).is_err());
        store.sources.insert(b"bad".as_slice(), b"x".as_slice()).unwrap();
        assert!(store.sources().is_err());
        assert!(to_key(&[1u8; 3]).is_err());
    }

    #[test]
    fn uncompressed_store_works_too() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open_with(dir.path(), CompressionType::None).unwrap();
        super::super::tests::exercise(&store);
        assert_eq!(compression_from_env(), CompressionType::Lz4);
    }

    #[test]
    fn open_fails_on_a_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(FjallStore::open(&file).is_err());
    }
}
