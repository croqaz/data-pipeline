//! `pipeline export`: the only way out. Scan `meta` for the matching keys,
//! then fetch every document in parallel and write JSON lines in key order
//! (a deterministic shuffle, since keys are hashes).
//!
//! Three phases, each reported: collect keys (filter, sample, top, limit),
//! fetch and write (`threads` workers, one ordered writer, `.partial` files
//! renamed when complete), and an optional `--verify` that re-reads the
//! output and parses every line. `written == selected` is checked, and a
//! mismatch is an error, never a silent loss.

pub mod fields;
pub mod sink;
pub mod template;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::expr::{self, Compiled};
use crate::hash::{Key, key_to_hex};
use crate::jsonl::reader::{Chunker, open_input};
use crate::meta::Meta;
use crate::stats::{ScanConfig, scan};
use crate::store::{self, Sources, Store};
use crate::util::{fmt_int, fmt_num, fmt_rate};

pub use fields::{Emitter, Field, Fields};
pub use sink::{FileInfo, Naming, Sink};
pub use template::{Template, unescape};

pub const DEFAULT_CHUNK_DOCS: usize = 256;
const PROGRESS_EVERY: u64 = 100_000;

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub db: PathBuf,
    pub expr: Option<String>,
    /// Field names; empty means the default list.
    pub fields: Vec<String>,
    pub limit: Option<u64>,
    /// Keep this fraction of the matches, decided per key, so the same
    /// `(sample, seed)` selects the same documents on every run.
    pub sample: Option<f64>,
    pub seed: u64,
    /// Keep only the N highest `Meta::score` matches, best first.
    pub top: Option<u64>,
    /// Header template, already unescaped.
    pub header: Option<String>,
    pub header_sep: String,
    /// Output path; `-` is stdout.
    pub out: PathBuf,
    pub shard: Option<u64>,
    pub zstd: bool,
    pub zstd_level: i32,
    pub threads: usize,
    pub verify: bool,
    pub force: bool,
    /// Documents per fetch job.
    pub chunk_docs: usize,
    /// Log a progress line every N written documents.
    pub progress_every: u64,
}

impl ExportOptions {
    pub fn new(db: PathBuf, out: PathBuf) -> Self {
        ExportOptions {
            db,
            expr: None,
            fields: Vec::new(),
            limit: None,
            sample: None,
            seed: 0,
            top: None,
            header: None,
            header_sep: "\n\n".to_string(),
            out,
            shard: None,
            zstd: false,
            zstd_level: 3,
            threads: 4,
            verify: false,
            force: false,
            chunk_docs: DEFAULT_CHUNK_DOCS,
            progress_every: PROGRESS_EVERY,
        }
    }

    pub fn to_stdout(&self) -> bool {
        self.out == Path::new("-")
    }

