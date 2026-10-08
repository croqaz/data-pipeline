//! `pipeline import`: JSONL files in, documents in the store.
//!
//! One reader thread cuts files into chunks of whole lines, N workers parse
//! and compute features, one writer owns the store, resolves duplicates and
//! keeps the counts. Workers run unordered; the conflict rule is a total
//! order, so the resulting store does not depend on thread count.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use hashbrown::{HashMap, HashSet};
use sonic_rs::JsonValueTrait;

use crate::error::{Error, Result};
use crate::features::words::IdentityBuild;
use crate::features::{self, DropReason, Features, Scratch};
use crate::hash::{Key, doc_key, raw_hash};
use crate::jsonl::reader::window;
use crate::jsonl::{Chunk, Chunker, open_input};
use crate::meta::Meta;
use crate::store::{self, Record, Sources, Store};
use crate::tokenizer::{self, TokenizerSpec};
use crate::util::{fmt_int, fmt_num, fmt_rate};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OnError {
    /// Count and skip lines that are not usable JSON records.
    Skip,
    /// Stop at the first unusable line (what was indexed so far stays).
    Abort,
}

/// Source label for documents with neither `--source` nor a source key.
pub const DEFAULT_SOURCE: &str = "cli";

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub db: PathBuf,
    /// Label for every document. `None` reads `source_key`, then falls back to `DEFAULT_SOURCE`.
    pub source: Option<String>,
    pub source_key: String,
    pub text_key: String,
    /// Record keys kept as metadata, each with the name it is stored under.
    pub extra: Vec<(String, String)>,
    pub max_len: u32,
    pub workers: usize,
    pub offset: u64,
    pub limit: Option<u64>,
    pub rejects: Option<PathBuf>,
    pub on_error: OnError,
    pub nocow: bool,
    pub chunk_bytes: usize,
    pub batch_size: usize,
    /// Parse, compute and count, but write nothing and never open a store.
    pub dry_run: bool,
    /// `--tokenizer`: count `tokens` with this tokenizer folder. A store that
    /// already has a tokenizer counts with its own without this.
    pub tokenizer: Option<PathBuf>,
    pub inputs: Vec<PathBuf>,
}

