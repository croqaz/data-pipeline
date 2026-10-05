//! A store that writes nothing. Used by `import --dry-run` to measure and
//! validate without touching disk, and by tests.

use std::sync::Mutex;

use crate::error::Result;
use crate::hash::Key;

use super::{Backend, Record, ScanFn, Store};

#[derive(Default)]
pub struct NullStore {
    pub sources: Mutex<Vec<(u16, String)>>,
}

impl Store for NullStore {
    fn backend(&self) -> Backend {
        Backend::Fjall
    }
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
    fn sources(&self) -> Result<Vec<(u16, String)>> {
        Ok(self.sources.lock().expect("lock").clone())
    }
    fn put_source(&self, id: u16, label: &str) -> Result<()> {
        self.sources.lock().expect("lock").push((id, label.to_string()));
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
        assert_eq!(e.backend(), Backend::Fjall);
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
    }
}
