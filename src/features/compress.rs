//! Compressibility: how small LZ4 makes the text, as a percent of its size.
//!
//! LZ4 is a pure LZ77 coder with no entropy stage, so this number reacts
//! only to repeated byte strings: a paragraph pasted fifty times, a table
//! of near-identical rows, a looping generator. That makes it the
//! complement of `entropy`, which sees only the symbol distribution and is
//! blind to repetition.
//!
//! The value is the LZ4 ratio divided by [`COMPRESS_NORM`], so ordinary
//! period prose reads about 100, a degenerate loop under 10 and random-looking
//! OCR noise about 130.

use lz4_flex::block::{compress_into, get_maximum_output_size};

/// The LZ4 ratio that counts as 100: set so the median document of the clean
/// Vintage-1 store (1.14 M documents) reads about 99.5. LZ4 only finds
/// repeats inside one document, so shorter documents read above 100 and
/// longer ones below: compare documents of similar length.
pub const COMPRESS_NORM: f64 = 0.82;

/// Compressed size over raw size, as basis points of [`COMPRESS_NORM`]
/// (10000 = 100), capped at the u16 range. `buf` is reused across documents.
pub fn compress_ratio(bytes: &[u8], buf: &mut Vec<u8>) -> u16 {
    if bytes.is_empty() {
        return 0;
    }
    let need = get_maximum_output_size(bytes.len());
    if buf.len() < need {
        buf.resize(need, 0);
    }
    let out = compress_into(bytes, buf).expect("buffer sized by get_maximum_output_size");
    let bp = (out as f64 / bytes.len() as f64 / COMPRESS_NORM * 10_000.0).round();
    bp.min(f64::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repetition_prose_and_noise_are_ordered() {
        let mut buf = Vec::new();
        assert_eq!(compress_ratio(b"", &mut buf), 0);
        let looped = "The same sentence again and again. ".repeat(200);
        let prose = "It was the best of times, it was the worst of times, it was the age of wisdom, it was the age of \
foolishness, it was the epoch of belief, it was the epoch of incredulity, it was the season of Light, it was the \
season of Darkness, it was the spring of hope, it was the winter of despair, we had everything before us, we had \
nothing before us, we were all going direct to Heaven, we were all going direct the other way.";
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let noise: String = (0..4000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from(b'!' + (x % 90) as u8)
            })
            .collect();
        let l = compress_ratio(looped.as_bytes(), &mut buf);
        let p = compress_ratio(prose.as_bytes(), &mut buf);
        let n = compress_ratio(noise.as_bytes(), &mut buf);
        assert!(l < 300, "a loop compresses to under 3: {l}");
        assert!((4000..10_000).contains(&p), "prose: {p}");
        assert!(n >= 12_000, "noise does not compress: {n}");
        // the buffer grows once and is reused
        let cap = buf.len();
        compress_ratio(prose.as_bytes(), &mut buf);
        assert_eq!(buf.len(), cap);
        // a short text cannot shrink and the framing shows
        let tiny = compress_ratio(b"abc", &mut buf);
        assert!(tiny > 10_000, "{tiny}");
    }
}
