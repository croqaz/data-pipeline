//! Character-level counts: scalar count and distinct scalars.

use hashbrown::HashSet;

/// Unicode scalar values in a UTF-8 string: every byte that is not a
/// continuation byte starts a character.
pub fn count_chars(bytes: &[u8]) -> u32 {
    bytes.iter().filter(|&&b| (b & 0xC0) != 0x80).count() as u32
}

/// Scratch space for distinct-character counting, reused across documents.
#[derive(Default)]
pub struct CharSet {
    latin1: [u64; 4],
    others: HashSet<u32>,
}

impl CharSet {
    fn clear(&mut self) {
        self.latin1 = [0; 4];
        self.others.clear();
    }

    #[inline]
    fn insert(&mut self, c: u32) -> bool {
        if c < 256 {
            let word = (c >> 6) as usize;
            let bit = 1u64 << (c & 63);
            let seen = self.latin1[word] & bit != 0;
            self.latin1[word] |= bit;
            !seen
        } else {
            self.others.insert(c)
        }
    }
}

/// Number of distinct Unicode scalar values in `text`.
pub fn uniq_chars(text: &str, set: &mut CharSet) -> u32 {
    set.clear();
    let mut n = 0u32;
    if text.is_ascii() {
        for &b in text.as_bytes() {
            if set.insert(u32::from(b)) {
                n += 1;
            }
        }
    } else {
        for c in text.chars() {
            if set.insert(c as u32) {
                n += 1;
            }
        }
    }
    n
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
        let mut set = CharSet::default();
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
        let mut set = CharSet::default();
        let samples = ["The quick brown fox", "ſome archaic text, with ligatures ﬁ ﬂ", "\u{0}\u{7f}\u{80}\u{ff}\u{100}"];
        for s in samples {
            let expected = s.chars().collect::<std::collections::HashSet<_>>().len() as u32;
            assert_eq!(uniq_chars(s, &mut set), expected, "{s:?}");
        }
    }
}
