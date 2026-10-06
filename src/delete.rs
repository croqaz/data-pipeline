//! `pipeline delete`: remove documents by id or by expression.
//!
//! Selection first, then confirmation (in the command layer), then an
//! optional backup of the selected documents, then batched deletes. Every
//! step prints a count; nothing is removed that was not listed. The space
//! comes back at the next `compact`.

use std::path::PathBuf;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::export::{self, FileInfo};
use crate::expr;
use crate::hash::Key;
use crate::show::resolve_id;
use crate::stats::{ScanConfig, scan};
use crate::store::{self, Sources, Store};
use crate::util::{fmt_int, fmt_mb, fmt_num};

pub const DEFAULT_BATCH: usize = 2000;

#[derive(Debug, Clone)]
pub struct DeleteOptions {
    pub db: PathBuf,
    pub expr: Option<String>,
    pub ids: Vec<String>,
    pub limit: Option<u64>,
    pub threads: usize,
    pub backup: Option<PathBuf>,
    pub force: bool,
    pub batch_size: usize,
}

impl DeleteOptions {
    pub fn validate(&self) -> Result<()> {
        match (self.expr.is_some(), self.ids.is_empty()) {
            (false, true) => {
                return Err(Error::Usage("give an expression or at least one --id".into()));
            }
            (true, false) => {
                return Err(Error::Usage("give either an expression or --id, not both".into()));
            }
            _ => {}
        }
        if self.limit == Some(0) {
            return Err(Error::Usage("--limit must be at least 1".into()));
        }
        Ok(())
    }
}

/// What a delete would touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub scanned: u64,
    pub keys: Vec<Key>,
}

/// Resolve `--id` values or scan for the expression; keys come back in
/// key order, deduplicated, at most `limit` of them.
pub fn select(store: &dyn Store, sources: &Sources, opts: &DeleteOptions) -> Result<Selection> {
    if !opts.ids.is_empty() {
        let mut keys = Vec::with_capacity(opts.ids.len());
        for id in &opts.ids {
            keys.push(resolve_id(store, id)?);
        }
        keys.sort_unstable();
        keys.dedup();
        if let Some(l) = opts.limit {
            keys.truncate(usize::try_from(l).unwrap_or(usize::MAX));
        }
        return Ok(Selection { scanned: 0, keys });
    }
    let filter = expr::compile(opts.expr.as_deref().expect("validated"))?;
    // a limit stops early, which only works in key order: one thread
    let threads = if opts.limit.is_some() { 1 } else { opts.threads };
    let cfg = ScanConfig { filter: Some(&filter), threads, limit: None };
    let limit = opts.limit;
    let (scanned, _, accs) = scan(store, sources, &cfg, Vec::<Key>::new, |acc, k, _| {
        acc.push(*k);
        limit.is_none_or(|l| (acc.len() as u64) < l)
    })?;
    Ok(Selection { scanned, keys: accs.into_iter().flatten().collect() })
}

/// Delete in batches; returns how many keys were sent.
pub fn delete_keys(store: &dyn Store, keys: &[Key], batch_size: usize) -> Result<u64> {
    for chunk in keys.chunks(batch_size.max(1)) {
        store.delete_batch(chunk)?;
    }
    store.persist()?;
    Ok(keys.len() as u64)
}

#[derive(Debug, Clone)]
pub struct DeleteReport {
    pub scanned: u64,
    pub deleted: u64,
    pub backup: Option<FileInfo>,
    pub secs: f64,
    pub approx_docs: u64,
    pub disk: u64,
    /// Source names no remaining document uses.
    pub removed_sources: Vec<String>,
}

impl DeleteReport {
    pub fn render(&self) -> String {
        let mut s =
            format!("Deleted {} documents in {} s ({} scanned)\n", fmt_int(self.deleted), fmt_num(self.secs, 2), fmt_int(self.scanned));
        if let Some(b) = &self.backup {
            s.push_str(&format!("  Backup: {} ({} documents, {})\n", b.path.display(), fmt_int(b.docs), fmt_mb(b.bytes)));
        }
        s.push_str(&format!(
            "  Store: about {} documents, {} on disk; run `compact` to reclaim the space\n",
            fmt_int(self.approx_docs),
            fmt_mb(self.disk)
        ));
        if !self.removed_sources.is_empty() {
            s.push_str(&format!("  Unused sources removed: {}\n", self.removed_sources.join(", ")));
        }
        s
    }
}

