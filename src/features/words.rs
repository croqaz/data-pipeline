//! Word scanner: counts, distinct words, alphabetic and vowel shares.
//!
//! A word is a maximal run of non-whitespace with leading and trailing
//! non-alphanumeric characters stripped (so `"fox."` is `fox`, `--` is
//! nothing). `uniqWords` lowercases ASCII letters and folds the curly
//! apostrophe before hashing. An alphabetic word matches
//! `[A-Za-z][A-Za-z'’-]*`; a vowel word is an alphabetic word containing one
//! of `aeiouy` in either case. A dictionary word is an alphabetic word whose
//! folded form is in `vocab.txt`, or ends in `'s` and is there without it.
//!
//! Both word lists are compiled into the binary with `include_str!`, so the
//! executable needs no files at run time.

use std::hash::BuildHasher;

use hashbrown::HashSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WordStats {
    pub words: u32,
    pub uniq_words: u32,
    pub alpha_words: u32,
    pub vowel_words: u32,
    /// Characters in all words (after trimming), for `meanWordLen`.
    pub word_chars: u64,
    /// Words in [`STOPWORDS`].
    pub stop_words: u32,
    /// Alphabetic words in [`VOCAB`].
    pub dict_words: u32,
}

/// The stopword list: whitespace-separated words, `#` comments.
pub const STOPWORDS: &str = include_str!("stopwords.txt");

/// The `dictHit` vocabulary: one word per line, most frequent first, `#`
/// comments (research/REPORT-vocab-2026-10-06.md).
pub const VOCAB: &str = include_str!("vocab.txt");

/// The stopwords as word hashes, made with the same hasher `scan_words`
/// uses for every folded word, so membership is one set lookup.
static STOP_SET: std::sync::LazyLock<HashSet<u64, IdentityBuild>> = std::sync::LazyLock::new(|| hash_words(stopword_list()));

/// The vocabulary as word hashes, like [`STOP_SET`].
static VOCAB_SET: std::sync::LazyLock<HashSet<u64, IdentityBuild>> = std::sync::LazyLock::new(|| hash_words(vocab_list()));

fn hash_words(words: impl Iterator<Item = &'static str>) -> HashSet<u64, IdentityBuild> {
    let hasher = foldhash::fast::FixedState::default();
    words.map(|w| hasher.hash_one(w.as_bytes())).collect()
}

/// Whitespace-separated words of a list, `#` comment lines removed.
fn word_list(list: &'static str) -> impl Iterator<Item = &'static str> {
    list.lines().filter(|l| !l.trim_start().starts_with('#')).flat_map(str::split_whitespace)
}

/// The words of [`STOPWORDS`], comments removed.
pub fn stopword_list() -> impl Iterator<Item = &'static str> {
    word_list(STOPWORDS)
}

/// The words of [`VOCAB`], comments removed.
pub fn vocab_list() -> impl Iterator<Item = &'static str> {
    word_list(VOCAB)
}

/// Is the folded word `lower` (hash `h`) in the vocabulary, as it is or
/// without a trailing `'s`?
fn in_vocab(h: u64, lower: &[u8], hasher: &foldhash::fast::FixedState) -> bool {
    VOCAB_SET.contains(&h) || lower.strip_suffix(b"'s").is_some_and(|stem| VOCAB_SET.contains(&hasher.hash_one(stem)))
}

/// Scratch reused across documents: the distinct-word set, a lowercase
/// buffer, and the hash and length of every word in order (for the n-gram
/// scanner). Word hashes are 64-bit and already uniform, so the set uses an
/// identity hasher.
pub struct WordScratch {
    set: HashSet<u64, IdentityBuild>,
    lower: Vec<u8>,
    hasher: foldhash::fast::FixedState,
    hashes: Vec<u64>,
    lens: Vec<u32>,
}

impl WordScratch {
    /// Hash of each folded word of the last scanned text, in order.
    pub fn hashes(&self) -> &[u64] {
        &self.hashes
    }

