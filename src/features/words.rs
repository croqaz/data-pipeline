//! Word scanner: counts, distinct words, alphabetic and vowel shares.
//!
//! A word is a maximal run of non-whitespace with leading and trailing
//! non-alphanumeric characters stripped (so `"fox."` is `fox`, `--` is
//! nothing). `uniqWords` lowercases ASCII letters and folds the curly
//! apostrophe before hashing. An alphabetic word matches
//! `[A-Za-z][A-Za-z'’-]*`; a vowel word is an alphabetic word containing one
//! of `aeiouy` in either case.

use std::hash::BuildHasher;

use hashbrown::HashSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WordStats {
    pub words: u32,
    pub uniq_words: u32,
    pub alpha_words: u32,
    pub vowel_words: u32,
}

/// Scratch reused across documents: the distinct-word set and a lowercase
/// buffer. Word hashes are 64-bit and already uniform, so the set uses an
/// identity hasher.
pub struct WordScratch {
    set: HashSet<u64, IdentityBuild>,
    lower: Vec<u8>,
    hasher: foldhash::fast::FixedState,
}

impl Default for WordScratch {
    fn default() -> Self {
        Self { set: HashSet::with_hasher(IdentityBuild), lower: Vec::with_capacity(64), hasher: foldhash::fast::FixedState::default() }
    }
}

#[derive(Clone, Copy, Default)]
pub struct IdentityBuild;

#[derive(Default)]
pub struct IdentityHasher(u64);

impl std::hash::Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}

impl BuildHasher for IdentityBuild {
    type Hasher = IdentityHasher;
    fn build_hasher(&self) -> IdentityHasher {
        IdentityHasher::default()
    }
}

/// Whitespace as the TS tokenizer saw it: Unicode `White_Space` plus the BOM.
#[inline]
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

#[inline]
fn is_vowel(b: u8) -> bool {
    matches!(b | 0x20, b'a' | b'e' | b'i' | b'o' | b'u' | b'y')
}

/// `[A-Za-z][A-Za-z'’-]*` on the UTF-8 bytes (`’` is E2 80 99).
fn is_alpha_word(tok: &[u8]) -> bool {
    let Some(&first) = tok.first() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let mut i = 1;
    while i < tok.len() {
        let b = tok[i];
        if b.is_ascii_alphabetic() || b == b'\'' || b == b'-' {
            i += 1;
        } else if b == 0xE2 && tok.len() >= i + 3 && tok[i + 1] == 0x80 && tok[i + 2] == 0x99 {
            i += 3;
        } else {
            return false;
        }
    }
    true
}

fn has_vowel(tok: &[u8]) -> bool {
    tok.iter().any(|&b| b.is_ascii_alphabetic() && is_vowel(b))
}

fn fold_lower(tok: &str, out: &mut Vec<u8>) {
    out.clear();
    for c in tok.chars() {
        match c {
            'A'..='Z' => out.push(c as u8 | 0x20),
            '\u{2019}' => out.push(b'\''),
            c if c.is_ascii() => out.push(c as u8),
            c => {
                let mut tmp = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
    }
}

pub fn scan_words(text: &str, scratch: &mut WordScratch) -> WordStats {
    let mut st = WordStats::default();
    scratch.set.clear();
    for raw in text.split(is_space) {
        if raw.is_empty() {
            continue;
        }
        let tok = raw.trim_matches(|c: char| !c.is_alphanumeric());
        if tok.is_empty() {
            continue;
        }
        st.words += 1;
        fold_lower(tok, &mut scratch.lower);
        let h = scratch.hasher.hash_one(&scratch.lower[..]);
        if scratch.set.insert(h) {
            st.uniq_words += 1;
        }
        let bytes = tok.as_bytes();
        if is_alpha_word(bytes) {
            st.alpha_words += 1;
            if has_vowel(bytes) {
                st.vowel_words += 1;
            }
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> WordStats {
        let mut sc = WordScratch::default();
        scan_words(s, &mut sc)
    }

    #[test]
    fn counts_words_like_the_ts_tokenizer() {
        let st = scan("The quick, brown fox -- jumps (over) the lazy dog.");
        assert_eq!(st.words, 9);
        assert_eq!(st.alpha_words, 9);
        assert_eq!(st.vowel_words, 9);
        // "The" and "the" are one distinct word.
        assert_eq!(st.uniq_words, 8);
    }

    #[test]
    fn empty_and_punctuation_only() {
        assert_eq!(scan(""), WordStats::default());
        assert_eq!(scan("   \t\n"), WordStats::default());
        assert_eq!(scan("-- ... !!! (())"), WordStats::default());
    }

    #[test]
    fn alpha_and_vowel_rules() {
        let st = scan("don't o'brien smooth’d well-known 1842 x9 thn I");
        assert_eq!(st.words, 8);
        // alphabetic: don't o'brien smooth’d well-known thn I
        assert_eq!(st.alpha_words, 6);
        // vowel words exclude "thn" (no vowel)
        assert_eq!(st.vowel_words, 5);
    }

    #[test]
    fn curly_apostrophe_folds_for_uniqueness() {
        let st = scan("don’t don't DON'T");
        assert_eq!(st.words, 3);
        assert_eq!(st.uniq_words, 1);
    }

    #[test]
    fn unicode_words_and_spaces() {
        let st = scan("café\u{00a0}café\u{3000}日本語\u{FEFF}x");
        assert_eq!(st.words, 4);
        assert_eq!(st.uniq_words, 3);
        assert_eq!(st.alpha_words, 1);
        assert_eq!(st.vowel_words, 0);
        assert!(!is_alpha_word("é".as_bytes()));
        assert!(!is_alpha_word(b"a\xe2\x80"));
        assert!(!is_alpha_word(b""));
    }

    #[test]
    fn identity_hasher_passes_values_through() {
        use std::hash::Hasher;
        let mut h = IdentityHasher::default();
        h.write_u64(42);
        assert_eq!(h.finish(), 42);
        let mut h2 = IdentityHasher::default();
        h2.write(&[1, 2]);
        assert_ne!(h2.finish(), 0);
    }

    #[test]
    fn degenerate_text_has_few_unique_words() {
        let text = "And he said. ".repeat(200);
        let st = scan(&text);
        assert_eq!(st.words, 600);
        assert_eq!(st.uniq_words, 3);
    }
}
