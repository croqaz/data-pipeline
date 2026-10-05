//! RocksDB backend: column families with BlobDB for `text`.

use std::path::{Path, PathBuf};

use rust_rocksdb::{BlockBasedOptions, ColumnFamily, ColumnFamilyDescriptor, DB, DBCompressionType, Direction, IteratorMode, Options, WriteBatch, WriteOptions};

use crate::error::{Error, Result};
use crate::hash::Key;

use super::{Backend, Record, ScanFn, Store};

pub struct RocksStore {
    db: DB,
    path: PathBuf,
    disable_wal: bool,
}

/// `PIPELINE_ROCKS_WAL=off` skips the write-ahead log during imports. Faster,
/// and an interrupted import then loses its unflushed tail, which a re-run
/// restores. Experimental; the default keeps the log.
pub fn wal_from_env() -> bool {
    !matches!(std::env::var("PIPELINE_ROCKS_WAL").as_deref(), Ok("off"))
}

const META: &str = "meta";
const TEXT: &str = "text";
const EXTRA: &str = "extra";
const SOURCES: &str = "sources";

impl RocksStore {
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with(dir, wal_from_env())
    }

    pub fn open_with(dir: &Path, wal: bool) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.increase_parallelism(8);
        opts.set_max_background_jobs(6);

        let mut table = BlockBasedOptions::default();
        table.set_bloom_filter(10.0, false);

        let mut meta = Options::default();
        meta.set_compression_type(DBCompressionType::Lz4);
        meta.set_write_buffer_size(64 << 20);
        meta.set_block_based_table_factory(&table);

        let mut text = Options::default();
        text.set_compression_type(DBCompressionType::Zstd);
        text.set_write_buffer_size(128 << 20);
        text.set_enable_blob_files(true);
        text.set_min_blob_size(1024);
        text.set_blob_compression_type(DBCompressionType::Zstd);
        text.set_enable_blob_gc(true);
        text.set_block_based_table_factory(&table);

        let mut extra = Options::default();
        extra.set_compression_type(DBCompressionType::Lz4);
        extra.set_block_based_table_factory(&table);

        let sources = Options::default();

        let cfs = vec![
            ColumnFamilyDescriptor::new(META, meta),
            ColumnFamilyDescriptor::new(TEXT, text),
            ColumnFamilyDescriptor::new(EXTRA, extra),
            ColumnFamilyDescriptor::new(SOURCES, sources),
        ];
        let db = DB::open_cf_descriptors(&opts, dir, cfs).map_err(Error::store)?;
        Ok(Self { db, path: dir.to_path_buf(), disable_wal: !wal })
    }

    fn cf(&self, name: &str) -> &ColumnFamily {
        self.db.cf_handle(name).expect("column family was opened")
    }

    fn get(&self, name: &str, key: &Key) -> Result<Option<Vec<u8>>> {
        self.db.get_cf(self.cf(name), key).map_err(Error::store)
    }
}

fn to_key(bytes: &[u8]) -> Result<Key> {
    bytes.try_into().map_err(|_| Error::Store(format!("key of length {} in meta column family", bytes.len())))
}

impl Store for RocksStore {
    fn backend(&self) -> Backend {
        Backend::Rocksdb
    }

    fn put_batch(&self, records: &[Record]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::default();
        let (meta, text, extra) = (self.cf(META), self.cf(TEXT), self.cf(EXTRA));
        for r in records {
            batch.put_cf(meta, r.key, &r.meta);
            batch.put_cf(text, r.key, &r.text);
            match &r.extra {
                Some(e) => batch.put_cf(extra, r.key, e),
                None => batch.delete_cf(extra, r.key),
            }
        }
        let mut wo = WriteOptions::default();
        wo.disable_wal(self.disable_wal);
        self.db.write_opt(&batch, &wo).map_err(Error::store)
    }

    fn get_meta(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        self.get(META, key)
    }

    fn get_text(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        self.get(TEXT, key)
    }

    fn get_extra(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        self.get(EXTRA, key)
    }

    fn scan_meta(&self, lo: &Key, hi: Option<&Key>, f: &mut ScanFn<'_>) -> Result<()> {
        let iter = self.db.iterator_cf(self.cf(META), IteratorMode::From(lo, Direction::Forward));
        for item in iter {
            let (k, v) = item.map_err(Error::store)?;
            if let Some(hi) = hi
                && k.as_ref() >= hi.as_slice()
            {
                break;
            }
            let key = to_key(&k)?;
            if !f(&key, &v)? {
                break;
            }
        }
        Ok(())
    }

    fn sources(&self) -> Result<Vec<(u16, String)>> {
        let mut out = Vec::new();
        for item in self.db.iterator_cf(self.cf(SOURCES), IteratorMode::Start) {
            let (k, v) = item.map_err(Error::store)?;
            let id = u16::from_be_bytes(k.as_ref().try_into().map_err(|_| Error::Store("bad source id".into()))?);
            out.push((id, String::from_utf8_lossy(&v).into_owned()));
        }
        Ok(out)
    }

    fn put_source(&self, id: u16, label: &str) -> Result<()> {
        self.db.put_cf(self.cf(SOURCES), id.to_be_bytes(), label.as_bytes()).map_err(Error::store)
    }

    fn persist(&self) -> Result<()> {
        for name in [META, TEXT, EXTRA, SOURCES] {
            self.db.flush_cf(self.cf(name)).map_err(Error::store)?;
        }
        self.db.flush_wal(true).map_err(Error::store)
    }

    fn disk_size(&self) -> Result<u64> {
        Ok(crate::util::dir_size(&self.path)?)
    }

    fn approx_count(&self) -> Result<u64> {
        Ok(self.db.property_int_value_cf(self.cf(META), "rocksdb.estimate-num-keys").map_err(Error::store)?.unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksStore::open(dir.path()).unwrap();
        assert_eq!(store.backend(), Backend::Rocksdb);
        super::super::tests::exercise(&store);
    }

    #[test]
    fn reopen_keeps_data_and_rejects_bad_keys() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = RocksStore::open(dir.path()).unwrap();
            store.put_batch(&[super::super::tests::rec(5, 1, None)]).unwrap();
            store.persist().unwrap();
        }
        let store = RocksStore::open(dir.path()).unwrap();
        assert!(store.get_text(&[5; 16]).unwrap().is_some());
        store.db.put_cf(store.cf(META), b"short", b"x").unwrap();
        assert!(store.scan_meta(&[0; 16], None, &mut |_, _| Ok(true)).is_err());
        store.db.put_cf(store.cf(SOURCES), b"bad", b"x").unwrap();
        assert!(store.sources().is_err());
        assert!(to_key(&[1u8; 3]).is_err());
    }

    #[test]
    fn without_wal_works_too() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksStore::open_with(dir.path(), false).unwrap();
        super::super::tests::exercise(&store);
        assert!(wal_from_env());
    }

    #[test]
    fn open_fails_on_a_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(RocksStore::open(&file).is_err());
    }
}
