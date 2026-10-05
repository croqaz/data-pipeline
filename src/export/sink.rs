//! Where exported lines go: stdout, one file, or numbered shards, each
//! written as `<name>.partial`, fsynced and renamed when complete, so a
//! file with the final name is always a complete file.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const BUF_SIZE: usize = 8 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub path: PathBuf,
    pub docs: u64,
    pub bytes: u64,
}

/// Output naming, decided before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Naming {
    stem: PathBuf,
    ext: String,
    pub zstd: bool,
    pub shard: Option<u64>,
}

impl Naming {
    /// `out` is the user's path. `.zst` at the end turns zstd on; `--zstd`
    /// without the extension adds it. Shards go in `<stem>-0001<ext>`.
    pub fn new(out: &Path, zstd_flag: bool, shard: Option<u64>) -> Naming {
        let name = out.to_string_lossy().into_owned();
        let (base, zstd) = match name.strip_suffix(".zst") {
            Some(b) => (b.to_string(), true),
            None => (name, zstd_flag),
        };
        let (stem, ext) = match base.rfind('.') {
            Some(i) if i > base.rfind('/').map_or(0, |s| s + 1) => (base[..i].to_string(), base[i..].to_string()),
            _ => (base, String::new()),
        };
        Naming { stem: PathBuf::from(stem), ext, zstd, shard }
    }

    fn suffix(&self) -> String {
        if self.zstd { format!("{}.zst", self.ext) } else { self.ext.clone() }
    }

    /// Final path of file number `n` (1-based; ignored without sharding).
    pub fn path(&self, n: u64) -> PathBuf {
        let s = self.stem.to_string_lossy();
        if self.shard.is_some() {
            PathBuf::from(format!("{s}-{n:04}{}", self.suffix()))
        } else {
            PathBuf::from(format!("{s}{}", self.suffix()))
        }
    }

    /// How many files `docs` documents produce.
    pub fn file_count(&self, docs: u64) -> u64 {
        match self.shard {
            Some(n) => docs.div_ceil(n).max(1),
            None => 1,
        }
    }

    /// Refuse to overwrite: every final path must be absent unless `force`.
    pub fn check_targets(&self, docs: u64, force: bool) -> Result<()> {
        if force {
            return Ok(());
        }
        for n in 1..=self.file_count(docs) {
            let p = self.path(n);
            if p.exists() {
                return Err(Error::Usage(format!("{} exists; use --force to overwrite", p.display())));
            }
        }
        Ok(())
    }
}

enum Out {
    Plain(BufWriter<File>),
    Zstd(zstd::Encoder<'static, BufWriter<File>>),
}

struct Open {
    out: Out,
    partial: PathBuf,
    path: PathBuf,
    docs: u64,
}

pub struct Sink<'a> {
    naming: Option<Naming>,
    stdout: Option<&'a mut dyn Write>,
    stdout_bytes: u64,
    stdout_docs: u64,
    zstd_level: i32,
    zstd_threads: u32,
    current: Option<Open>,
    files: Vec<FileInfo>,
}

