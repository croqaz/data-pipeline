//! Compressibility: how small LZ4 makes the text, as a percent of its size.
//!
//! LZ4 is a pure LZ77 coder with no entropy stage, so this number reacts
//! only to repeated byte strings: a paragraph pasted fifty times, a table
//! of near-identical rows, a looping generator. That makes it the
//! complement of `entropy`, which sees only the symbol distribution and is
//! blind to repetition. Ordinary prose lands around 55 to 70 percent; a
//! degenerate loop drops below 10; random-looking OCR noise sits at 100
//! or just above (LZ4 cannot shrink it and adds a little framing).

use lz4_flex::block::{compress_into, get_maximum_output_size};

/// Compressed size over raw size, in basis points (10000 = 100%), capped
/// at the u16 range. `buf` is reused across documents.
pub fn compress_ratio(bytes: &[u8], buf: &mut Vec<u8>) -> u16 {
    if bytes.is_empty() {
        return 0;
    }
    let need = get_maximum_output_size(bytes.len());
    if buf.len() < need {
        buf.resize(need, 0);
    }
    let out = compress_into(bytes, buf).expect("buffer sized by get_maximum_output_size");
    let bp = (out as u64 * 10_000 + bytes.len() as u64 / 2) / bytes.len() as u64;
    bp.min(u64::from(u16::MAX)) as u16
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
        assert!(l < 200, "a loop compresses to under 2%: {l}");
        assert!((3000..8000).contains(&p), "prose: {p}");
        assert!(n >= 9800, "noise does not compress: {n}");
        // the buffer grows once and is reused
        let cap = buf.len();
        compress_ratio(prose.as_bytes(), &mut buf);
        assert_eq!(buf.len(), cap);
        // a short text cannot shrink and the framing shows
        let tiny = compress_ratio(b"abc", &mut buf);
        assert!(tiny > 10_000, "{tiny}");
    }
}
