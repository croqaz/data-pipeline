//! Character-level counts: scalar count, distinct scalars and the Shannon
//! entropy of the character distribution.

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
    /// Order-0 Shannon entropy of the scalar distribution, in hundredths of
    /// a bit per character (English prose sits around 4.1 to 4.6 bits).
    pub entropy: u16,
}

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
    freq.counts.extend(freq.latin1.iter().copied().filter(|&c| c > 0));
    freq.counts.extend(freq.others.values().copied());
    freq.counts.sort_unstable();
    let uniq = freq.counts.len() as u32;
    let n: u64 = freq.counts.iter().map(|&c| u64::from(c)).sum();
    let entropy = if n == 0 {
        0.0
    } else {
        let sum: f64 = freq.counts.iter().map(|&c| f64::from(c) * f64::from(c).log2()).sum();
        ((n as f64).log2() - sum / n as f64).max(0.0)
    };
    CharStats { uniq, entropy: (entropy * 100.0).round().min(65_535.0) as u16 }
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
        assert_eq!(char_stats("abababab", &mut f).entropy, 100, "two equiprobable symbols: one bit");
        assert_eq!(char_stats("abcd", &mut f).entropy, 200);
        assert_eq!(char_stats("日本日本", &mut f).entropy, 100, "non-Latin text takes the same path");
        // 1/2, 1/4, 1/4 -> 1.5 bits
        assert_eq!(char_stats("aabc", &mut f).entropy, 150);
        let prose = char_stats("The quick brown fox jumps over the lazy dog and keeps running through the night.", &mut f);
        assert!((380..460).contains(&prose.entropy), "{}", prose.entropy);
        assert_eq!(prose.uniq, uniq_chars("The quick brown fox jumps over the lazy dog and keeps running through the night.", &mut f));
        // order of first occurrence does not matter
        assert_eq!(char_stats("aab", &mut f).entropy, char_stats("baa", &mut f).entropy);
    }
}