    /// Length in characters of each word of the last scanned text.
    pub fn lens(&self) -> &[u32] {
        &self.lens
    }
}

impl Default for WordScratch {
    fn default() -> Self {
        Self {
            set: HashSet::with_hasher(IdentityBuild),
            lower: Vec::with_capacity(64),
            hasher: foldhash::fast::FixedState::default(),
            hashes: Vec::new(),
            lens: Vec::new(),
        }
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

const OTHER: u8 = 0;
const SPACE: u8 = 1;
const ALNUM: u8 = 2;

/// Class of each ASCII byte. `SPACE` is Unicode `White_Space`: tab, LF, VT,
/// FF, CR, space (0x1C to 0x1F are not, unlike Python's `isspace`).
const ASCII_CLASS: [u8; 128] = {
    let mut t = [OTHER; 128];
    let mut b = 0;
    while b < 128 {
        if (b >= 0x09 && b <= 0x0D) || b == 0x20 {
            t[b] = SPACE;
        } else if (b as u8).is_ascii_alphanumeric() {
            t[b] = ALNUM;
        }
        b += 1;
    }
    t
};

/// The character starting at byte `i`, which must be a char boundary.
#[inline]
fn char_at(text: &str, i: usize) -> char {
    text[i..].chars().next().expect("i is inside the text")
}

/// Is the character starting at byte `i` whitespace? Returns the answer and
/// its length in bytes. ASCII is a table lookup; only other bytes decode.
#[inline]
fn space_at(text: &str, i: usize) -> (bool, usize) {
    let b = text.as_bytes()[i];
    if b < 0x80 {
        (ASCII_CLASS[b as usize] == SPACE, 1)
    } else {
        let c = char_at(text, i);
        (is_space(c), c.len_utf8())
    }
}

/// `tok` without leading and trailing non-alphanumeric characters.
#[inline]
fn trim_token(tok: &str) -> &str {
    let b = tok.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] < 0x80 {
            if b[i].is_ascii_alphanumeric() {
                break;
            }
            i += 1;
        } else {
            let c = char_at(tok, i);
            if c.is_alphanumeric() {
                break;
            }
            i += c.len_utf8();
        }
    }
    let mut j = b.len();
    while j > i {
        if b[j - 1] < 0x80 {
            if b[j - 1].is_ascii_alphanumeric() {
                break;
            }
            j -= 1;
        } else {
            let c = tok[..j].chars().next_back().expect("j > i");
            if c.is_alphanumeric() {
                break;
            }
            j -= c.len_utf8();
        }
    }
    &tok[i..j]
}

