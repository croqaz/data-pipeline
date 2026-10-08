//! The per-document feature record and the feature registry.
//!
//! `Meta` is stored as a CBOR map with small integer keys (minicbor),
//! numbered in field order from 0. A feature added later is one more
//! optional field and one registry entry.
//! Percent-like features are stored as basis points (`u16`, 10000 = 100%)
//! and presented as percents with two decimals.

use minicbor::{Decode, Encode};

use crate::error::Result;

pub const SCHEMA_VERSION: u8 = 1;
pub const FLAG_HAS_EXTRA: u8 = 0b0000_0001;

#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode)]
#[cbor(map)]
pub struct Meta {
    #[n(0)]
    pub schema: u8,
    #[n(1)]
    pub source: u16,
    /// Unicode scalar values in the raw text.
    #[n(2)]
    pub len: u32,
    /// UTF-8 bytes of the raw text.
    #[n(3)]
    pub bytes: u32,
    /// Distinct characters.
    #[n(4)]
    pub uniq_chars: u32,
    #[n(5)]
    pub words: u32,
    /// Distinct words after lowercasing and apostrophe folding.
    #[n(6)]
    pub uniq_words: u32,
    /// Lines with any non-whitespace character.
    #[n(7)]
    pub lines: Option<u32>,
    #[n(8)]
    pub sentences: u32,
    /// Basis points: alphabetic words containing a vowel, over alphabetic words.
    #[n(9)]
    pub vowel: u16,
    /// Basis points: purely alphabetic words over all words.
    #[n(10)]
    pub alpha: u16,
    #[n(11)]
    pub flags: u8,
    /// Basis points of the alphabetic words in the vocabulary
    /// (`src/features/vocab.txt`); `None` on records imported before it.
    #[n(12)]
    pub dict_hit: Option<u16>,
    /// Order-0 Shannon entropy of the characters as basis points of 4.47
    /// bits per character: 10000 is 4.47 bits, clean period prose sits near 10000.
    #[n(13)]
    pub entropy: Option<u16>,
    /// Basis points of the characters inside word 5-grams that occur more
    /// than once.
    #[n(14)]
    pub dup5gram: Option<u16>,
    /// Mean characters per word, in hundredths (452 = 4.52).
    #[n(15)]
    pub mean_word_len: Option<u16>,
    /// Basis points of the words that are in the stopword list
    /// (`src/features/stopwords.txt`).
    #[n(16)]
    pub stop_words: Option<u16>,
    /// Character-class score in hundredths: Latin letters weigh 2, digits
    /// and spaces 1, punctuation 0.5, anything else -0.5; the mean minus
    /// 0.75, clamped to 0..=125. Prose reads about 100.
    #[n(17)]
    pub quality: Option<u16>,
    /// Tokens of the text, counted with the store's tokenizer (see
    /// `src/tokenizer.rs`); `None` in a store without one.
    #[n(18)]
    pub tokens: Option<u32>,
}

impl Meta {
    pub fn encode(&self) -> Vec<u8> {
        minicbor::to_vec(self).expect("encoding into a Vec cannot fail")
    }

    pub fn decode(bytes: &[u8]) -> Result<Meta> {
        Ok(minicbor::decode::<Meta>(bytes)?)
    }

    pub fn has_extra(&self) -> bool {
        self.flags & FLAG_HAS_EXTRA != 0
    }

    /// Conflict score, higher wins. Version 1: word count. Replaced once the
    /// real quality signals exist.
    pub fn score(&self) -> f64 {
        f64::from(self.words)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Plain integer.
    Int,
    /// Stored in hundredths (basis points for a percent), shown with two
    /// decimals.
    Pct,
}

pub struct FeatureDef {
    /// Name used in expressions, JSON output and tables (camelCase).
    pub name: &'static str,
    pub kind: Kind,
    /// Value as the expression language sees it (`None` when absent).
    pub get: fn(&Meta) -> Option<f64>,
    /// Stored integer, for histograms.
    pub raw: fn(&Meta) -> Option<u64>,
}

impl FeatureDef {
    /// Decimals to print for this feature's values.
    pub fn decimals(&self) -> usize {
        match self.kind {
            Kind::Int => 0,
            Kind::Pct => 2,
        }
    }