impl<'a> Sink<'a> {
    pub fn files(naming: Naming, zstd_level: i32, zstd_threads: usize) -> Sink<'a> {
        Sink {
            naming: Some(naming),
            stdout: None,
            stdout_bytes: 0,
            stdout_docs: 0,
            zstd_level,
            zstd_threads: zstd_threads.clamp(1, 64) as u32,
            current: None,
            files: Vec::new(),
        }
    }

    pub fn stdout(w: &'a mut dyn Write) -> Sink<'a> {
        Sink {
            naming: None,
            stdout: Some(w),
            stdout_bytes: 0,
            stdout_docs: 0,
            zstd_level: 0,
            zstd_threads: 1,
            current: None,
            files: Vec::new(),
        }
    }

    fn open(&mut self, n: u64) -> Result<()> {
        let naming = self.naming.as_ref().expect("file sink");
        let path = naming.path(n);
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let partial = PathBuf::from(format!("{}.partial", path.display()));
        let file = BufWriter::with_capacity(BUF_SIZE, File::create(&partial)?);
        let out = if naming.zstd {
            let mut enc = zstd::Encoder::new(file, self.zstd_level)?;
            enc.multithread(self.zstd_threads)?;
            Out::Zstd(enc)
        } else {
            Out::Plain(file)
        };
        self.current = Some(Open { out, partial, path, docs: 0 });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let Some(open) = self.current.take() else { return Ok(()) };
        let mut file = match open.out {
            Out::Plain(w) => w.into_inner().map_err(|e| Error::Io(e.into_error()))?,
            Out::Zstd(enc) => enc.finish()?.into_inner().map_err(|e| Error::Io(e.into_error()))?,
        };
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&open.partial, &open.path)?;
        let bytes = std::fs::metadata(&open.path)?.len();
        self.files.push(FileInfo { path: open.path, docs: open.docs, bytes });
        Ok(())
    }

    /// Write `buf`, which holds exactly `docs` newline-terminated lines.
    pub fn write(&mut self, buf: &[u8], docs: u64) -> Result<()> {
        if let Some(w) = self.stdout.as_deref_mut() {
            w.write_all(buf)?;
            self.stdout_bytes += buf.len() as u64;
            self.stdout_docs += docs;
            return Ok(());
        }
        let shard = self.naming.as_ref().and_then(|n| n.shard);
        let mut rest = buf;
        let mut left = docs;
        while left > 0 || (rest.is_empty() && self.current.is_none()) {
            if self.current.is_none() {
                let n = self.files.len() as u64 + 1;
                self.open(n)?;
            }
            let open = self.current.as_mut().expect("opened above");
            let room = shard.map_or(u64::MAX, |s| s - open.docs);
            if room == 0 {
                self.close()?;
                continue;
            }
            let (chunk, n) = if left <= room {
                (rest, left)
            } else {
                let cut = memchr::memchr_iter(b'\n', rest).nth(room as usize - 1).expect("buffer holds `docs` lines") + 1;
                (&rest[..cut], room)
            };
            match &mut open.out {
                Out::Plain(w) => w.write_all(chunk)?,
                Out::Zstd(w) => w.write_all(chunk)?,
            }
            open.docs += n;
            rest = &rest[chunk.len()..];
            left -= n;
            if left == 0 {
                break;
            }
        }
        Ok(())
    }

    /// Close the last file. Returns one entry per file written (one entry
    /// with an empty path for stdout).
    pub fn finish(mut self) -> Result<Vec<FileInfo>> {
        if let Some(w) = self.stdout.as_deref_mut() {
            w.flush()?;
            return Ok(vec![FileInfo { path: PathBuf::new(), docs: self.stdout_docs, bytes: self.stdout_bytes }]);
        }
        if self.current.is_none() && self.files.is_empty() {
            // nothing matched: still leave an empty, complete file behind
            self.open(1)?;
        }
        self.close()?;
        Ok(self.files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming_rules() {
        let n = Naming::new(Path::new("/x/out.jsonl"), false, None);
        assert_eq!(n.path(1), PathBuf::from("/x/out.jsonl"));
        assert!(!n.zstd);
        let n = Naming::new(Path::new("/x/out.jsonl"), true, Some(10));
        assert_eq!(n.path(3), PathBuf::from("/x/out-0003.jsonl.zst"));
        let n = Naming::new(Path::new("out.jsonl.zst"), false, None);
        assert!(n.zstd);
        assert_eq!(n.path(1), PathBuf::from("out.jsonl.zst"));
        let n = Naming::new(Path::new("/a.b/noext"), false, Some(1));
        assert_eq!(n.path(12345), PathBuf::from("/a.b/noext-12345"));
        assert_eq!(Naming::new(Path::new("v.jsonl"), false, Some(4)).file_count(9), 3);
        assert_eq!(Naming::new(Path::new("v.jsonl"), false, Some(4)).file_count(0), 1);
        assert_eq!(Naming::new(Path::new("v.jsonl"), false, None).file_count(1000), 1);
    }

    #[test]
    fn check_targets_refuses_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let n = Naming::new(&dir.path().join("o.jsonl"), false, Some(2));
        n.check_targets(5, false).unwrap();
        std::fs::write(n.path(3), "x").unwrap();
        let e = n.check_targets(5, false).unwrap_err().to_string();
        assert!(e.contains("o-0003.jsonl exists"), "{e}");
        n.check_targets(4, false).unwrap();
        n.check_targets(5, true).unwrap();
    }

    fn lines(n: u64) -> Vec<u8> {
        lines_from(0, n)
    }

    fn lines_from(start: u64, n: u64) -> Vec<u8> {
        (start..start + n).map(|i| format!("{{\"i\":{i}}}\n")).collect::<String>().into_bytes()
    }

    #[test]
    fn shards_split_inside_a_buffer_and_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let n = Naming::new(&dir.path().join("o.jsonl"), false, Some(3));
        let mut s = Sink::files(n.clone(), 3, 1);
        s.write(&lines_from(0, 2), 2).unwrap(); // file 1: 2 docs
        s.write(&lines_from(2, 5), 5).unwrap(); // file 1 gets 1, file 2 gets 3, file 3 gets 1
        s.write(&[], 0).unwrap(); // a no-op
        let files = s.finish().unwrap();
        assert_eq!(files.iter().map(|f| f.docs).collect::<Vec<_>>(), vec![3, 3, 1]);
        assert_eq!(files[0].path, n.path(1));
        for f in &files {
            let text = std::fs::read_to_string(&f.path).unwrap();
            assert_eq!(text.lines().count() as u64, f.docs);
            assert_eq!(text.len() as u64, f.bytes);
            assert!(!PathBuf::from(format!("{}.partial", f.path.display())).exists());
        }
        assert_eq!(std::fs::read_to_string(n.path(1)).unwrap(), "{\"i\":0}\n{\"i\":1}\n{\"i\":2}\n");
        assert_eq!(std::fs::read_to_string(n.path(2)).unwrap(), "{\"i\":3}\n{\"i\":4}\n{\"i\":5}\n");
        assert_eq!(std::fs::read_to_string(n.path(3)).unwrap(), "{\"i\":6}\n");
    }

    #[test]
    fn zstd_round_trip_and_empty_output() {
        let dir = tempfile::tempdir().unwrap();
        let n = Naming::new(&dir.path().join("o.jsonl"), true, None);
        let mut s = Sink::files(n.clone(), 1, 2);
        s.write(&lines(100), 100).unwrap();
        let files = s.finish().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].docs, 100);
        let raw = std::fs::read(&files[0].path).unwrap();
        assert_eq!(raw.len() as u64, files[0].bytes);
        assert_eq!(zstd::decode_all(&raw[..]).unwrap(), lines(100));

        let n = Naming::new(&dir.path().join("empty.jsonl"), false, Some(10));
        let files = Sink::files(n.clone(), 3, 1).finish().unwrap();
        assert_eq!(files, vec![FileInfo { path: n.path(1), docs: 0, bytes: 0 }]);
        assert!(n.path(1).exists());
    }

    #[test]
    fn stdout_sink_counts() {
        let mut buf = Vec::new();
        let mut s = Sink::stdout(&mut buf);
        s.write(&lines(3), 3).unwrap();
        let files = s.finish().unwrap();
        assert_eq!(files, vec![FileInfo { path: PathBuf::new(), docs: 3, bytes: lines(3).len() as u64 }]);
        assert_eq!(buf, lines(3));
    }

    #[test]
    fn missing_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let n = Naming::new(&dir.path().join("deep/er/o.jsonl"), false, None);
        let mut s = Sink::files(n.clone(), 3, 1);
        s.write(&lines(1), 1).unwrap();
        s.finish().unwrap();
        assert!(n.path(1).exists());
    }
}