/// One pass over the bytes. Tokens are the maximal runs between whitespace
/// characters, as `text.split(is_space)` gives them; the work per byte is a
/// table lookup unless the byte starts a non-ASCII character. The same pass
/// notes the first and last ASCII letter or digit, so a token that is all
/// ASCII (the usual case) is trimmed without a second look, folded with
/// `to_ascii_lowercase`, and its length in characters is its length in
/// bytes.
pub fn scan_words(text: &str, scratch: &mut WordScratch) -> WordStats {
    let mut st = WordStats::default();
    scratch.set.clear();
    if scratch.hashes.capacity() > super::ngrams::BIG_DOC {
        scratch.hashes = Vec::new();
        scratch.lens = Vec::new();
    }
    scratch.hashes.clear();
    scratch.lens.clear();
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        // skip whitespace
        let (space, w) = space_at(text, i);
        if space {
            i += w;
            continue;
        }
        // the token runs to the next whitespace character
        let start = i;
        let mut ascii = true;
        let (mut first, mut last) = (usize::MAX, 0);
        while i < n {
            let b = bytes[i];
            if b < 0x80 {
                match ASCII_CLASS[b as usize] {
                    SPACE => break,
                    ALNUM => {
                        first = first.min(i);
                        last = i;
                    }
                    _ => {}
                }
                i += 1;
            } else {
                let c = char_at(text, i);
                if is_space(c) {
                    break;
                }
                ascii = false;
                i += c.len_utf8();
            }
        }
        let tok = match (ascii, first) {
            (true, usize::MAX) => "",
            (true, _) => &text[first..=last],
            (false, _) => trim_token(&text[start..i]),
        };
        if tok.is_empty() {
            continue;
        }
        st.words += 1;
        let len = if ascii {
            scratch.lower.clear();
            scratch.lower.extend(tok.bytes().map(|b| b.to_ascii_lowercase()));
            tok.len() as u32
        } else {
            fold_lower(tok, &mut scratch.lower);
            scratch.lower.iter().filter(|&&b| (b as i8) >= -0x40).count() as u32
        };
        let h = scratch.hasher.hash_one(&scratch.lower[..]);
        scratch.hashes.push(h);
        scratch.lens.push(len);
        st.word_chars += u64::from(len);
        if STOP_SET.contains(&h) {
            st.stop_words += 1;
        }
        if scratch.set.insert(h) {
            st.uniq_words += 1;
        }
        let tb = tok.as_bytes();
        if is_alpha_word(tb) {
            st.alpha_words += 1;
            if has_vowel(tb) {
                st.vowel_words += 1;
            }
            if in_vocab(h, &scratch.lower, &scratch.hasher) {
                st.dict_words += 1;
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

    /// The previous implementation, kept as the reference the fast path
    /// must match exactly.
    fn scan_reference(text: &str) -> (WordStats, Vec<u64>, Vec<u32>) {
        let mut sc = WordScratch::default();
        let mut st = WordStats::default();
        for raw in text.split(is_space) {
            if raw.is_empty() {
                continue;
            }
            let tok = raw.trim_matches(|c: char| !c.is_alphanumeric());
            if tok.is_empty() {
                continue;
            }
            st.words += 1;
            fold_lower(tok, &mut sc.lower);
            let h = sc.hasher.hash_one(&sc.lower[..]);
            sc.hashes.push(h);
            let len = sc.lower.iter().filter(|&&b| (b as i8) >= -0x40).count() as u32;
            sc.lens.push(len);
            st.word_chars += u64::from(len);
            if STOP_SET.contains(&h) {
                st.stop_words += 1;
            }
            if sc.set.insert(h) {
                st.uniq_words += 1;
            }
            let bytes = tok.as_bytes();
            if is_alpha_word(bytes) {
                st.alpha_words += 1;
                if has_vowel(bytes) {
                    st.vowel_words += 1;
                }
                if in_vocab(h, &sc.lower, &sc.hasher) {
                    st.dict_words += 1;
                }
            }
        }
        (st, sc.hashes, sc.lens)
    }

    fn scan_full(text: &str) -> (WordStats, Vec<u64>, Vec<u32>) {
        let mut sc = WordScratch::default();
        let st = scan_words(text, &mut sc);
        (st, sc.hashes.clone(), sc.lens.clone())
    }

    /// Pieces chosen to hit every branch: ASCII and Unicode whitespace, the
    /// C0 separators that are not whitespace, punctuation to trim on either
    /// side, curly quotes, accents, CJK, emoji, non-ASCII punctuation.
    const PIECES: &[&str] = &[
        "a", "The", "WORD", "don't", "don’t", "x9", "42", " ", "  ", "\t", "\n", "\r\n", "\u{0b}", "\u{0c}", "\u{1c}", "\u{1f}", "\u{85}",
        "\u{a0}", "\u{1680}", "\u{2003}", "\u{2028}", "\u{202f}", "\u{3000}", "\u{feff}", "\u{200b}", ".", ",", "--", "(", ")", "\"", "“",
        "”", "‘", "’", "…", "—", "«", "»", "¿", "é", "café", "naïve", "ſ", "ﬁ", "日本", "語", "😀", "Ⅻ", "²", "_", "#", "'",
    ];

    proptest::proptest! {
        #[test]
        fn fast_path_matches_the_reference(idx in proptest::collection::vec(0..PIECES.len(), 0..40)) {
            let text: String = idx.iter().map(|&i| PIECES[i]).collect();
            proptest::prop_assert_eq!(scan_full(&text), scan_reference(&text));
        }

        #[test]
        fn fast_path_matches_on_any_string(text in "\\PC{0,60}") {
            proptest::prop_assert_eq!(scan_full(&text), scan_reference(&text));
        }
    }

    #[test]
    fn fast_path_matches_on_samples() {
        for s in ["", " ", "a", "...", " .a. ", "\u{a0}x\u{a0}", "é", "’tis", "o’", "x\u{1c}y", "日本 語", "a\u{3000}b", "--é--"] {
            assert_eq!(scan_full(s), scan_reference(s), "{s:?}");
        }
    }

    #[test]
    fn word_lengths_and_stopwords() {
        let st = scan("The quick, brown fox -- jumps (over) the lazy dog.");
        // the quick brown fox jumps over the lazy dog: 3+5+5+3+5+4+3+4+3
        assert_eq!((st.words, st.word_chars), (9, 35));
        // the, over, the
        assert_eq!(st.stop_words, 3);
        let early = scan("Thou hast not ſeene what I haue, vnto THEE ’tis giuen");
        // thou hast not what i haue vnto thee tis (giuen and ſeene are not)
        assert_eq!(early.stop_words, 9, "{early:?}");
        assert_eq!(scan("café naïve").word_chars, 9, "characters, not bytes");
        let list: Vec<&str> = stopword_list().collect();
        assert!(list.contains(&"the") && list.contains(&"hath") && list.contains(&"vpon"));
        assert!(!list.iter().any(|w| w.starts_with('#') || w.chars().any(|c| c.is_ascii_uppercase())));
        let unique: std::collections::HashSet<&&str> = list.iter().collect();
        assert_eq!(unique.len(), list.len(), "no duplicates");
        assert_eq!(STOP_SET.len(), list.len());
    }

    #[test]
    fn dictionary_words_match_any_case() {
        // christ god and the are in the vocabulary in lowercase
        let st = scan("Christ CHRIST christ God GOD god The tbe 1842 Xqzvw");
        assert_eq!((st.alpha_words, st.dict_words), (9, 7), "{st:?}");
        // a curly apostrophe folds before the lookup; a possessive falls back to its stem
        let st = scan("don’t Wilberforce's Xqzvw's isn't");
        assert_eq!((st.alpha_words, st.dict_words), (4, 3), "{st:?}");
        // only alphabetic words are looked up: numbers and hyphen-free junk miss or don't count
        assert_eq!(scan("1842 £20 x9").dict_words, 0);
    }

    #[test]
    fn vocabulary_is_folded_unique_and_ranked() {
        let list: Vec<&str> = vocab_list().collect();
        assert_eq!(list.len(), 98_304);
        assert_eq!(&list[..3], ["the", "of", "and"]);
        assert!(list.contains(&"christ") && list.contains(&"god") && list.contains(&"to-day") && list.contains(&"haue"));
        let alpha = |w: &&str| {
            let b = w.as_bytes();
            b[0].is_ascii_lowercase() && b.iter().all(|&c| c.is_ascii_lowercase() || c == b'\'' || c == b'-')
        };
        assert!(list.iter().all(alpha), "every entry is folded and matches [a-z][a-z'-]*");
        assert!(!list.iter().any(|w| w.len() == 1 && !"aeiou".contains(*w)), "no single consonants");
        let unique: std::collections::HashSet<&&str> = list.iter().collect();
        assert_eq!(unique.len(), list.len(), "no duplicates");
        assert_eq!(VOCAB_SET.len(), list.len());
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