    /// Convert a stored integer to the presented value.
    pub fn present(&self, raw: f64) -> f64 {
        match self.kind {
            Kind::Int => raw,
            Kind::Pct => raw / 100.0,
        }
    }
}

macro_rules! int_feature {
    ($name:literal, $field:ident) => {
        FeatureDef { name: $name, kind: Kind::Int, get: |m| Some(f64::from(m.$field)), raw: |m| Some(u64::from(m.$field)) }
    };
}

macro_rules! opt_int_feature {
    ($name:literal, $field:ident) => {
        FeatureDef { name: $name, kind: Kind::Int, get: |m| m.$field.map(f64::from), raw: |m| m.$field.map(u64::from) }
    };
}

macro_rules! opt_pct_feature {
    ($name:literal, $field:ident) => {
        FeatureDef { name: $name, kind: Kind::Pct, get: |m| m.$field.map(|v| f64::from(v) / 100.0), raw: |m| m.$field.map(u64::from) }
    };
}

macro_rules! pct_feature {
    ($name:literal, $field:ident) => {
        FeatureDef { name: $name, kind: Kind::Pct, get: |m| Some(f64::from(m.$field) / 100.0), raw: |m| Some(u64::from(m.$field)) }
    };
}

/// Every stored feature, in display order. Adding a feature means one entry
/// here plus one field in `Meta`; stats, expressions and export read this.
pub const FEATURES: &[FeatureDef] = &[
    int_feature!("len", len),
    int_feature!("bytes", bytes),
    opt_int_feature!("tokens", tokens),
    int_feature!("uniqChars", uniq_chars),
    int_feature!("words", words),
    int_feature!("uniqWords", uniq_words),
    opt_pct_feature!("meanWordLen", mean_word_len),
    opt_pct_feature!("stopWords", stop_words),
    opt_int_feature!("lines", lines),
    int_feature!("sentences", sentences),
    pct_feature!("vowel", vowel),
    pct_feature!("alpha", alpha),
    opt_pct_feature!("dictHit", dict_hit),
    opt_pct_feature!("dup5gram", dup5gram),
    opt_pct_feature!("entropy", entropy),
    opt_pct_feature!("quality", quality),
];

pub fn feature_index(name: &str) -> Option<usize> {
    FEATURES.iter().position(|f| f.name == name)
}

pub fn feature_names() -> Vec<&'static str> {
    FEATURES.iter().map(|f| f.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Meta {
        Meta {
            schema: SCHEMA_VERSION,
            source: 3,
            len: 1200,
            bytes: 1210,
            words: 200,
            uniq_words: 150,
            lines: None,
            sentences: 12,
            uniq_chars: 40,
            vowel: 9950,
            alpha: 9800,
            flags: FLAG_HAS_EXTRA,
            dict_hit: None,
            entropy: Some(9800),
            dup5gram: None,
            mean_word_len: None,
            stop_words: None,
            quality: Some(10_150),
            tokens: None,
        }
    }

    #[test]
    fn cbor_round_trip_is_compact() {
        let m = sample();
        let bytes = m.encode();
        assert!(bytes.len() < 48, "meta is {} bytes", bytes.len());
        assert_eq!(Meta::decode(&bytes).unwrap(), m);
        assert_eq!(m.clone(), m);
        assert!(m.has_extra());
        assert_eq!(m.score(), 200.0);
    }

    #[test]
    fn optional_fields_survive_both_ways() {
        let mut m = sample();
        m.dict_hit = Some(8800);
        let decoded = Meta::decode(&m.encode()).unwrap();
        assert_eq!(decoded.dict_hit, Some(8800));
        let dict = feature_index("dictHit").unwrap();
        assert_eq!((FEATURES[dict].get)(&decoded), Some(88.0));
        assert_eq!((FEATURES[dict].raw)(&decoded), Some(8800));
        let old = Meta { entropy: None, quality: None, ..sample() };
        let decoded = Meta::decode(&old.encode()).unwrap();
        assert_eq!((FEATURES[feature_index("entropy").unwrap()].get)(&decoded), None);
        assert_eq!((FEATURES[feature_index("quality").unwrap()].get)(&sample()), Some(101.5));
        let lines = &FEATURES[feature_index("lines").unwrap()];
        assert_eq!((lines.get)(&sample()), None);
        let m = Meta { lines: Some(70_000), ..sample() };
        assert_eq!(((lines.get)(&m), (lines.raw)(&m), lines.decimals()), (Some(70_000.0), Some(70_000), 0));
        assert_eq!(Meta::decode(&m.encode()).unwrap(), m);
        assert_eq!(FEATURES[feature_index("entropy").unwrap()].decimals(), 2);
    }

    #[test]
    fn unknown_future_fields_are_skipped() {
        // A record written by a newer binary: every required field plus field 99.
        let mut enc = minicbor::Encoder::new(Vec::new());
        enc.map(12).unwrap();
        for (k, v) in [(0u8, 1u32), (1, 0), (2, 101), (3, 101), (4, 20), (5, 50), (6, 40), (8, 2), (9, 10_000), (10, 10_000), (11, 0)] {
            enc.u8(k).unwrap().u32(v).unwrap();
        }
        enc.u8(99).unwrap().u32(7).unwrap();
        let bytes = enc.into_writer();
        let m = Meta::decode(&bytes).unwrap();
        assert_eq!(m.words, 50);
        assert_eq!(m.len, 101);
        assert_eq!((m.sentences, m.alpha, m.lines), (2, 10_000, None));
        assert_eq!(m.dict_hit, None);
        assert!(!m.has_extra());
        // A required field missing altogether is corruption.
        let mut enc = minicbor::Encoder::new(Vec::new());
        enc.map(1).unwrap();
        enc.u8(4).unwrap().u32(50).unwrap();
        assert!(Meta::decode(&enc.into_writer()).is_err());
    }

    #[test]
    fn corrupt_bytes_are_an_error() {
        assert!(Meta::decode(&[0xff, 0x00]).is_err());
        assert!(Meta::decode(&[]).is_err());
    }

    #[test]
    fn registry_lookup_and_presentation() {
        assert_eq!(feature_index("words"), Some(4));
        assert_eq!(feature_index("tokens"), Some(2));
        assert_eq!(feature_index("nope"), None);
        assert!(feature_names().contains(&"uniqWords"));
        let m = sample();
        let vowel = &FEATURES[feature_index("vowel").unwrap()];
        assert_eq!((vowel.get)(&m), Some(99.5));
        assert_eq!(vowel.decimals(), 2);
        assert_eq!(vowel.present(9950.0), 99.5);
        let words = &FEATURES[feature_index("words").unwrap()];
        assert_eq!(words.decimals(), 0);
        assert_eq!(words.present(200.0), 200.0);
        assert_eq!((words.raw)(&m), Some(200));
        assert_eq!((FEATURES[feature_index("dictHit").unwrap()].get)(&m), None);
    }
}
