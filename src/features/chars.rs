//! Character-level counts: scalar count, distinct scalars, the Shannon
//! entropy of the character distribution, the `quality` score and the
//! non-blank line count.

use hashbrown::HashMap;

/// Unicode scalar values in a UTF-8 string: every byte that is not a
/// continuation byte starts a character.
pub fn count_chars(bytes: &[u8]) -> u32 {
    bytes.iter().filter(|&&b| (b & 0xC0) != 0x80).count() as u32
}

/// Scratch space for character frequencies, reused across documents.
pub struct CharFreq {
    latin1: [u32; 256],
    others: HashMap<u32, u32>,
    counts: Vec<u32>,
}

impl Default for CharFreq {
    fn default() -> Self {
        CharFreq { latin1: [0; 256], others: HashMap::new(), counts: Vec::new() }
    }
}

impl CharFreq {
    fn clear(&mut self) {
        self.latin1 = [0; 256];
        self.others.clear();
        self.counts.clear();
    }

    #[inline]
    fn add(&mut self, c: u32) {
        if c < 256 {
            self.latin1[c as usize] += 1;
        } else {
            *self.others.entry(c).or_insert(0) += 1;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CharStats {
    /// Distinct Unicode scalar values.
    pub uniq: u32,
    /// Order-0 Shannon entropy of the scalar distribution as basis points
    /// of [`ENTROPY_NORM`] bits per character: 10000 is 4.47 bits.
    pub entropy: u16,
    /// Character-class score in hundredths, see [`quality_weight`].
    pub quality: u16,
}

/// Twice the `quality` weight of one character: Latin and Greek letters +2,
/// digits, space, `\n` and `\r` +1, common punctuation +0.5, anything else
/// (tabs, symbols, other scripts) -0.5. Doubled so the sums stay integers.
///
/// This is the Cro's `qualityScore` with three changes: every Latin letter
/// counts, including `ſ`, `å` and `ñ`, plus the `ﬁ`-style ligatures; `:`,
/// parentheses, curly quotes and dashes count as punctuation; `_` does not.
#[inline]
fn quality_weight(c: u32) -> i64 {
    match c {
        0x41..=0x5A | 0x61..=0x7A => 4,
        0x30..=0x39 | 0x20 | 0x0A | 0x0D => 2,
        0x2E | 0x2C | 0x3B | 0x3A | 0x21 | 0x3F | 0x27 | 0x22 | 0x2D | 0x28 | 0x29 => 1,
        0x2018 | 0x2019 | 0x201C | 0x201D | 0x2013 | 0x2014 => 1,
        0x80..=0x24F | 0x370..=0x3FF | 0x1E00..=0x1EFF | 0xFB00..=0xFB06 if char::from_u32(c).is_some_and(char::is_alphabetic) => 4,
        _ => -1,
    }
}

/// Mean character weight minus 0.75, in hundredths and clamped to 0..=12500:
/// ordinary prose reads about 100, text made only of letters 125, and
/// anything mostly symbols, tabs or other scripts 0.
fn quality(weighted: i64, n: u64) -> u16 {
    if n == 0 {
        return 0;
    }
    let n = n as i128;
    // (weighted / 2 / n - 0.75) * 10000, rounded half up
    let num = i128::from(weighted) * 5_000 - 7_500 * n;
    if num <= 0 {
        return 0;
    }
    ((num + n / 2) / n).min(12_500) as u16
}

/// Lines with at least one character other than ASCII whitespace; lines end
/// at `\n` or `\r`, so LF, CRLF and bare CR text all count the same.
pub fn count_lines(bytes: &[u8]) -> u32 {
    let (mut lines, mut content) = (0u32, false);
    for &b in bytes {
        match b {
            b'\n' | b'\r' => {
                lines += u32::from(content);
                content = false;
            }
            b' ' | b'\t' | 0x0b | 0x0c => {}
            _ => content = true,
        }
    }
    lines + u32::from(content)
}

/// Bits per character that count as 100% entropy: set so the median document
/// of the clean Vintage-1 store (1.14 M documents) reads about 99.5.
pub const ENTROPY_NORM: f64 = 4.47;

/// Distinct characters and entropy of `text` in one pass.
///
/// The entropy is `log2(n) - sum(c * log2(c)) / n` over the counts `c`,
/// summed in ascending order of `c` so the result does not depend on the
/// order characters were met in (`tests/reference.py` sums the same way).
pub fn char_stats(text: &str, freq: &mut CharFreq) -> CharStats {
    freq.clear();
    if text.is_ascii() {
        for &b in text.as_bytes() {
            freq.latin1[b as usize] += 1;
        }
    } else {
        for c in text.chars() {
            freq.add(c as u32);
        }
    }
    let mut weighted = 0i64;
    for (c, &k) in freq.latin1.iter().enumerate() {
        if k > 0 {
            weighted += quality_weight(c as u32) * i64::from(k);
            freq.counts.push(k);
        }
    }
    for (&c, &k) in &freq.others {
        weighted += quality_weight(c) * i64::from(k);
        freq.counts.push(k);
    }
    freq.counts.sort_unstable();
    let uniq = freq.counts.len() as u32;
    let n: u64 = freq.counts.iter().map(|&c| u64::from(c)).sum();
    let entropy = if n == 0 {
        0.0
    } else {
        let sum: f64 = freq.counts.iter().map(|&c| f64::from(c) * f64::from(c).log2()).sum();
        ((n as f64).log2() - sum / n as f64).max(0.0)
    };
    CharStats { uniq, entropy: (entropy / ENTROPY_NORM * 10_000.0).round().min(65_535.0) as u16, quality: quality(weighted, n) }
}

/// Number of distinct Unicode scalar values in `text`.
pub fn uniq_chars(text: &str, freq: &mut CharFreq) -> u32 {
    char_stats(text, freq).uniq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_counts_follow_scalars_not_bytes() {
        assert_eq!(count_chars(b""), 0);
        assert_eq!(count_chars(b"abc"), 3);
        assert_eq!(count_chars("é".as_bytes()), 1);
        assert_eq!(count_chars("日本語 ok".as_bytes()), 6);
        assert_eq!(count_chars("😀".as_bytes()), 1);
    }

    #[test]
    fn uniq_chars_counts_distinct_scalars() {
        let mut set = CharFreq::default();
        assert_eq!(uniq_chars("", &mut set), 0);
        assert_eq!(uniq_chars("aaaa", &mut set), 1);
        assert_eq!(uniq_chars("abcabc", &mut set), 3);
        assert_eq!(uniq_chars("ééé日日😀", &mut set), 3);
        // The scratch set is cleared between calls.
        assert_eq!(uniq_chars("xyz", &mut set), 3);
        assert_eq!(uniq_chars("x\u{ff}\u{100}", &mut set), 3);
    }

    #[test]
    fn uniq_chars_agrees_with_a_hashset() {
        let mut set = CharFreq::default();
        let samples = ["The quick brown fox", "ſome archaic text, with ligatures ﬁ ﬂ", "\u{0}\u{7f}\u{80}\u{ff}\u{100}"];
        for s in samples {
            let expected = s.chars().collect::<std::collections::HashSet<_>>().len() as u32;
            assert_eq!(uniq_chars(s, &mut set), expected, "{s:?}");
        }
    }

    #[test]
    fn entropy_of_known_distributions() {
        let mut f = CharFreq::default();
        assert_eq!(char_stats("", &mut f).entropy, 0);
        assert_eq!(char_stats("aaaaaaaa", &mut f).entropy, 0, "one symbol: zero bits");
        assert_eq!(char_stats("abababab", &mut f).entropy, 2237, "two equiprobable symbols: one bit is 22.37% of 4.47");
        assert_eq!(char_stats("abcd", &mut f).entropy, 4474, "two bits");
        assert_eq!(char_stats("日本日本", &mut f).entropy, 2237, "non-Latin text takes the same path");
        // 1/2, 1/4, 1/4 -> 1.5 bits
        assert_eq!(char_stats("aabc", &mut f).entropy, 3356);
        let prose = char_stats("The quick brown fox jumps over the lazy dog and keeps running through the night.", &mut f);
        assert!((8_600..10_500).contains(&prose.entropy), "{}", prose.entropy);
        assert_eq!(prose.uniq, uniq_chars("The quick brown fox jumps over the lazy dog and keeps running through the night.", &mut f));
        // order of first occurrence does not matter
        assert_eq!(char_stats("aab", &mut f).entropy, char_stats("baa", &mut f).entropy);
    }

    #[test]
    fn quality_by_character_class() {
        let mut f = CharFreq::default();
        let q = |s: &str, f: &mut CharFreq| char_stats(s, f).quality;
        assert_eq!(q("", &mut f), 0);
        assert_eq!(q("abcd", &mut f), 12_500, "letters only: 2 - 0.75");
        assert_eq!(q("ſåñéſ", &mut f), 12_500, "every Latin letter counts");
        assert_eq!(q("ﬁαβ", &mut f), 12_500, "ligatures and Greek too");
        assert_eq!(q("ab 1", &mut f), 7_500, "(2+2+1+1)/4 - 0.75");
        assert_eq!(q("ab.,", &mut f), 5_000, "(2+2+0.5+0.5)/4 - 0.75");
        assert_eq!(q("ab:(", &mut f), q("ab“—", &mut f), "colon, parentheses, curly quotes, dashes");
        assert_eq!(q("ab_\t", &mut f), 0, "(2+2-0.5-0.5)/4 = 0.75");
        assert_eq!(q("$$$$ ####", &mut f), 0, "clamped at zero");
        assert_eq!(q("日本語の文章です", &mut f), 0, "other scripts");
        assert_eq!(q("a\r\nb", &mut f), q("a  b", &mut f), "CRLF reads like spaces");
        let prose = q("The quick brown fox jumps over the lazy dog, and keeps running through the night.", &mut f);
        assert!((10_000..11_000).contains(&prose), "{prose}");
        // "é" sends the text down the non-ASCII path and still weighs like "e"
        assert_eq!(q("abc café", &mut f), q("abc cafe", &mut f));
    }

    #[test]
    fn lines_skip_blank_ones() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"one"), 1);
        assert_eq!(count_lines(b"one\n"), 1);
        assert_eq!(count_lines(b"one\ntwo"), 2);
        assert_eq!(count_lines(b"one\r\n\r\n \t\n\ntwo\r\n"), 2);
        assert_eq!(count_lines(b"\n\n\n"), 0);
        assert_eq!(count_lines(b"one\rtwo\r\rthree\r"), 3, "bare CR, old Mac files");
        assert_eq!(count_lines(b"one\r\ntwo\r\n"), count_lines(b"one\ntwo\n"), "CRLF counts once");
        assert_eq!(count_lines("é\n日本".as_bytes()), 2);
    }
}
