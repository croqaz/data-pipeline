//! Per-document features and the prefilter.
//!
//! `compute` runs the scanners in order of cost, dropping the document as
//! soon as a prefilter rule fails so junk never reaches the expensive passes.

pub mod chars;
pub mod compress;
pub mod ngrams;
pub mod sentences;
pub mod words;

use crate::meta::{FLAG_HAS_EXTRA, Meta, SCHEMA_VERSION};

pub const MIN_LEN: u32 = 100;
pub const MIN_UNIQ_CHARS: u32 = 10;
pub const MAX_UNIQ_CHARS: u32 = 132;
pub const MIN_WORDS: u32 = 3;
pub const DEFAULT_MAX_LEN: u32 = 10_000_000;

/// Why a line was not indexed. The order is the order of the reject table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    BadJson,
    NotObject,
    NoText,
    TooShort,
    TooLong,
    UniqChars,
    FewWords,
}

impl DropReason {
    pub const ALL: [DropReason; 7] = [
        DropReason::BadJson,
        DropReason::NotObject,
        DropReason::NoText,
        DropReason::TooShort,
        DropReason::TooLong,
        DropReason::UniqChars,
        DropReason::FewWords,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::BadJson => "bad_json",
            DropReason::NotObject => "not_object",
            DropReason::NoText => "no_text",
            DropReason::TooShort => "too_short",
            DropReason::TooLong => "too_long",
            DropReason::UniqChars => "uniq_chars",
            DropReason::FewWords => "few_words",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    /// Parse-level failures: the line was not a usable JSON record at all.
    pub fn is_parse_error(self) -> bool {
        matches!(self, DropReason::BadJson | DropReason::NotObject | DropReason::NoText)
    }
}

/// Everything a worker reuses between documents.
#[derive(Default)]
pub struct Scratch {
    pub chars: chars::CharFreq,
    pub words: words::WordScratch,
    pub norm: Vec<u8>,
    pub lz4: Vec<u8>,
    pub ngrams: ngrams::NgramScratch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub len: u32,
    pub bytes: u32,
    pub words: u32,
    pub uniq_words: u32,
    pub sentences: u32,
    pub uniq_chars: u32,
    pub alpha_words: u32,
    pub vowel_words: u32,
    /// Characters in all words.
    pub word_chars: u64,
    pub stop_words: u32,
    /// Hundredths of a bit per character.
    pub entropy: u16,
    /// LZ4 size over raw size, basis points of `COMPRESS_NORM`.
    pub compress: u16,
    pub ngrams: ngrams::NgramStats,
}

/// Basis points of `num / den`, rounded; 0 when `den` is 0.
pub fn basis_points(num: u32, den: u32) -> u16 {
    if den == 0 {
        return 0;
    }
    let bp = (u64::from(num) * 10_000 + u64::from(den) / 2) / u64::from(den);
    bp.min(10_000) as u16
}

/// `num / den` in hundredths, rounded and capped at the u16 range; 0 when
/// `den` is 0.
pub fn hundredths(num: u64, den: u32) -> u16 {
    if den == 0 {
        return 0;
    }
    let v = (u128::from(num) * 100 + u128::from(den) / 2) / u128::from(den);
    v.min(u128::from(u16::MAX)) as u16
}

impl Features {
    pub fn to_meta(&self, source: u16, has_extra: bool) -> Meta {
        Meta {
            schema: SCHEMA_VERSION,
            source,
            len: self.len,
            bytes: self.bytes,
            words: self.words,
            uniq_words: self.uniq_words,
            sentences: self.sentences,
            uniq_chars: self.uniq_chars,
            vowel: basis_points(self.vowel_words, self.alpha_words),
            alpha: basis_points(self.alpha_words, self.words),
            flags: if has_extra { FLAG_HAS_EXTRA } else { 0 },
            dict_hit: None,
            entropy: Some(self.entropy),
            compress: Some(self.compress),
            mean_word_len: Some(hundredths(self.word_chars, self.words)),
            stop_words: Some(basis_points(self.stop_words, self.words)),
            top4gram: Some(self.ngrams.top4),
            dup5gram: Some(self.ngrams.dup5),
            dup10gram: Some(self.ngrams.dup10),
        }
    }
}

/// Compute the features of `text`, or the prefilter reason to drop it.
pub fn compute(text: &str, max_len: u32, scratch: &mut Scratch) -> Result<Features, DropReason> {
    let bytes = text.len();
    if bytes <= MIN_LEN as usize {
        return Err(DropReason::TooShort);
    }
    if bytes > max_len as usize * 4 {
        return Err(DropReason::TooLong);
    }
    let len = chars::count_chars(text.as_bytes());
    if len <= MIN_LEN {
        return Err(DropReason::TooShort);
    }
    if len > max_len {
        return Err(DropReason::TooLong);
    }
    let cs = chars::char_stats(text, &mut scratch.chars);
    let uniq_chars = cs.uniq;
    if uniq_chars <= MIN_UNIQ_CHARS || uniq_chars > MAX_UNIQ_CHARS {
        return Err(DropReason::UniqChars);
    }
    let w = words::scan_words(text, &mut scratch.words);
    if w.words < MIN_WORDS {
        return Err(DropReason::FewWords);
    }
    let ngrams = ngrams::ngram_stats(scratch.words.hashes(), scratch.words.lens(), &mut scratch.ngrams);
    let sentences = sentences::count_sentences(text);
    let compress = compress::compress_ratio(text.as_bytes(), &mut scratch.lz4);
    Ok(Features {
        len,
        bytes: bytes as u32,
        words: w.words,
        uniq_words: w.uniq_words,
        sentences,
        uniq_chars,
        alpha_words: w.alpha_words,
        vowel_words: w.vowel_words,
        word_chars: w.word_chars,
        stop_words: w.stop_words,
        entropy: cs.entropy,
        compress,
        ngrams,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROSE: &str = "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! \
How vexingly quick daft zebras jump. The five boxing wizards jump quickly.";

    #[test]
    fn prose_passes_and_counts() {
        let mut sc = Scratch::default();
        let f = compute(PROSE, DEFAULT_MAX_LEN, &mut sc).unwrap();
        assert_eq!(f.bytes as usize, PROSE.len());
        assert_eq!(f.len as usize, PROSE.chars().count());
        assert_eq!(f.words, 29);
        assert_eq!(f.sentences, 4);
        assert!(f.uniq_chars > 20 && f.uniq_chars < 40);
        assert_eq!(f.alpha_words, 29);
        assert_eq!(f.vowel_words, 29);
        let m = f.to_meta(7, true);
        assert_eq!(m.source, 7);
        assert!(m.has_extra());
        assert_eq!(m.vowel, 10_000);
        assert_eq!(m.alpha, 10_000);
        assert_eq!(m.schema, SCHEMA_VERSION);
        assert!(!f.to_meta(0, false).has_extra());
    }

    #[test]
    fn prefilter_reasons_in_order() {
        let mut sc = Scratch::default();
        assert_eq!(compute("short", 1000, &mut sc), Err(DropReason::TooShort));
        // 101 chars but only 100 bytes? no: multi-byte chars make bytes > len.
        let hundred_e = "é".repeat(100);
        assert_eq!(compute(&hundred_e, 1000, &mut sc), Err(DropReason::TooShort));
        let long = "word ".repeat(300);
        assert_eq!(compute(&long, 1000, &mut sc), Err(DropReason::TooLong));
        assert_eq!(compute(&long, 100, &mut sc), Err(DropReason::TooLong));
        let few_uniq = "ab ".repeat(60);
        assert_eq!(compute(&few_uniq, DEFAULT_MAX_LEN, &mut sc), Err(DropReason::UniqChars));
        let many_uniq: String = (0..200u32).map(|i| char::from_u32(0x100 + i).unwrap()).collect::<String>() + " spread out words";
        assert_eq!(compute(&many_uniq, DEFAULT_MAX_LEN, &mut sc), Err(DropReason::UniqChars));
        let two_words = format!("wordone {}", "abcdefghijklmnopqrstuvwxyz0123456789".repeat(4));
        assert_eq!(compute(&two_words, DEFAULT_MAX_LEN, &mut sc), Err(DropReason::FewWords));
        assert!(compute(PROSE, DEFAULT_MAX_LEN, &mut sc).is_ok());
    }

    #[test]
    fn hundredths_round_and_clamp() {
        assert_eq!(hundredths(1, 0), 0);
        assert_eq!(hundredths(35, 9), 389);
        assert_eq!(hundredths(9, 2), 450);
        assert_eq!(hundredths(u64::MAX, 1), u16::MAX);
    }

    #[test]
    fn basis_points_round_and_clamp() {
        assert_eq!(basis_points(1, 0), 0);
        assert_eq!(basis_points(1, 3), 3333);
        assert_eq!(basis_points(2, 3), 6667);
        assert_eq!(basis_points(5, 5), 10_000);
        assert_eq!(basis_points(6, 5), 10_000);
    }

    #[test]
    fn drop_reason_table() {
        for (i, r) in DropReason::ALL.iter().enumerate() {
            assert_eq!(r.index(), i);
            assert!(!r.as_str().is_empty());
        }
        assert!(DropReason::BadJson.is_parse_error());
        assert!(DropReason::NoText.is_parse_error());
        assert!(!DropReason::TooShort.is_parse_error());
        assert_eq!(DropReason::UniqChars.as_str(), "uniq_chars");
    }
}