impl ImportOptions {
    pub fn new(db: PathBuf, inputs: Vec<PathBuf>) -> Self {
        Self {
            db,
            source: None,
            source_key: "source".into(),
            text_key: "text".into(),
            extra: Vec::new(),
            max_len: features::DEFAULT_MAX_LEN,
            workers: std::thread::available_parallelism().map_or(4, |n| n.get()),
            offset: 0,
            limit: None,
            rejects: None,
            on_error: OnError::Skip,
            nocow: false,
            chunk_bytes: 2 << 20,
            batch_size: 2000,
            dry_run: false,
            tokenizer: None,
            inputs,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// Lines in the file, including skipped and empty ones.
    pub lines: u64,
    /// Lines skipped by `--offset`.
    pub skipped: u64,
    /// Blank lines.
    pub empty: u64,
    /// Lines dropped, by `DropReason` index.
    pub dropped: [u64; 7],
    /// Conflicts where the stored copy stayed.
    pub duplicates: u64,
    /// Conflicts where the new copy won.
    pub replaced: u64,
    /// Stored documents re-imported with the same text and a new source or extra.
    pub updated: u64,
    /// New documents.
    pub indexed: u64,
}

impl Counts {
    pub fn dropped_total(&self) -> u64 {
        self.dropped.iter().sum()
    }

    /// Non-empty lines that reached the parser.
    pub fn loaded(&self) -> u64 {
        self.lines - self.skipped - self.empty
    }

    /// Every line is accounted for exactly once.
    pub fn balanced(&self) -> bool {
        self.lines == self.skipped + self.empty + self.dropped_total() + self.duplicates + self.replaced + self.updated + self.indexed
    }

    pub fn add(&mut self, o: &Counts) {
        self.lines += o.lines;
        self.skipped += o.skipped;
        self.empty += o.empty;
        for (a, b) in self.dropped.iter_mut().zip(o.dropped.iter()) {
            *a += b;
        }
        self.duplicates += o.duplicates;
        self.replaced += o.replaced;
        self.updated += o.updated;
        self.indexed += o.indexed;
    }
}

#[derive(Debug, Clone, Default)]
pub struct FileReport {
    pub path: PathBuf,
    pub counts: Counts,
    pub bytes: u64,
    /// False when `--limit` or an abort stopped the import before this file was opened.
    pub reached: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub files: Vec<FileReport>,
    pub totals: Counts,
    pub bytes: u64,
    pub elapsed: Duration,
    pub stopped_at_limit: bool,
    pub dry_run: bool,
    /// False for a dry run, which never opens a store.
    pub has_store: bool,
    pub approx_docs: u64,
    pub disk_size: u64,
    /// Peak resident memory of the process, bytes.
    pub peak_rss: u64,
    /// Resident memory at the end, split (anonymous, file-backed).
    pub rss_split: Option<(u64, u64)>,
    /// Source names no document uses after this import's updates and replacements.
    pub removed_sources: Vec<String>,
    /// The tokenizer `tokens` was counted with, and whether this import set
    /// it on the store.
    pub tokenizer: Option<(String, bool)>,
}

impl ImportReport {
    pub fn render(&self) -> String {
        let mut s = String::new();
        for f in &self.files {
            if !f.reached {
                s.push_str(&format!("Not reached: {}\n", f.path.display()));
                continue;
            }
            s.push_str(&format!("Imported: {}\n", f.path.display()));
            s.push_str(&format!(
                "  Lines:      {:>12}  ({} skipped by --offset, {} empty)\n",
                fmt_int(f.counts.lines),
                fmt_int(f.counts.skipped),
                fmt_int(f.counts.empty)
            ));
            let reasons: Vec<String> = DropReason::ALL
                .iter()
                .filter(|r| f.counts.dropped[r.index()] > 0)
                .map(|r| format!("{} {}", r.as_str(), fmt_int(f.counts.dropped[r.index()])))
                .collect();
            s.push_str(&format!(
                "  Dropped:    {:>12}  ({})\n",
                fmt_int(f.counts.dropped_total()),
                if reasons.is_empty() { "none".to_string() } else { reasons.join(", ") }
            ));
            s.push_str(&format!("  Duplicates: {:>12}  (stored copy kept)\n", fmt_int(f.counts.duplicates)));
            s.push_str(&format!("  Replaced:   {:>12}  (new copy scored higher)\n", fmt_int(f.counts.replaced)));
            s.push_str(&format!("  Updated:    {:>12}  (same text, new source or extra)\n", fmt_int(f.counts.updated)));
            s.push_str(&format!("  Indexed:    {:>12}\n", fmt_int(f.counts.indexed)));
        }
        let secs = self.elapsed.as_secs_f64();
        let docs = self.totals.indexed + self.totals.replaced;
        s.push_str(&format!(
            "Totals: {} lines, {} indexed, {} replaced, {} updated, {} duplicates, {} dropped\n",
            fmt_int(self.totals.lines),
            fmt_int(self.totals.indexed),
            fmt_int(self.totals.replaced),
            fmt_int(self.totals.updated),
            fmt_int(self.totals.duplicates),
            fmt_int(self.totals.dropped_total())
        ));
        s.push_str(&format!(
            "Elapsed {}s, {}, {} docs/s\n",
            fmt_num(secs, 1),
            fmt_rate(self.bytes, secs),
            fmt_num(if secs > 0.0 { docs as f64 / secs } else { 0.0 }, 0)
        ));
        if self.has_store {
            s.push_str(&format!(
                "Store: about {} documents, {} MB on disk\n",
                fmt_int(self.approx_docs),
                fmt_num(self.disk_size as f64 / 1e6, 1)
            ));
        }
        if !self.removed_sources.is_empty() {
            s.push_str(&format!("Unused sources removed: {}\n", self.removed_sources.join(", ")));
        }
        if let Some((label, set)) = &self.tokenizer {
            // A dry run never opens the store, so it cannot say whose tokenizer it is.
            let how = match (self.dry_run, set) {
                (true, _) => "",
                (false, true) => ", now the store's tokenizer",
                (false, false) => ", the store's tokenizer",
            };
            s.push_str(&format!("Tokens: counted with {label}{how}\n"));
        }
        match self.rss_split {
            Some((anon, file)) => s.push_str(&format!(
                "Peak RSS: {} MB (at the end: {} MB heap, {} MB mapped files)\n",
                fmt_num(self.peak_rss as f64 / 1e6, 1),
                fmt_num(anon as f64 / 1e6, 1),
                fmt_num(file as f64 / 1e6, 1)
            )),
            None => s.push_str(&format!("Peak RSS: {} MB\n", fmt_num(self.peak_rss as f64 / 1e6, 1))),
        }
        if self.stopped_at_limit {
            s.push_str("Stopped at --limit.\n");
        }
        if self.dry_run {
            s.push_str("Dry run: nothing was written.\n");
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Worker side: one line in, one document or one reject out.
// ---------------------------------------------------------------------------

struct WorkerCfg {
    text_key: String,
    source_key: String,
    extra: Vec<(String, String)>,
    max_len: u32,
    keep_raw: bool,
}

struct Doc {
    key: Key,
    features: Features,
    source: Option<String>,
    text: String,
    extra: Option<String>,
}

struct Reject {
    line: u64,
    reason: DropReason,
    raw: Option<Vec<u8>>,
}

struct ChunkResult {
    file: usize,
    lines: u64,
    empty: u64,
    bytes: u64,
    docs: Vec<Doc>,
    rejects: Vec<Reject>,
}

fn decode_str(v: &sonic_rs::LazyValue<'_>) -> std::result::Result<String, DropReason> {
    v.as_str().map(str::to_string).ok_or(DropReason::BadJson)
}

/// Build the raw JSON object holding the kept extra fields.
/// Parse `--extra` items: `key` keeps a record key under its own name,
/// `key=name` stores it under `name`. Each key and each name may appear once.
pub fn parse_extra(items: &[String]) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(items.len());
    for item in items {
        let (from, to) = match item.split_once('=') {
            Some((f, t)) => (f.trim(), t.trim()),
            None => (item.trim(), item.trim()),
        };
        if from.is_empty() || to.is_empty() || to.contains('=') {
            return Err(Error::Usage(format!("--extra {item:?}: expected KEY or KEY=NAME")));
        }
        if out.iter().any(|(f, _)| f == from) {
            return Err(Error::Usage(format!("--extra: key {from:?} is listed twice")));
        }
        if out.iter().any(|(_, t)| t == to) {
            return Err(Error::Usage(format!("--extra: two keys are stored as {to:?}")));
        }
        out.push((from.to_string(), to.to_string()));
    }
    Ok(out)
}

fn extra_json(fields: &[(String, String)]) -> String {
    let mut s = String::from("{");
    for (i, (k, raw)) in fields.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&serde_json::to_string(k).expect("strings always serialize"));
        s.push(':');
        s.push_str(raw);
    }
    s.push('}');
    s
}

fn process_line(line: &[u8], cfg: &WorkerCfg, scratch: &mut Scratch) -> std::result::Result<Option<Doc>, DropReason> {
    let trimmed = line.trim_ascii();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let json = std::str::from_utf8(trimmed).map_err(|_| DropReason::BadJson)?;
    if !json.starts_with('{') {
        return Err(if serde_json::from_str::<serde_json::Value>(json).is_ok() { DropReason::NotObject } else { DropReason::BadJson });
    }
    let mut text: Option<String> = None;
    let mut source: Option<String> = None;
    let mut extras: Vec<(String, String)> = Vec::new();
    for item in sonic_rs::to_object_iter(json) {
        let (k, v) = item.map_err(|_| DropReason::BadJson)?;
        if k == cfg.text_key {
            text = if v.is_str() { Some(decode_str(&v)?) } else { None };
        }
        if !cfg.source_key.is_empty() && k == cfg.source_key && v.is_str() {
            source = Some(decode_str(&v)?);
        }
        if let Some((_, name)) = cfg.extra.iter().find(|(from, _)| *from == k) {
            let raw = v.as_raw_str().to_string();
            match extras.iter_mut().find(|(ek, _)| ek == name) {
                Some(slot) => slot.1 = raw,
                None => extras.push((name.clone(), raw)),
            }
        }
    }
    let text = text.filter(|t| !t.is_empty()).ok_or(DropReason::NoText)?;
    let features = features::compute(&text, cfg.max_len, scratch)?;
    let key = doc_key(&text, &mut scratch.norm);
    let extra = if extras.is_empty() { None } else { Some(extra_json(&extras)) };
    Ok(Some(Doc { key, features, source, text, extra }))
}

fn process_chunk(file: usize, chunk: &Chunk, cfg: &WorkerCfg, scratch: &mut Scratch) -> ChunkResult {
    let mut res = ChunkResult { file, lines: chunk.lines, empty: 0, bytes: chunk.data.len() as u64, docs: Vec::new(), rejects: Vec::new() };
    let mut start = 0;
    for (line_no, p) in (chunk.first_line..).zip(memchr::memchr_iter(b'\n', &chunk.data)) {
        let line = &chunk.data[start..p];
        start = p + 1;
        match process_line(line, cfg, scratch) {
            Ok(None) => res.empty += 1,
            Ok(Some(doc)) => res.docs.push(doc),
            Err(reason) => res.rejects.push(Reject { line: line_no, reason, raw: cfg.keep_raw.then(|| line.to_vec()) }),
        }
    }
    res
}

// ---------------------------------------------------------------------------
// Writer side.
// ---------------------------------------------------------------------------

/// The conflict rule: higher score, then smaller raw-text hash, then the
/// stored copy. A total order, so arrival order never matters.
pub fn new_wins(old_meta: &Meta, old_text: &[u8], new_meta: &Meta, new_text: &[u8]) -> bool {
    let (a, b) = (new_meta.score(), old_meta.score());
    if a != b {
        return a > b;
    }
    if new_text == old_text {
        return false;
    }
    raw_hash(new_text) < raw_hash(old_text)
}

/// Set the btrfs `nodatacow` attribute on a directory (the `chattr +C`
/// equivalent). Only meaningful while the directory is empty.
pub fn set_nocow(dir: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
    const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
    const FS_NOCOW_FL: libc::c_long = 0x0080_0000;
    let f = std::fs::File::open(dir)?;
    let fd = f.as_raw_fd();
    let mut flags: libc::c_long = 0;
    // SAFETY: fd is an open directory and `flags` is a valid c_long out-pointer.
    if unsafe { libc::ioctl(fd, FS_IOC_GETFLAGS, &mut flags as *mut libc::c_long) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    flags |= FS_NOCOW_FL;
    // SAFETY: same fd, `flags` is a valid c_long in-pointer.
    if unsafe { libc::ioctl(fd, FS_IOC_SETFLAGS, &flags as *const libc::c_long) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The reader and the writer share the log: the reader announces each
/// file as it opens it, the writer prints the running counts.
type Log<'l> = Mutex<&'l mut (dyn Write + Send)>;

struct Writer<'a, 'l> {
    store: &'a dyn Store,
    opts: &'a ImportOptions,
    sources: Sources,
    seen: HashSet<u64, IdentityBuild>,
    batch: Vec<Record>,
    pending: HashMap<Key, usize>,
    counts: Vec<Counts>,
    bytes: u64,
    rejects_out: Option<std::io::BufWriter<std::fs::File>>,
    abort: Option<Error>,
    last_progress: u64,
    started: Instant,
    log: &'a Log<'l>,
}

impl Writer<'_, '_> {
    fn loaded_total(&self) -> u64 {
        self.counts.iter().map(Counts::loaded).sum()
    }

    fn flush(&mut self) -> Result<()> {
        if !self.batch.is_empty() {
            self.store.put_batch(&self.batch)?;
            self.batch.clear();
            self.pending.clear();
        }
        Ok(())
    }

    fn add_doc(&mut self, file: usize, doc: Doc) -> Result<()> {
        let label = self.opts.source.as_deref().or(doc.source.as_deref()).unwrap_or(DEFAULT_SOURCE);
        let sid = self.sources.intern(label, self.store)?;
        let meta = doc.features.to_meta(sid, doc.extra.is_some());
        let mut record = Record {
            key: doc.key,
            meta: meta.encode(),
            text: doc.text.into_bytes(),
            extra: doc.extra.map(String::into_bytes),
            replace: false,
        };
        let k8 = u64::from_le_bytes(record.key[..8].try_into().expect("8 bytes"));
        let counts = &mut self.counts[file];
        if self.seen.insert(k8) {
            self.pending.insert(record.key, self.batch.len());
            self.batch.push(record);
            counts.indexed += 1;
        } else if let Some(&i) = self.pending.get(&record.key) {
            let old_meta = Meta::decode(&self.batch[i].meta)?;
            if new_wins(&old_meta, &self.batch[i].text, &meta, &record.text) {
                // inherit whether the slot overwrites a stored document
                record.replace = self.batch[i].replace;
                self.batch[i] = record;
                counts.replaced += 1;
            } else {
                counts.duplicates += 1;
            }
        } else if let Some(old_bytes) = self.store.get_meta(&record.key)? {
            let old_meta = Meta::decode(&old_bytes)?;
            let old_text = self.store.get_text(&record.key)?.unwrap_or_default();
            if new_wins(&old_meta, &old_text, &meta, &record.text) {
                record.replace = true;
                self.pending.insert(record.key, self.batch.len());
                self.batch.push(record);
                counts.replaced += 1;
            } else if old_text == record.text
                && (old_meta.source != meta.source || (record.extra.is_some() && self.store.get_extra(&record.key)? != record.extra))
            {
                // The same document imported again: its newer labels win. An
                // import without extras keeps the stored ones.
                if record.extra.is_none() {
                    record.extra = self.store.get_extra(&record.key)?;
                    record.meta = doc.features.to_meta(sid, record.extra.is_some()).encode();
                }
                record.replace = true;
                self.pending.insert(record.key, self.batch.len());
                self.batch.push(record);
                counts.updated += 1;
            } else {
                counts.duplicates += 1;
            }
        } else {
            // The 8-byte prefix collided with another key: a new document.
            self.pending.insert(record.key, self.batch.len());
            self.batch.push(record);
            counts.indexed += 1;
        }
        if self.batch.len() >= self.opts.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    fn add_reject(&mut self, file: usize, r: &Reject) -> Result<()> {
        self.counts[file].dropped[r.reason.index()] += 1;
        if let Some(out) = self.rejects_out.as_mut() {
            let line = serde_json::json!({
                "file": self.opts.inputs[file].display().to_string(),
                "line": r.line,
                "reason": r.reason.as_str(),
                "raw": r.raw.as_deref().map(String::from_utf8_lossy),
            });
            serde_json::to_writer(&mut *out, &line)?;
            out.write_all(b"\n")?;
        }
        if self.opts.on_error == OnError::Abort && r.reason.is_parse_error() && self.abort.is_none() {
            self.abort = Some(Error::Aborted(format!("{}:{}: {}", self.opts.inputs[file].display(), r.line, r.reason.as_str())));
        }
        Ok(())
    }

    fn consume(&mut self, res: ChunkResult) -> Result<()> {
        self.counts[res.file].lines += res.lines;
        self.counts[res.file].empty += res.empty;
        self.bytes += res.bytes;
        for r in &res.rejects {
            self.add_reject(res.file, r)?;
        }
        for doc in res.docs {
            self.add_doc(res.file, doc)?;
        }
        let loaded = self.loaded_total();
        if loaded / 100_000 > self.last_progress / 100_000 {
            self.last_progress = loaded;
            let secs = self.started.elapsed().as_secs_f64();
            let indexed: u64 = self.counts.iter().map(|c| c.indexed + c.replaced).sum();
            let mut log = self.log.lock().expect("log lock");
            writeln!(log, "  {} lines, {} indexed, {}", fmt_int(loaded), fmt_int(indexed), fmt_rate(self.bytes, secs))?;
        }
        Ok(())
    }
}

/// What the reader learned, per input file.
struct ReaderOutcome {
    skipped: Vec<u64>,
    sizes: Vec<u64>,
    reached: Vec<bool>,
    hit_limit: bool,
}

fn reader_loop(
    opts: &ImportOptions,
    tx: &crossbeam_channel::Sender<(usize, Chunk)>,
    stop: &AtomicBool,
    log: &Log<'_>,
) -> Result<ReaderOutcome> {
    let n = opts.inputs.len();
    let mut out = ReaderOutcome { skipped: vec![0; n], sizes: vec![0; n], reached: vec![false; n], hit_limit: false };
    let mut skip = opts.offset;
    let mut limit = opts.limit;
    'files: for (i, path) in opts.inputs.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        writeln!(log.lock().expect("log lock"), "Importing: {}", path.display())?;
        out.reached[i] = true;
        out.sizes[i] = std::fs::metadata(path)?.len();
        let reader = open_input(path)?;
        let mut chunker = Chunker::new(reader, opts.chunk_bytes);
        while let Some(chunk) = chunker.next_chunk()? {
            if stop.load(Ordering::Relaxed) {
                break 'files;
            }
            let (chunk, skipped_now, exhausted) = window(chunk, &mut skip, &mut limit);
            out.skipped[i] += skipped_now;
            if let Some(chunk) = chunk
                && tx.send((i, chunk)).is_err()
            {
                break 'files;
            }
            if exhausted {
                out.hit_limit = true;
                break 'files;
            }
        }
    }
    Ok(out)
}

/// Run an import. Progress and warnings go to `log`: one `Importing:`
/// line as each file is opened, then a count every 100,000 lines.
pub fn import(opts: &ImportOptions, log: &mut (dyn Write + Send)) -> Result<ImportReport> {
    if opts.inputs.is_empty() {
        return Err(Error::Usage("at least one input file is required".into()));
    }
    for p in &opts.inputs {
        if !p.is_file() {
            return Err(Error::Usage(format!("input {} is not a file", p.display())));
        }
    }
    let workers = opts.workers.max(1);
    let started = Instant::now();
    let given = opts.tokenizer.as_deref().map(|p| TokenizerSpec::from_path(p).map(|l| (p, l))).transpose()?;

    let store: Box<dyn Store> = if opts.dry_run {
        Box::new(store::NullStore::default())
    } else {
        let fresh = !opts.db.exists() || std::fs::read_dir(&opts.db)?.next().is_none();
        if fresh {
            std::fs::create_dir_all(&opts.db)?;
            if opts.nocow {
                match set_nocow(&opts.db) {
                    Ok(()) => writeln!(log, "Set nodatacow on {}", opts.db.display())?,
                    Err(e) => writeln!(log, "Warning: could not set nodatacow on {}: {e}", opts.db.display())?,
                }
            }
        }
        store::open(&opts.db, true)?
    };

    let mut seen: HashSet<u64, IdentityBuild> = HashSet::with_hasher(IdentityBuild);
    store.scan_meta(&[0; 16], None, &mut |k, _| {
        seen.insert(u64::from_le_bytes(k[..8].try_into().expect("8 bytes")));
        Ok(true)
    })?;
    let sources = Sources::load(store.as_ref())?;
    let state = tokenizer::State::read(store.as_ref())?;
    let tokenizer = tokenizer::for_import(&opts.db, &state, given, seen.len() as u64)?;
    if let Some((loaded, true)) = &tokenizer {
        // Set before the first document is written, so every stored count
        // has its tokenizer on record even if the import stops early.
        store.put_setting(tokenizer::CURRENT, Some(&loaded.spec.encode()))?;
        store.persist()?;
    }
    if let Some((loaded, _)) = &tokenizer {
        writeln!(log, "Counting tokens with {}", loaded.spec.label())?;
    }
    let shared_tokenizer = tokenizer.as_ref().map(|(l, _)| l.tokenizer.clone());
    let rejects_out = match &opts.rejects {
        Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p)?)),
        None => None,
    };

    let cfg = WorkerCfg {
        text_key: opts.text_key.clone(),
        source_key: if opts.source.is_some() { String::new() } else { opts.source_key.clone() },
        extra: opts.extra.clone(),
        max_len: opts.max_len,
        keep_raw: opts.rejects.is_some() || opts.on_error == OnError::Abort,
    };
    let stop = AtomicBool::new(false);
    let log: Log<'_> = Mutex::new(log);
    let (chunk_tx, chunk_rx) = bounded::<(usize, Chunk)>(workers * 2);
    let (res_tx, res_rx) = bounded::<ChunkResult>(workers * 4);

    let mut writer = Writer {
        store: store.as_ref(),
        opts,
        sources,
        seen,
        batch: Vec::with_capacity(opts.batch_size),
        pending: HashMap::new(),
        counts: vec![Counts::default(); opts.inputs.len()],
        bytes: 0,
        rejects_out,
        abort: None,
        last_progress: 0,
        started,
        log: &log,
    };

    let (writer_result, reader_result) = std::thread::scope(|s| {
        let stop_ref = &stop;
        let log_ref = &log;
        let reader = s.spawn(move || reader_loop(opts, &chunk_tx, stop_ref, log_ref));
        for _ in 0..workers {
            let rx: Receiver<(usize, Chunk)> = chunk_rx.clone();
            let tx = res_tx.clone();
            let cfg = &cfg;
            let shared_tokenizer = shared_tokenizer.clone();
            s.spawn(move || {
                let mut scratch = Scratch { tokenizer: shared_tokenizer, ..Scratch::default() };
                for (file, chunk) in rx.iter() {
                    let res = process_chunk(file, &chunk, cfg, &mut scratch);
                    if tx.send(res).is_err() {
                        break;
                    }
                }
            });
        }
        drop(chunk_rx);
        drop(res_tx);
        let writer_result: Result<()> = (|| {
            for res in res_rx.iter() {
                writer.consume(res)?;
                if writer.abort.is_some() {
                    stop.store(true, Ordering::Relaxed);
                }
            }
            Ok(())
        })();
        if writer_result.is_err() {
            stop.store(true, Ordering::Relaxed);
        }
        let reader_result = reader.join().expect("reader thread panicked");
        (writer_result, reader_result)
    });

    // Whatever happened, keep what was indexed.
    let flush_result = writer.flush().and_then(|()| {
        if let Some(out) = writer.rejects_out.as_mut() {
            out.flush()?;
        }
        store.persist()
    });
    writer_result?;
    flush_result?;
    let files = reader_result?;
    if let Some(e) = writer.abort.take() {
        return Err(e);
    }

    let totals = writer.counts.iter().fold(Counts::default(), |mut a, c| {
        a.add(c);
        a
    });
    let removed_sources =
        if opts.dry_run || totals.replaced + totals.updated == 0 { Vec::new() } else { store::prune_sources(store.as_ref())? };
    let mut report = ImportReport {
        removed_sources,
        elapsed: started.elapsed(),
        bytes: writer.bytes,
        stopped_at_limit: files.hit_limit,
        dry_run: opts.dry_run,
        has_store: !opts.dry_run,
        approx_docs: store.approx_count()?,
        disk_size: store.disk_size()?,
        peak_rss: crate::util::peak_rss_bytes(),
        rss_split: crate::util::rss_breakdown(),
        tokenizer: tokenizer.as_ref().map(|(l, set)| (l.spec.label(), *set)),
        ..Default::default()
    };
    for (i, path) in opts.inputs.iter().enumerate() {
        let mut counts = writer.counts[i].clone();
        counts.lines += files.skipped[i];
        counts.skipped = files.skipped[i];
        report.totals.add(&counts);
        report.files.push(FileReport { path: path.clone(), counts, bytes: files.sizes[i], reached: files.reached[i] });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WorkerCfg {
        WorkerCfg {
            text_key: "text".into(),
            source_key: "source".into(),
            extra: parse_extra(&["year".into(), "title".into()]).unwrap(),
            max_len: 10_000,
            keep_raw: true,
        }
    }

    const GOOD: &str =
        "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! How vexingly quick daft zebras jump.";

    #[test]
    fn parse_line_paths() {
        let mut sc = Scratch::default();
        let c = cfg();
        assert!(process_line(b"   ", &c, &mut sc).unwrap().is_none());
        assert_eq!(process_line(b"{not json", &c, &mut sc).err(), Some(DropReason::BadJson));
        assert_eq!(process_line(b"[1, 2]", &c, &mut sc).err(), Some(DropReason::NotObject));
        assert_eq!(process_line(b"garbage", &c, &mut sc).err(), Some(DropReason::BadJson));
        assert_eq!(process_line(b"\xff\xfe{", &c, &mut sc).err(), Some(DropReason::BadJson));
        assert_eq!(process_line(b"{\"x\": 1}", &c, &mut sc).err(), Some(DropReason::NoText));
        assert_eq!(process_line(b"{\"text\": 5}", &c, &mut sc).err(), Some(DropReason::NoText));
        assert_eq!(process_line(b"{\"text\": \"\"}", &c, &mut sc).err(), Some(DropReason::NoText));
        assert_eq!(process_line(b"{\"text\": \"short\"}", &c, &mut sc).err(), Some(DropReason::TooShort));
        let line = format!(
            "{{\"text\": \"{GOOD}\", \"source\": \"gut\", \"year\": 1842, \"title\": \"A \\\"B\\\"\", \"skip\": true, \"year\": 1843}}"
        );
        let doc = process_line(line.as_bytes(), &c, &mut sc).unwrap().unwrap();
        assert_eq!(doc.text, GOOD);
        assert_eq!(doc.source.as_deref(), Some("gut"));
        assert_eq!(doc.extra.as_deref(), Some("{\"year\":1843,\"title\":\"A \\\"B\\\"\"}"));
        assert_eq!(doc.features.words, 23);
        // escaped text decodes; non-string source is ignored; no extras -> None
        let line = format!("{{\"source\": 7, \"text\": \"{GOOD}\\n\\u00e9\"}}");
        let doc = process_line(line.as_bytes(), &c, &mut sc).unwrap().unwrap();
        assert!(doc.text.ends_with("\né"));
        assert!(doc.source.is_none());
        assert!(doc.extra.is_none());
        // a later non-string text key wins (last occurrence)
        let line = format!("{{\"text\": \"{GOOD}\", \"text\": 1}}");
        assert_eq!(process_line(line.as_bytes(), &c, &mut sc).err(), Some(DropReason::NoText));
        // source key disabled
        let c2 = WorkerCfg { source_key: String::new(), ..cfg() };
        let line = format!("{{\"text\": \"{GOOD}\", \"source\": \"gut\"}}");
        assert!(process_line(line.as_bytes(), &c2, &mut sc).unwrap().unwrap().source.is_none());
        // broken escape inside a string is bad json
        let line = format!("{{\"text\": \"{GOOD}\\q\"}}");
        assert_eq!(process_line(line.as_bytes(), &c, &mut sc).err(), Some(DropReason::BadJson));
    }

    #[test]
    fn chunk_processing_keeps_line_numbers() {
        let mut sc = Scratch::default();
        let c = cfg();
        let data = format!("{{\"text\": \"{GOOD}\"}}\n\nbad\n{{\"text\": \"{GOOD} again\"}}\n");
        let chunk = Chunk { data: data.into_bytes(), first_line: 10, lines: 4 };
        let res = process_chunk(0, &chunk, &c, &mut sc);
        assert_eq!((res.lines, res.empty, res.docs.len(), res.rejects.len()), (4, 1, 2, 1));
        assert_eq!(res.rejects[0].line, 12);
        assert_eq!(res.rejects[0].reason, DropReason::BadJson);
        assert_eq!(res.rejects[0].raw.as_deref(), Some(b"bad".as_slice()));
        let c2 = WorkerCfg { keep_raw: false, ..cfg() };
        let res = process_chunk(0, &chunk, &c2, &mut sc);
        assert!(res.rejects[0].raw.is_none());
    }

    #[test]
    fn conflict_rule_is_a_total_order() {
        let hi = Meta { words: 10, ..Default::default() };
        let lo = Meta { words: 5, ..Default::default() };
        assert!(new_wins(&lo, b"a", &hi, b"b"));
        assert!(!new_wins(&hi, b"a", &lo, b"b"));
        assert!(!new_wins(&hi, b"same", &hi, b"same"));
        let a_wins = new_wins(&hi, b"x", &hi, b"y");
        let b_wins = new_wins(&hi, b"y", &hi, b"x");
        assert_ne!(a_wins, b_wins);
    }

    #[test]
    fn counts_balance() {
        let mut c =
            Counts { lines: 10, skipped: 2, empty: 1, dropped: [1, 0, 0, 1, 0, 0, 0], duplicates: 1, replaced: 1, updated: 0, indexed: 3 };
        assert!(c.balanced());
        assert_eq!(c.loaded(), 7);
        assert_eq!(c.dropped_total(), 2);
        c.add(&c.clone());
        assert_eq!(c.lines, 20);
        assert!(c.balanced());
        c.indexed += 1;
        assert!(!c.balanced());
    }

    #[test]
    fn extra_keys_and_renames() {
        let p = |items: &[&str]| parse_extra(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let got = p(&["year", "Publication=title", " titleCode = pubID "]).unwrap();
        let want = [("year", "year"), ("Publication", "title"), ("titleCode", "pubID")];
        assert_eq!(got, want.map(|(a, b)| (a.to_string(), b.to_string())));
        for bad in [&["=x"][..], &["x="], &["a=b=c"], &[""], &["a", "a=b"], &["a=t", "b=t"], &["title", "Publication=title"]] {
            assert!(matches!(p(bad), Err(Error::Usage(_))), "{bad:?}");
        }
        let mut sc = Scratch::default();
        let c = WorkerCfg { extra: p(&["Publication=title", "year"]).unwrap(), ..cfg() };
        let line = format!("{{\"text\": \"{GOOD}\", \"year\": 1700, \"title\": \"ignored\", \"Publication\": \"Gazette\"}}");
        let doc = process_line(line.as_bytes(), &c, &mut sc).unwrap().unwrap();
        assert_eq!(doc.extra.as_deref(), Some("{\"year\":1700,\"title\":\"Gazette\"}"));
    }

    #[test]
    fn extra_json_escapes_keys() {
        assert_eq!(extra_json(&[("a\"b".into(), "1".into()), ("c".into(), "\"x\"".into())]), "{\"a\\\"b\":1,\"c\":\"x\"}");
    }

    #[test]
    fn nocow_outcomes() {
        // tmpfs rejects the ioctl; the cargo target dir (btrfs on the target
        // machine) accepts it. Either way the call must not panic.
        let tmp = tempfile::tempdir().unwrap();
        let _ = set_nocow(tmp.path());
        // The crate directory is btrfs on the target machine.
        let local = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let _ = set_nocow(local.path());
        assert!(set_nocow(&tmp.path().join("missing")).is_err());
    }

    #[test]
    fn writer_handles_prefix_collisions_and_in_batch_replace() {
        let store = crate::store::NullStore::default();
        let opts = ImportOptions::new("x".into(), vec!["in.jsonl".into()]);
        let mut sink = Vec::new();
        let log: Log<'_> = Mutex::new(&mut sink);
        let mut w = Writer {
            store: &store,
            opts: &opts,
            sources: Sources::default(),
            seen: HashSet::with_hasher(IdentityBuild),
            batch: Vec::new(),
            pending: HashMap::new(),
            counts: vec![Counts::default()],
            bytes: 0,
            rejects_out: None,
            abort: None,
            last_progress: 0,
            started: Instant::now(),
            log: &log,
        };
        let mut sc = Scratch::default();
        let mk = |text: &str, sc: &mut Scratch| {
            let features = features::compute(text, 1_000_000, sc).unwrap();
            Doc { key: doc_key(text, &mut sc.norm), features, source: None, text: text.to_string(), extra: None }
        };
        let fewer = format!("{GOOD} and quick-brown ends here");
        let more = format!("{GOOD} and quick brown ends here");
        // Pretend another key shares the 8-byte prefix: still a new document.
        let d = mk(&fewer, &mut sc);
        w.seen.insert(u64::from_le_bytes(d.key[..8].try_into().unwrap()));
        w.add_doc(0, d).unwrap();
        assert_eq!(w.counts[0].indexed, 1);
        assert!(!w.batch[0].replace, "a fresh document never writes an extra tombstone");
        // Same key, higher score, still in the pending batch: replaced in place.
        w.add_doc(0, mk(&more, &mut sc)).unwrap();
        assert_eq!((w.counts[0].replaced, w.batch.len()), (1, 1));
        assert!(String::from_utf8(w.batch[0].text.clone()).unwrap().contains("quick brown ends"));
        assert!(!w.batch[0].replace, "an in-batch replacement of a fresh document is still fresh");
        // Lower score again: kept as a duplicate.
        w.add_doc(0, mk(&fewer, &mut sc)).unwrap();
        assert_eq!(w.counts[0].duplicates, 1);
        w.flush().unwrap();
        assert!(w.batch.is_empty() && w.pending.is_empty());
    }

    #[test]
    fn report_renders_every_section() {
        let mut rep = ImportReport {
            elapsed: Duration::from_secs(2),
            bytes: 4_000_000,
            stopped_at_limit: true,
            has_store: true,
            ..Default::default()
        };
        let mut counts = Counts { lines: 5, skipped: 1, empty: 1, indexed: 2, ..Default::default() };
        counts.dropped[DropReason::BadJson.index()] = 1;
        rep.totals.add(&counts);
        rep.files.push(FileReport { path: "a.jsonl".into(), counts, bytes: 4_000_000, reached: true });
        rep.files.push(FileReport { path: "b.jsonl".into(), ..Default::default() });
        let s = rep.render();
        assert!(s.contains("Imported: a.jsonl\n  Lines:"), "{s}");
        assert!(s.contains("Not reached: b.jsonl\nTotals:"), "an unopened file gets no counts: {s}");
        assert!(s.contains("bad_json 1"));
        assert!(s.contains("2.0 MB/s"));
        assert!(s.contains("Stopped at --limit"));
        assert!(s.contains("Store: about 0 documents"));
        let empty = ImportReport::default().render();
        assert!(empty.contains("Totals: 0 lines"));
        let none =
            ImportReport { files: vec![FileReport { reached: true, ..Default::default() }], dry_run: true, ..Default::default() }.render();
        assert!(none.contains("(none)") && none.contains("Dry run"));
    }
}
