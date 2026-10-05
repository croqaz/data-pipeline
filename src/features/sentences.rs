//! Sentence boundaries, counted on the UTF-8 bytes.
//!
//! A boundary is either
//! - `.`, `!` or `?`, an optional closing quote, at least one ASCII
//!   whitespace character, then an ASCII uppercase letter or an opening quote;
//! - or a blank line (a newline, optional spaces, another newline), optional
//!   whitespace, then an ASCII letter or a quote.
//!
//! The count is boundaries plus one, so any non-empty text has at least one
//! sentence. This is the TS rule rewritten as a scanner; the regex crate
//! cannot express its lookahead.

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Length in bytes of a quote character at `i`, or 0.
#[inline]
fn quote_len(b: &[u8], i: usize) -> usize {
    match b.get(i) {
        Some(b'"') | Some(b'\'') => 1,
        Some(0xE2) if b.len() >= i + 3 && b[i + 1] == 0x80 && matches!(b[i + 2], 0x98 | 0x99 | 0x9C | 0x9D) => 3,
        _ => 0,
    }
}

#[inline]
fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    i
}

pub fn count_sentences(text: &str) -> u32 {
    let b = text.as_bytes();
    let n = b.len();
    let mut count = 0u32;
    let mut i = 0;
    while i < n {
        match b[i] {
            b'.' | b'!' | b'?' => {
                let j = i + 1 + quote_len(b, i + 1);
                let k = skip_ws(b, j);
                if k > j && k < n && (b[k].is_ascii_uppercase() || quote_len(b, k) > 0) {
                    count += 1;
                    i = k;
                } else {
                    i = j;
                }
            }
            b'\n' => {
                // A blank line: newline, horizontal space, newline.
                let mut j = i + 1;
                while j < n && matches!(b[j], b' ' | b'\t' | b'\r') {
                    j += 1;
                }
                if j < n && b[j] == b'\n' {
                    let k = skip_ws(b, j);
                    if k < n && (b[k].is_ascii_alphabetic() || quote_len(b, k) > 0) {
                        count += 1;
                    }
                    i = k.max(j + 1);
                } else {
                    i = j;
                }
            }
            _ => i += 1,
        }
    }
    count + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prose() {
        assert_eq!(count_sentences(""), 1);
        assert_eq!(count_sentences("One sentence"), 1);
        assert_eq!(count_sentences("One. Two. Three."), 3);
        assert_eq!(count_sentences("Really? Yes! Fine."), 3);
        assert_eq!(count_sentences("Mr. smith went. He left."), 2);
        assert_eq!(count_sentences("Wait... What"), 2);
        assert_eq!(count_sentences("Ends with period."), 1);
        assert_eq!(count_sentences("No space.Next"), 1);
    }

    #[test]
    fn quotes_after_punctuation_and_before_sentences() {
        assert_eq!(count_sentences("\"Go.\" He went."), 2);
        assert_eq!(count_sentences("He said no. \"Really.\""), 2);
        assert_eq!(count_sentences("He said no. “Really.”"), 2);
        assert_eq!(count_sentences("Said ‘no.’ ’Tis true."), 2);
        assert_eq!(count_sentences("Odd. \u{e2}\u{80}\u{a0}x"), 1);
    }

    #[test]
    fn blank_lines_split_paragraphs() {
        assert_eq!(count_sentences("first paragraph\n\nsecond paragraph"), 2);
        assert_eq!(count_sentences("first\n   \n\n  second"), 2);
        assert_eq!(count_sentences("first\r\n\r\nsecond"), 2);
        assert_eq!(count_sentences("single\nnewline"), 1);
        assert_eq!(count_sentences("line\n\n123 numbers"), 1);
        assert_eq!(count_sentences("line\n\n\"quoted"), 2);
        assert_eq!(count_sentences("trailing\n\n"), 1);
        assert_eq!(count_sentences("trailing\n \n"), 1);
    }

    #[test]
    fn tail_cases() {
        assert_eq!(count_sentences("end."), 1);
        assert_eq!(count_sentences("end.\""), 1);
        assert_eq!(count_sentences("end. "), 1);
        assert_eq!(count_sentences("a. b. C."), 2);
    }
}