    fn validate(&self) -> Result<()> {
        let usage = |m: &str| Err(Error::Usage(m.to_string()));
        if let Some(r) = self.sample
            && !(r > 0.0 && r <= 1.0)
        {
            return usage("--sample must be in (0, 1]");
        }
        if self.top == Some(0) {
            return usage("--top must be at least 1");
        }
        if self.limit == Some(0) {
            return usage("--limit must be at least 1");
        }
        if self.shard == Some(0) {
            return usage("--shard must be at least 1");
        }
        if !(1..=22).contains(&self.zstd_level) {
            return usage("--zstd-level must be between 1 and 22");
        }
        if self.to_stdout() {
            if self.shard.is_some() {
                return usage("--shard cannot be combined with --out -");
            }
            if self.verify {
                return usage("--verify cannot be combined with --out -");
            }
            if self.zstd {
                return usage("--zstd cannot be combined with --out -");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub files: usize,
    pub lines: u64,
    pub millis: u64,
}

#[derive(Debug, Clone)]
pub struct ExportReport {
    pub scanned: u64,
    pub matched: u64,
    pub selected: u64,
    pub written: u64,
    pub files: Vec<FileInfo>,
    pub shadowed: u64,
    pub scan_secs: f64,
    pub write_secs: f64,
    pub verify: Option<VerifyReport>,
    pub expr: Option<String>,
    pub sample: Option<f64>,
    pub seed: u64,
    pub top: Option<u64>,
    pub limit: Option<u64>,
    pub to_stdout: bool,
}

impl ExportReport {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    pub fn render(&self) -> String {
        let mb = |b: u64| format!("{} MB", fmt_num(b as f64 / 1_000_000.0, 1));
        let mut s = String::new();
        let n = self.files.len();
        if self.to_stdout {
            s.push_str(&format!("Exported {} documents to stdout ({})\n", fmt_int(self.written), mb(self.bytes())));
        } else {
            let first = self.files.first().map(|f| f.path.display().to_string()).unwrap_or_default();
            let where_ = if n == 1 { first } else { format!("{n} files") };
            s.push_str(&format!("Exported {} documents to {where_} ({})\n", fmt_int(self.written), mb(self.bytes())));
        }
        let mut sel = Vec::new();
        if let Some(r) = self.sample {
            sel.push(format!("sample {}% seed {}", fmt_num(r * 100.0, 2), self.seed));
        }
        if let Some(t) = self.top {
            sel.push(format!("top {}", fmt_int(t)));
        }
        if let Some(l) = self.limit {
            sel.push(format!("limit {}", fmt_int(l)));
        }
        let matched = match &self.expr {
            Some(e) => format!("matched {} ({e})", fmt_int(self.matched)),
            None => format!("matched {}", fmt_int(self.matched)),
        };
        let selected = if sel.is_empty() { String::new() } else { format!(", selected {} ({})", fmt_int(self.selected), sel.join(", ")) };
        s.push_str(&format!("  Scanned {}, {matched}{selected}\n", fmt_int(self.scanned)));
        let docs_per_s = if self.write_secs > 0.0 { self.written as f64 / self.write_secs } else { 0.0 };
        s.push_str(&format!(
            "  Scan {} s, fetch and write {} s ({}, {} docs/s)\n",
            fmt_num(self.scan_secs, 2),
            fmt_num(self.write_secs, 2),
            fmt_rate(self.bytes(), self.write_secs),
            fmt_num(docs_per_s, 0)
        ));
        if n > 1 {
            s.push_str("  Files:\n");
            for f in &self.files {
                s.push_str(&format!("    {}  {} docs  {}\n", f.path.display(), fmt_int(f.docs), mb(f.bytes)));
            }
        }
        if let Some(v) = &self.verify {
            s.push_str(&format!(
                "  Verified: {} lines parse as JSON in {} file{} ({} s)\n",
                fmt_int(v.lines),
                v.files,
                if v.files == 1 { "" } else { "s" },
                fmt_num(v.millis as f64 / 1000.0, 2)
            ));
        }
        if self.shadowed > 0 {
            s.push_str(&format!("  Skipped {} metadata keys whose names belong to built-in fields\n", fmt_int(self.shadowed)));
        }
        s
    }
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic per-key coin: the same key, rate and seed always agree.
pub fn sampled(key: &Key, rate: f64, seed: u64) -> bool {
    let x = u64::from_le_bytes(key[8..16].try_into().expect("8 bytes")) ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let unit = (splitmix64(x) >> 11) as f64 / (1u64 << 53) as f64;
    unit < rate
}

/// Phase 1: the keys to export, in output order.
fn collect_keys(store: &dyn Store, sources: &Sources, filter: Option<&Compiled>, opts: &ExportOptions) -> Result<(u64, u64, Vec<Key>)> {
    let sample = opts.sample.filter(|r| *r < 1.0);
    let keep = |k: &Key| sample.is_none_or(|r| sampled(k, r, opts.seed));
    if let Some(top) = opts.top {
        let cfg = ScanConfig { filter, threads: opts.threads, limit: None };
        let (scanned, matched, accs) = scan(store, sources, &cfg, Vec::<(f64, Key)>::new, |acc, k, m| {
            if keep(k) {
                acc.push((m.score(), *k));
            }
            true
        })?;
        let mut all: Vec<(f64, Key)> = accs.into_iter().flatten().collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        all.truncate(usize::try_from(top).unwrap_or(usize::MAX));
        if let Some(l) = opts.limit {
            all.truncate(usize::try_from(l).unwrap_or(usize::MAX));
        }
        return Ok((scanned, matched, all.into_iter().map(|(_, k)| k).collect()));
    }
    // A limit stops the scan early, which only works in key order: one thread.
    let threads = if opts.limit.is_some() { 1 } else { opts.threads };
    let cfg = ScanConfig { filter, threads, limit: None };
    let limit = opts.limit;
    let (scanned, matched, accs) = scan(store, sources, &cfg, Vec::<Key>::new, |acc, k, _| {
        if keep(k) {
            acc.push(*k);
        }
        limit.is_none_or(|l| (acc.len() as u64) < l)
    })?;
    Ok((scanned, matched, accs.into_iter().flatten().collect()))
}

fn fetch_chunk(store: &dyn Store, keys: &[Key], emitter: &Emitter<'_>, needs_text: bool, needs_extra: bool) -> Result<(Vec<u8>, u64)> {
    let mut buf = Vec::with_capacity(keys.len() * 2048);
    for key in keys {
        let missing = |what: &str| Error::Store(format!("{what} missing for document {}", key_to_hex(key)));
        let meta_bytes = store.get_meta(key)?.ok_or_else(|| missing("meta"))?;
        let meta = Meta::decode(&meta_bytes)?;
        let text = if needs_text { Some(store.get_text(key)?.ok_or_else(|| missing("text"))?) } else { None };
        let text_str = match &text {
            Some(t) => Some(std::str::from_utf8(t).map_err(|_| Error::Store(format!("text of document {} is not UTF-8", key_to_hex(key))))?),
            None => None,
        };
        let extra = if needs_extra && meta.has_extra() { store.get_extra(key)? } else { None };
        emitter.write(&mut buf, key, &meta, text_str, extra.as_deref())?;
    }
    Ok((buf, keys.len() as u64))
}

/// Phase 2: `threads` workers fetch and serialize chunks of keys; this
/// thread writes them in order. At most `2 * threads` chunks are in flight.
fn write_all(store: &dyn Store, keys: &[Key], emitter: &Emitter<'_>, sink: &mut Sink<'_>, opts: &ExportOptions, log: &mut dyn Write) -> Result<u64> {
    if keys.is_empty() {
        return Ok(0);
    }
    let threads = opts.threads.clamp(1, 256);
    let chunk = opts.chunk_docs.max(1);
    let progress_every = opts.progress_every.max(1);
    let njobs = keys.len().div_ceil(chunk);
    let max_inflight = threads * 2;
    let (job_tx, job_rx) = crossbeam_channel::bounded::<usize>(threads);
    let (res_tx, res_rx) = crossbeam_channel::bounded::<(usize, Result<(Vec<u8>, u64)>)>(max_inflight);
    let needs_text = emitter.needs_text();
    let needs_extra = emitter.needs_extra();
    let total = keys.len() as u64;
    std::thread::scope(|s| {
        for _ in 0..threads.min(njobs) {
            let job_rx = job_rx.clone();
            let res_tx = res_tx.clone();
            s.spawn(move || {
                for j in job_rx {
                    let slice = &keys[j * chunk..((j + 1) * chunk).min(keys.len())];
                    let r = fetch_chunk(store, slice, emitter, needs_text, needs_extra);
                    if res_tx.send((j, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(res_tx);
        let mut next_job = 0usize;
        let mut in_flight = 0usize;
        let mut next_write = 0usize;
        let mut pending: BTreeMap<usize, (Vec<u8>, u64)> = BTreeMap::new();
        let mut written = 0u64;
        let mut next_log = progress_every;
        let result = (|| -> Result<u64> {
            loop {
                while in_flight < max_inflight && next_job < njobs {
                    job_tx.send(next_job).map_err(|_| Error::Store("export workers stopped".into()))?;
                    next_job += 1;
                    in_flight += 1;
                }
                if in_flight == 0 {
                    break;
                }
                let (j, r) = res_rx.recv().map_err(|_| Error::Store("export workers stopped".into()))?;
                in_flight -= 1;
                pending.insert(j, r?);
                while let Some((buf, docs)) = pending.remove(&next_write) {
                    sink.write(&buf, docs)?;
                    written += docs;
                    next_write += 1;
                    if written >= next_log {
                        writeln!(log, "  exported {} / {} documents", fmt_int(written), fmt_int(total))?;
                        next_log += progress_every;
                    }
                }
            }
            Ok(written)
        })();
        drop(job_tx);
        result
    })
}

/// Phase 3: re-read one output file, parse every line, return the count.
pub fn verify_file(path: &Path, threads: usize) -> Result<u64> {
    let reader = open_input(path)?;
    let mut chunker = Chunker::new(reader, 4 << 20);
    let threads = threads.clamp(1, 64);
    let (tx, rx) = crossbeam_channel::bounded::<crate::jsonl::reader::Chunk>(threads * 2);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let rx = rx.clone();
                s.spawn(move || -> Result<u64> {
                    let mut n = 0u64;
                    for chunk in rx {
                        for (i, line) in chunk.data.split(|&b| b == b'\n').enumerate() {
                            if (i as u64) >= chunk.lines {
                                break;
                            }
                            let lineno = chunk.first_line + i as u64;
                            if line.is_empty() {
                                return Err(Error::Store(format!("{}:{lineno}: empty line", path.display())));
                            }
                            sonic_rs::from_slice::<serde::de::IgnoredAny>(line).map_err(|e| Error::Store(format!("{}:{lineno}: {e}", path.display())))?;
                            n += 1;
                        }
                    }
                    Ok(n)
                })
            })
            .collect();
        drop(rx);
        let fed = (|| -> Result<()> {
            while let Some(chunk) = chunker.next_chunk()? {
                if tx.send(chunk).is_err() {
                    break;
                }
            }
            Ok(())
        })();
        drop(tx);
        let mut total = 0;
        let mut first_err = fed.err();
        for h in handles {
            match h.join().expect("verify thread panicked") {
                Ok(n) => total += n,
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(total),
        }
    })
}

fn verify_files(files: &[FileInfo], threads: usize) -> Result<VerifyReport> {
    let t0 = Instant::now();
    let mut lines = 0;
    for f in files {
        let n = verify_file(&f.path, threads)?;
        if n != f.docs {
            return Err(Error::Store(format!("{}: {} lines parsed, {} documents written", f.path.display(), fmt_int(n), fmt_int(f.docs))));
        }
        lines += n;
    }
    Ok(VerifyReport { files: files.len(), lines, millis: t0.elapsed().as_millis() as u64 })
}

/// Run an export. Records go to `stdout` only with `--out -`; progress goes
/// to `log`. The caller prints the report.
pub fn export(opts: &ExportOptions, stdout: &mut dyn Write, log: &mut dyn Write) -> Result<ExportReport> {
    opts.validate()?;
    let filter = opts.expr.as_deref().map(expr::compile).transpose()?;
    let fields = if opts.fields.is_empty() { Fields::default() } else { Fields::parse(&opts.fields)? };
    let header = opts.header.as_deref().map(Template::parse).transpose()?;
    let naming = if opts.to_stdout() { None } else { Some(Naming::new(&opts.out, opts.zstd, opts.shard)) };

    let store = store::open(&opts.db, false)?;
    let sources = Sources::load(store.as_ref())?;
    let emitter = Emitter::new(&fields, header.as_ref(), &opts.header_sep, &sources);

    let t0 = Instant::now();
    let (scanned, matched, keys) = collect_keys(store.as_ref(), &sources, filter.as_ref(), opts)?;
    let scan_secs = t0.elapsed().as_secs_f64();
    let selected = keys.len() as u64;

    let t1 = Instant::now();
    let mut sink = match naming {
        Some(n) => {
            n.check_targets(selected, opts.force)?;
            Sink::files(n, opts.zstd_level, opts.threads)
        }
        None => Sink::stdout(stdout),
    };
    let written = write_all(store.as_ref(), &keys, &emitter, &mut sink, opts, log)?;
    let files = sink.finish()?;
    let write_secs = t1.elapsed().as_secs_f64();
    if written != selected {
        return Err(Error::Store(format!("{} documents selected but {} written", fmt_int(selected), fmt_int(written))));
    }
    let verify = if opts.verify { Some(verify_files(&files, opts.threads)?) } else { None };
    Ok(ExportReport {
        scanned,
        matched,
        selected,
        written,
        files,
        shadowed: emitter.shadowed.load(Ordering::Relaxed),
        scan_secs,
        write_secs,
        verify,
        expr: opts.expr.clone(),
        sample: opts.sample,
        seed: opts.seed,
        top: opts.top,
        limit: opts.limit,
        to_stdout: opts.to_stdout(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_is_deterministic_and_about_right() {
        let mut keys = Vec::new();
        for i in 0..20_000u64 {
            let mut k = [0u8; 16];
            k[8..16].copy_from_slice(&i.to_le_bytes());
            keys.push(k);
        }
        let a: Vec<bool> = keys.iter().map(|k| sampled(k, 0.1, 7)).collect();
        let b: Vec<bool> = keys.iter().map(|k| sampled(k, 0.1, 7)).collect();
        assert_eq!(a, b);
        let n = a.iter().filter(|x| **x).count();
        assert!((1700..2300).contains(&n), "{n}");
        let c: Vec<bool> = keys.iter().map(|k| sampled(k, 0.1, 8)).collect();
        assert_ne!(a, c, "the seed changes the selection");
        assert!(keys.iter().all(|k| sampled(k, 1.0, 0)));
        assert!(keys.iter().all(|k| !sampled(k, 0.0, 0)));
    }

    #[test]
    fn option_validation() {
        let base = ExportOptions::new(PathBuf::from("db"), PathBuf::from("o.jsonl"));
        base.validate().unwrap();
        let msg = |f: &dyn Fn(&mut ExportOptions)| {
            let mut o = base.clone();
            f(&mut o);
            o.validate().unwrap_err().to_string()
        };
        assert!(msg(&|o| o.sample = Some(0.0)).contains("--sample"));
        assert!(msg(&|o| o.sample = Some(1.5)).contains("--sample"));
        assert!(msg(&|o| o.top = Some(0)).contains("--top"));
        assert!(msg(&|o| o.limit = Some(0)).contains("--limit"));
        assert!(msg(&|o| o.shard = Some(0)).contains("--shard"));
        assert!(msg(&|o| o.zstd_level = 0).contains("--zstd-level"));
        assert!(msg(&|o| {
            o.out = PathBuf::from("-");
            o.shard = Some(1);
        })
        .contains("--shard cannot"));
        assert!(msg(&|o| {
            o.out = PathBuf::from("-");
            o.verify = true;
        })
        .contains("--verify cannot"));
        assert!(msg(&|o| {
            o.out = PathBuf::from("-");
            o.zstd = true;
        })
        .contains("--zstd cannot"));
        let mut o = base.clone();
        o.out = PathBuf::from("-");
        assert!(o.to_stdout());
        o.validate().unwrap();
    }

    #[test]
    fn report_render_variants() {
        let files = vec![
            FileInfo { path: PathBuf::from("/o-0001.jsonl"), docs: 2, bytes: 1_500_000 },
            FileInfo { path: PathBuf::from("/o-0002.jsonl"), docs: 1, bytes: 500_000 },
        ];
        let rep = ExportReport {
            scanned: 10,
            matched: 5,
            selected: 3,
            written: 3,
            files,
            shadowed: 2,
            scan_secs: 0.5,
            write_secs: 2.0,
            verify: Some(VerifyReport { files: 2, lines: 3, millis: 1500 }),
            expr: Some("words > 1".into()),
            sample: Some(0.5),
            seed: 7,
            top: Some(4),
            limit: Some(3),
            to_stdout: false,
        };
        let s = rep.render();
        assert!(s.starts_with("Exported 3 documents to 2 files (2.0 MB)\n"), "{s}");
        assert!(s.contains("Scanned 10, matched 5 (words > 1), selected 3 (sample 50.00% seed 7, top 4, limit 3)"), "{s}");
        assert!(s.contains("Scan 0.50 s, fetch and write 2.00 s (1.0 MB/s, 2 docs/s)"), "{s}");
        assert!(s.contains("    /o-0001.jsonl  2 docs  1.5 MB\n"), "{s}");
        assert!(s.contains("Verified: 3 lines parse as JSON in 2 files (1.50 s)"), "{s}");
        assert!(s.contains("Skipped 2 metadata keys"), "{s}");
        let one = ExportReport { files: vec![FileInfo { path: PathBuf::from("/o.jsonl"), docs: 3, bytes: 10 }], verify: None, expr: None, sample: None, top: None, limit: None, shadowed: 0, ..rep.clone() };
        let s = one.render();
        assert!(s.starts_with("Exported 3 documents to /o.jsonl (0.0 MB)\n"), "{s}");
        assert!(s.contains("Scanned 10, matched 5\n"), "{s}");
        assert!(!s.contains("Files:") && !s.contains("Verified") && !s.contains("Skipped"), "{s}");
        let out = ExportReport { to_stdout: true, files: vec![FileInfo { path: PathBuf::new(), docs: 3, bytes: 10 }], ..one };
        assert!(out.render().starts_with("Exported 3 documents to stdout (0.0 MB)\n"));
        let v = VerifyReport { files: 1, lines: 0, millis: 0 };
        let one_file = ExportReport { verify: Some(v), ..rep };
        assert!(one_file.render().contains("in 1 file ("));
    }

    #[test]
    fn verify_file_parses_lines_and_reports_bad_ones() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.jsonl");
        std::fs::write(&good, "{\"a\":1}\n{\"b\":[1,2]}\n").unwrap();
        assert_eq!(verify_file(&good, 2).unwrap(), 2);
        let many = dir.path().join("many.jsonl");
        let body: String = (0..5000).map(|i| format!("{{\"i\":{i}}}\n")).collect();
        std::fs::write(&many, &body).unwrap();
        assert_eq!(verify_file(&many, 3).unwrap(), 5000);
        let bad = dir.path().join("bad.jsonl");
        std::fs::write(&bad, "{\"a\":1}\n{oops\n").unwrap();
        let e = verify_file(&bad, 2).unwrap_err().to_string();
        assert!(e.contains("bad.jsonl:2:"), "{e}");
        let blank = dir.path().join("blank.jsonl");
        std::fs::write(&blank, "{}\n\n{}\n").unwrap();
        let e = verify_file(&blank, 1).unwrap_err().to_string();
        assert!(e.contains("blank.jsonl:2: empty line"), "{e}");
        assert!(verify_file(&dir.path().join("missing.jsonl"), 1).is_err());
        let files = [FileInfo { path: good.clone(), docs: 3, bytes: 0 }];
        let e = verify_files(&files, 1).unwrap_err().to_string();
        assert!(e.contains("2 lines parsed, 3 documents written"), "{e}");
        let files = [FileInfo { path: good, docs: 2, bytes: 0 }];
        let v = verify_files(&files, 1).unwrap();
        assert_eq!((v.files, v.lines), (1, 2));
    }
}