/// Backup (if asked), delete, report. The caller has confirmed already.
pub fn run(
    store: &dyn Store,
    sources: &Sources,
    opts: &DeleteOptions,
    sel: Selection,
    log: &mut dyn std::io::Write,
) -> Result<DeleteReport> {
    let t0 = Instant::now();
    let backup = match &opts.backup {
        Some(path) => Some(export::export_keys(store, sources, &sel.keys, path, opts.threads, opts.force, log)?),
        None => None,
    };
    let deleted = delete_keys(store, &sel.keys, opts.batch_size)?;
    let removed_sources = if deleted > 0 { store::prune_sources(store)? } else { Vec::new() };
    Ok(DeleteReport {
        removed_sources,
        scanned: sel.scanned,
        deleted,
        backup,
        secs: t0.elapsed().as_secs_f64(),
        approx_docs: store.approx_count()?,
        disk: store.disk_size()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::key_to_hex;
    use crate::meta::Meta;
    use crate::store::{self, Record};

    fn opts(expr: Option<&str>, ids: &[&str]) -> DeleteOptions {
        DeleteOptions {
            db: PathBuf::from("x"),
            expr: expr.map(str::to_string),
            ids: ids.iter().map(|s| s.to_string()).collect(),
            limit: None,
            threads: 2,
            backup: None,
            force: false,
            batch_size: 2,
        }
    }

    fn store_with(n: u8) -> (tempfile::TempDir, Box<dyn Store>, Sources) {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&dir.path().join("db"), true).unwrap();
        let recs: Vec<Record> = (1..=n)
            .map(|i| Record {
                key: [i; 16],
                meta: Meta { words: u32::from(i), ..Default::default() }.encode(),
                text: format!("doc {i}").into_bytes(),
                extra: None,
                replace: false,
            })
            .collect();
        store.put_batch(&recs).unwrap();
        let mut sources = Sources::default();
        sources.intern("cli", store.as_ref()).unwrap();
        (dir, store, sources)
    }

    #[test]
    fn validation() {
        assert!(opts(None, &[]).validate().unwrap_err().to_string().contains("expression or at least one --id"));
        assert!(opts(Some("words > 1"), &["ab"]).validate().unwrap_err().to_string().contains("not both"));
        let mut o = opts(Some("words > 1"), &[]);
        o.limit = Some(0);
        assert!(o.validate().unwrap_err().to_string().contains("--limit"));
        o.limit = Some(1);
        o.validate().unwrap();
        opts(None, &["abcd"]).validate().unwrap();
    }

    #[test]
    fn select_by_expression_with_limit_and_by_ids() {
        let (_dir, store, sources) = store_with(6);
        let sel = select(store.as_ref(), &sources, &opts(Some("words >= 3"), &[])).unwrap();
        assert_eq!(sel.scanned, 6);
        assert_eq!(sel.keys.iter().map(|k| k[0]).collect::<Vec<_>>(), vec![3, 4, 5, 6]);
        let mut o = opts(Some("words >= 3"), &[]);
        o.limit = Some(2);
        let sel = select(store.as_ref(), &sources, &o).unwrap();
        assert_eq!(sel.keys.iter().map(|k| k[0]).collect::<Vec<_>>(), vec![3, 4]);
        assert_eq!(sel.scanned, 4, "the scan stops once the limit is reached");
        let full = key_to_hex(&[5; 16]);
        let sel = select(store.as_ref(), &sources, &opts(None, &[&full, "0202", &full])).unwrap();
        assert_eq!(sel.keys.iter().map(|k| k[0]).collect::<Vec<_>>(), vec![2, 5], "sorted and deduplicated");
        assert_eq!(sel.scanned, 0);
        let mut o = opts(None, &[&full, "0202"]);
        o.limit = Some(1);
        assert_eq!(select(store.as_ref(), &sources, &o).unwrap().keys.len(), 1);
        assert!(select(store.as_ref(), &sources, &opts(None, &["ffff"])).is_err());
        assert!(select(store.as_ref(), &sources, &opts(Some("nosuch > 1"), &[])).is_err());
    }

    #[test]
    fn delete_removes_everything_and_reports() {
        let (dir, store, sources) = store_with(5);
        let sel = select(store.as_ref(), &sources, &opts(Some("words <= 3"), &[])).unwrap();
        let backup = dir.path().join("gone.jsonl");
        let mut o = opts(Some("words <= 3"), &[]);
        o.backup = Some(backup.clone());
        let mut log = Vec::new();
        let rep = run(store.as_ref(), &sources, &o, sel, &mut log).unwrap();
        assert_eq!((rep.deleted, rep.scanned), (3, 5));
        let b = rep.backup.as_ref().unwrap();
        assert_eq!((b.path.clone(), b.docs), (backup.clone(), 3));
        let lines: Vec<serde_json::Value> =
            std::fs::read_to_string(&backup).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["text"], "doc 1");
        for i in 1..=3u8 {
            assert!(store.get_meta(&[i; 16]).unwrap().is_none());
            assert!(store.get_text(&[i; 16]).unwrap().is_none());
        }
        assert!(store.get_text(&[4; 16]).unwrap().is_some());
        let s = rep.render();
        assert!(s.starts_with("Deleted 3 documents in "), "{s}");
        assert!(s.contains(" s (5 scanned)\n  Backup: ") && s.contains("gone.jsonl (3 documents, "), "{s}");
        assert!(s.contains("run `compact` to reclaim"), "{s}");
        // the backup file is not overwritten without force
        let sel = Selection { scanned: 0, keys: vec![[4; 16]] };
        let e = run(store.as_ref(), &sources, &o, sel.clone(), &mut log).unwrap_err().to_string();
        assert!(e.contains("exists; use --force"), "{e}");
        o.force = true;
        let rep = run(store.as_ref(), &sources, &o, sel, &mut log).unwrap();
        assert_eq!(rep.deleted, 1);
        assert_eq!(std::fs::read_to_string(&backup).unwrap().lines().count(), 1);
        let plain = DeleteReport { scanned: 0, deleted: 0, backup: None, secs: 0.0, approx_docs: 1, disk: 0, removed_sources: vec![] };
        assert!(!plain.render().contains("Backup"));
        assert_eq!(delete_keys(store.as_ref(), &[], 0).unwrap(), 0);
    }
}
