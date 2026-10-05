//! `pipeline info` and `pipeline compact`: the cheap look and the rewrite.

use std::path::Path;
use std::time::Instant;

use crate::error::Result;
use crate::meta::{SCHEMA_VERSION, feature_names};
use crate::store::{Sources, Store};
use crate::util::{fmt_int, fmt_mb as mb, fmt_num};

pub struct InfoReport {
    pub dir: String,
    pub approx_docs: u64,
    pub disk: u64,
    pub sources: Vec<String>,
    pub details: Vec<(String, String)>,
}

impl InfoReport {
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("Store:      {}\n", self.dir));
        s.push_str(&format!("Documents:  about {}\n", fmt_int(self.approx_docs)));
        s.push_str(&format!("Disk:       {}\n", mb(self.disk)));
        let labels = if self.sources.is_empty() { "none".to_string() } else { self.sources.join(", ") };
        s.push_str(&format!("Sources:    {} ({})\n", self.sources.len(), labels));
        s.push_str(&format!("Features:   {} (schema {})\n", feature_names().join(", "), SCHEMA_VERSION));
        for (k, v) in &self.details {
            s.push_str(&format!("{:<12}{v}\n", format!("{k}:")));
        }
        s
    }
}

/// Everything `info` prints; no record is read.
pub fn info(store: &dyn Store, dir: &Path) -> Result<InfoReport> {
    let sources = Sources::load(store)?;
    let mut labels: Vec<(u16, String)> = sources.iter().map(|(id, n)| (id, n.to_string())).collect();
    labels.sort();
    Ok(InfoReport {
        dir: dir.display().to_string(),
        approx_docs: store.approx_count()?,
        disk: store.disk_size()?,
        sources: labels.into_iter().map(|(_, n)| n).collect(),
        details: store.details()?,
    })
}

pub struct CompactReport {
    pub before: u64,
    pub after: u64,
    pub secs: f64,
}

impl CompactReport {
    pub fn render(&self) -> String {
        format!("Compacted store: {} -> {} in {} s\n", mb(self.before), mb(self.after), fmt_num(self.secs, 1))
    }
}

/// Flush, rewrite every keyspace, flush again; report sizes and time.
pub fn compact(store: &dyn Store) -> Result<CompactReport> {
    let t0 = Instant::now();
    store.persist()?;
    let before = store.disk_size()?;
    store.compact()?;
    store.persist()?;
    let after = store.disk_size()?;
    Ok(CompactReport { before, after, secs: t0.elapsed().as_secs_f64() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NullStore;

    #[test]
    fn renders() {
        let rep = InfoReport {
            dir: "/x".into(),
            approx_docs: 1234,
            disk: 2_500_000,
            sources: vec!["a".into(), "b".into()],
            details: vec![("meta".into(), "1 MB".into())],
        };
        let s = rep.render();
        assert!(s.starts_with("Store:      /x\nDocuments:  about 1,234\nDisk:       2.5 MB\nSources:    2 (a, b)\nFeatures:   len, bytes"), "{s}");
        assert!(s.ends_with("meta:       1 MB\n"), "{s}");
        let none = InfoReport { sources: vec![], details: vec![], ..rep };
        assert!(none.render().contains("Sources:    0 (none)\n"));
        let c = CompactReport { before: 10_000_000, after: 9_000_000, secs: 1.26 };
        assert_eq!(c.render(), "Compacted store: 10.0 MB -> 9.0 MB in 1.3 s\n");
        assert_eq!(mb(0), "0.0 MB");
    }

    #[test]
    fn null_store_info_and_compact() {
        let store = NullStore::default();
        store.put_source(0, "z").unwrap();
        store.put_source(1, "a").unwrap();
        let rep = info(&store, Path::new("/dev/null")).unwrap();
        assert_eq!(rep.sources, vec!["z", "a"], "sorted by id, not by name");
        assert_eq!(rep.approx_docs, 0);
        let c = compact(&store).unwrap();
        assert_eq!((c.before, c.after), (0, 0));
    }
}
