//! A store that writes nothing. Used by `import --dry-run` to measure and
//! validate without touching disk, and by tests.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::Result;
use crate::hash::Key;

use super::{Keyspace, KvIter, Record, ScanFn, Store};

#[derive(Default)]
pub struct NullStore {
    pub sources: Mutex<Vec<(u16, String)>>,
    pub settings: Mutex<HashMap<String, Vec<u8>>>,
}

impl Store for NullStore {
    fn put_batch(&self, _: &[Record]) -> Result<()> {
        Ok(())
    }
    fn get_meta(&self, _: &Key) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn get_text(&self, _: &Key) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn get_extra(&self, _: &Key) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn scan_meta(&self, _: &Key, _: Option<&Key>, _: &mut ScanFn<'_>) -> Result<()> {
        Ok(())
    }
    fn iter<'a>(&'a self, _: Keyspace, _: &Key, _: Option<&Key>, _: bool) -> Result<KvIter<'a>> {
        Ok(Box::new(std::iter::empty()))
    }
    fn iter_rev<'a>(&'a self, _: Keyspace, _: &Key, _: Option<&Key>, _: bool) -> Result<KvIter<'a>> {
        Ok(Box::new(std::iter::empty()))
    }
    fn put_raw(&self, _: Keyspace, _: &Key, _: Option<&[u8]>) -> Result<()> {
        Ok(())
    }
    fn put_meta_batch(&self, _: &[(Key, Vec<u8>)]) -> Result<()> {
        Ok(())
    }
    fn delete_batch(&self, _: &[Key]) -> Result<()> {
        Ok(())
    }
    fn compact(&self) -> Result<()> {
        Ok(())
    }
    fn details(&self) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
    fn sources(&self) -> Result<Vec<(u16, String)>> {
        Ok(self.sources.lock().expect("lock").clone())
    }
    fn put_source(&self, id: u16, label: &str) -> Result<()> {
        self.sources.lock().expect("lock").push((id, label.to_string()));
        Ok(())
    }
    fn delete_source(&self, id: u16) -> Result<()> {
        self.sources.lock().expect("lock").retain(|(i, _)| *i != id);
        Ok(())
    }
    fn setting(&self, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.settings.lock().expect("lock").get(name).cloned())
    }
    fn put_setting(&self, name: &str, value: Option<&[u8]>) -> Result<()> {
        let mut s = self.settings.lock().expect("lock");
        match value {
            Some(v) => s.insert(name.to_string(), v.to_vec()),
            None => s.remove(name),
        };
        Ok(())
    }
    fn persist(&self) -> Result<()> {
        Ok(())
    }
    fn disk_size(&self) -> Result<u64> {
        Ok(0)
    }
    fn approx_count(&self) -> Result<u64> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_every_call_with_nothing() {
        let e = NullStore::default();
        e.put_batch(&[]).unwrap();
        assert!(e.get_meta(&[0; 16]).unwrap().is_none());
        assert!(e.get_text(&[0; 16]).unwrap().is_none());
        assert!(e.get_extra(&[0; 16]).unwrap().is_none());
        e.scan_meta(&[0; 16], None, &mut |_, _| Ok(true)).unwrap();
        e.put_source(1, "x").unwrap();
        assert_eq!(e.sources().unwrap(), vec![(1, "x".to_string())]);
        e.persist().unwrap();
        assert_eq!(e.disk_size().unwrap(), 0);
        assert_eq!(e.approx_count().unwrap(), 0);
        assert_eq!(e.iter(Keyspace::Meta, &[0; 16], None, true).unwrap().count(), 0);
        assert_eq!(e.iter_rev(Keyspace::Meta, &[0; 16], None, true).unwrap().count(), 0);
        e.put_raw(Keyspace::Text, &[0; 16], Some(b"x")).unwrap();
        e.put_meta_batch(&[([0; 16], vec![1])]).unwrap();
        e.delete_batch(&[[0; 16]]).unwrap();
        e.compact().unwrap();
        assert!(e.details().unwrap().is_empty());
        e.put_setting("k", Some(b"v")).unwrap();
        assert_eq!(e.setting("k").unwrap().as_deref(), Some(b"v".as_slice()));
        e.put_setting("k", None).unwrap();
        assert_eq!(e.setting("k").unwrap(), None);
    }
}
