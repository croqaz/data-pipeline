//! Chunked line reader.
//!
//! The reader thread pulls bytes from a file (decompressing on the fly when
//! the name ends in `.gz`, `.zst` or `.zstd`) and hands out chunks that end
//! on a newline, so workers can split lines with `memchr` and never see a
//! partial record. Lines are numbered from 1 across the whole file.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::error::Result;

/// A block of complete lines. `data` always ends with `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub data: Vec<u8>,
    /// 1-based number of the first line in `data`.
    pub first_line: u64,
    /// Number of lines (newlines) in `data`.
    pub lines: u64,
}

/// Open a JSONL file, transparently decompressing by extension.
pub fn open_input(path: &Path) -> Result<Box<dyn Read + Send>> {
    let file = File::open(path)?;
    let name = path.to_string_lossy();
    if name.ends_with(".gz") {
        Ok(Box::new(flate2::read::MultiGzDecoder::new(BufReader::with_capacity(1 << 20, file))))
    } else if name.ends_with(".zst") || name.ends_with(".zstd") {
        let dec = zstd::Decoder::new(file)?;
        Ok(Box::new(dec))
    } else {
        Ok(Box::new(file))
    }
}

pub struct Chunker<R: Read> {
    reader: R,
    buf: Vec<u8>,
    tmp: Vec<u8>,
    target: usize,
    next_line: u64,
    eof: bool,
}

impl<R: Read> Chunker<R> {
    /// `target` is the chunk size to aim for; a chunk can be larger when a
    /// single line is longer than that.
    pub fn new(reader: R, target: usize) -> Self {
        let target = target.max(1);
        Self {
            reader,
            buf: Vec::with_capacity(target + (1 << 20)),
            tmp: vec![0u8; 1 << 20],
            target,
            next_line: 1,
            eof: false,
        }
    }

    fn fill(&mut self) -> Result<()> {
        let n = self.reader.read(&mut self.tmp)?;
        if n == 0 {
            self.eof = true;
        } else {
            self.buf.extend_from_slice(&self.tmp[..n]);
        }
        Ok(())
    }

    fn emit(&mut self, end: usize) -> Chunk {
        let mut data: Vec<u8> = self.buf.drain(..end).collect();
        if data.last() != Some(&b'\n') {
            data.push(b'\n');
        }
        let lines = memchr::memchr_iter(b'\n', &data).count() as u64;
        let chunk = Chunk { data, first_line: self.next_line, lines };
        self.next_line += lines;
        chunk
    }

    /// The next chunk, or `None` at end of input. Chunks end at the last
    /// newline before `target` bytes; a single line longer than that becomes
    /// a chunk of its own.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        loop {
            if !self.eof && self.buf.len() < self.target {
                self.fill()?;
                continue;
            }
            if self.buf.is_empty() {
                return Ok(None);
            }
            let limit = self.buf.len().min(self.target);
            if let Some(p) = memchr::memrchr(b'\n', &self.buf[..limit]) {
                return Ok(Some(self.emit(p + 1)));
            }
            if let Some(p) = memchr::memchr(b'\n', &self.buf[limit..]) {
                return Ok(Some(self.emit(limit + p + 1)));
            }
            if self.eof {
                let end = self.buf.len();
                return Ok(Some(self.emit(end)));
            }
            // A line longer than everything buffered so far: keep reading.
            self.fill()?;
        }
    }
}

