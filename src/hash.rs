//! Document identity: a 16-byte BLAKE3 prefix of the normalized text.
//!
//! Normalization collapses every run of bytes outside `[A-Za-z0-9_]` into one
//! space and trims the ends, so two OCR runs that differ only in punctuation
//! or whitespace collide on purpose.

use crate::util::{from_hex, to_hex};

pub type Key = [u8; 16];

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Write the normalized form of `text` into `out` (cleared first).
pub fn normalize_into(text: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(text.len());
    let mut pending_space = false;
    for &b in text {
        if is_word_byte(b) {
            if pending_space && !out.is_empty() {
                out.push(b' ');
            }
            pending_space = false;
            out.push(b);
        } else {
            pending_space = true;
        }
    }
}

/// The document key. `scratch` is reused across documents to avoid allocation.
pub fn doc_key(text: &str, scratch: &mut Vec<u8>) -> Key {
    normalize_into(text.as_bytes(), scratch);
    let hash = blake3::hash(scratch);
    let mut key = [0u8; 16];
    key.copy_from_slice(&hash.as_bytes()[..16]);
    key
}

/// Full hash of the raw bytes, used only to break conflict ties.
pub fn raw_hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

pub fn key_to_hex(key: &Key) -> String {
    to_hex(key)
}

pub fn key_from_hex(s: &str) -> Option<Key> {
    let bytes = from_hex(s)?;
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        let mut out = Vec::new();
        normalize_into(s.as_bytes(), &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn normalization_matches_the_ts_rule() {
        assert_eq!(norm("Hello,   world!"), "Hello world");
        assert_eq!(norm("  leading and trailing  "), "leading and trailing");
        assert_eq!(norm("under_score stays"), "under_score stays");
        assert_eq!(norm("café é"), "caf");
        assert_eq!(norm("...!!!"), "");
        assert_eq!(norm(""), "");
        assert_eq!(norm("a\n\nb\tc"), "a b c");
    }

    #[test]
    fn keys_ignore_punctuation_but_not_words() {
        let mut scratch = Vec::new();
        let a = doc_key("The quick, brown fox.", &mut scratch);
        let b = doc_key("The  quick brown fox!!", &mut scratch);
        let c = doc_key("The quick brown dog.", &mut scratch);
        assert_eq!(a, b);
        assert_ne!(a, c);
        let hex = key_to_hex(&a);
        assert_eq!(hex.len(), 32);
        assert_eq!(key_from_hex(&hex), Some(a));
        assert_eq!(key_from_hex("abc"), None);
        assert_eq!(key_from_hex("00"), None);
    }

    #[test]
    fn raw_hash_is_sensitive_to_everything() {
        assert_ne!(raw_hash(b"a b"), raw_hash(b"a  b"));
        assert_eq!(raw_hash(b"x"), raw_hash(b"x"));
    }
}