/// Apply a line window (`skip` lines first, then at most `limit` lines) to a
/// chunk. Returns the trimmed chunk (if any lines remain), how many lines
/// were skipped, and whether the limit has been exhausted.
pub fn window(chunk: Chunk, skip: &mut u64, limit: &mut Option<u64>) -> (Option<Chunk>, u64, bool) {
    let mut data = chunk.data;
    let mut first_line = chunk.first_line;
    let mut lines = chunk.lines;
    let mut skipped = 0;
    if *skip > 0 {
        let drop = (*skip).min(lines);
        let mut cut = 0;
        for (i, p) in memchr::memchr_iter(b'\n', &data).enumerate() {
            if (i as u64) + 1 == drop {
                cut = p + 1;
                break;
            }
        }
        data.drain(..cut);
        *skip -= drop;
        skipped = drop;
        first_line += drop;
        lines -= drop;
    }
    let mut exhausted = false;
    if let Some(l) = limit {
        if *l <= lines {
            let keep = *l;
            let mut cut = data.len();
            for (i, p) in memchr::memchr_iter(b'\n', &data).enumerate() {
                if (i as u64) + 1 == keep {
                    cut = p + 1;
                    break;
                }
            }
            data.truncate(cut);
            lines = keep;
            *l = 0;
            exhausted = true;
        } else {
            *l -= lines;
        }
    }
    if lines == 0 {
        return (None, skipped, exhausted);
    }
    (Some(Chunk { data, first_line, lines }), skipped, exhausted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn chunks(input: &[u8], target: usize) -> Vec<Chunk> {
        let mut c = Chunker::new(std::io::Cursor::new(input.to_vec()), target);
        let mut out = Vec::new();
        while let Some(ch) = c.next_chunk().unwrap() {
            out.push(ch);
        }
        out
    }

    #[test]
    fn splits_on_line_boundaries() {
        let out = chunks(b"a\nbb\nccc\ndddd\n", 4);
        let datas: Vec<&[u8]> = out.iter().map(|c| c.data.as_slice()).collect();
        assert_eq!(datas, vec![b"a\n".as_slice(), b"bb\n", b"ccc\n", b"dddd\n"]);
        assert_eq!(out.iter().map(|c| c.first_line).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert!(out.iter().all(|c| c.lines == 1));
        // A bigger target packs several lines per chunk, never past the target.
        let out = chunks(b"a\nbb\nccc\ndddd\n", 6);
        let datas: Vec<&[u8]> = out.iter().map(|c| c.data.as_slice()).collect();
        assert_eq!(datas, vec![b"a\nbb\n".as_slice(), b"ccc\n", b"dddd\n"]);
        assert_eq!((out[0].first_line, out[0].lines, out[1].first_line, out[2].first_line), (1, 2, 3, 4));
    }

    #[test]
    fn missing_final_newline_and_long_lines() {
        let out = chunks(b"no newline at end", 4);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, b"no newline at end\n");
        assert_eq!(out[0].lines, 1);
        let long = vec![b'x'; 5000];
        let mut input = long.clone();
        input.push(b'\n');
        input.extend_from_slice(b"y\n");
        let out = chunks(&input, 16);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data.len(), 5001);
        assert_eq!(out[1].data, b"y\n");
        assert!(chunks(b"", 4).is_empty());
        assert_eq!(chunks(b"\n\n", 4)[0].lines, 2);
        assert_eq!(Chunker::new(std::io::Cursor::new(Vec::new()), 0).target, 1);
    }

    #[test]
    fn reader_errors_propagate() {
        struct Bad;
        impl Read for Bad {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("nope"))
            }
        }
        assert!(Chunker::new(Bad, 4).next_chunk().is_err());
    }

    #[test]
    fn opens_plain_gz_and_zst() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("a.jsonl");
        std::fs::write(&plain, b"{\"text\":1}\n").unwrap();
        let gz = dir.path().join("a.jsonl.gz");
        {
            let f = File::create(&gz).unwrap();
            let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            enc.write_all(b"{\"text\":2}\n").unwrap();
            enc.finish().unwrap();
        }
        let zst = dir.path().join("a.jsonl.zst");
        {
            let f = File::create(&zst).unwrap();
            let mut enc = zstd::Encoder::new(f, 1).unwrap();
            enc.write_all(b"{\"text\":3}\n").unwrap();
            enc.finish().unwrap();
        }
        for (p, want) in [(&plain, "1"), (&gz, "2"), (&zst, "3")] {
            let mut r = open_input(p).unwrap();
            let mut s = String::new();
            r.read_to_string(&mut s).unwrap();
            assert_eq!(s, format!("{{\"text\":{want}}}\n"));
        }
        assert!(open_input(&dir.path().join("missing.jsonl")).is_err());
    }

    #[test]
    fn window_skips_and_limits() {
        let mk = || Chunk { data: b"l1\nl2\nl3\nl4\n".to_vec(), first_line: 1, lines: 4 };
        // skip 2 of 4
        let (c, skipped, done) = window(mk(), &mut 2, &mut None);
        let c = c.unwrap();
        assert_eq!((c.data.as_slice(), c.first_line, c.lines, skipped, done), (b"l3\nl4\n".as_slice(), 3, 2, 2, false));
        // skip more than the chunk has
        let mut skip = 10;
        let (c, skipped, done) = window(mk(), &mut skip, &mut None);
        assert!(c.is_none() && skipped == 4 && !done && skip == 6);
        // limit inside the chunk
        let mut limit = Some(3);
        let (c, _, done) = window(mk(), &mut 0, &mut limit);
        assert_eq!((c.unwrap().data.as_slice(), done, limit), (b"l1\nl2\nl3\n".as_slice(), true, Some(0)));
        // limit exactly the chunk
        let mut limit = Some(4);
        let (c, _, done) = window(mk(), &mut 0, &mut limit);
        assert_eq!((c.unwrap().lines, done), (4, true));
        // limit larger than the chunk
        let mut limit = Some(9);
        let (c, _, done) = window(mk(), &mut 0, &mut limit);
        assert_eq!((c.unwrap().lines, done, limit), (4, false, Some(5)));
        // skip and limit together
        let mut limit = Some(1);
        let (c, skipped, done) = window(mk(), &mut 1, &mut limit);
        assert_eq!((c.unwrap().data.as_slice(), skipped, done), (b"l2\n".as_slice(), 1, true));
        // limit zero yields nothing
        let mut limit = Some(0);
        let (c, _, done) = window(mk(), &mut 0, &mut limit);
        assert!(c.is_none() && done);
    }
}
