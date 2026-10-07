//! Word n-gram repetition: `dup5gram`.
//!
//! The words are the tokens of `words`, folded like `uniqWords`, each
//! weighing its character count. `dup5gram` is the share of characters
//! inside any 5-word window that occurs at least twice, each word counted
//! once, in basis points.

use hashbrown::HashMap;

use super::basis_points;
use super::words::IdentityBuild;

/// Scratch reused across documents: window hash to slot, per-window slots
/// and counts, the counts per slot, and the prefix sums of word lengths.
#[derive(Default)]
pub struct NgramScratch {
    map: HashMap<u64, u32, IdentityBuild>,
    hashes: Vec<u64>,
    slots: Vec<u32>,
    per_slot: Vec<u32>,
    counts: Vec<u32>,
    prefix: Vec<u64>,
}

const B: u64 = 0x9E37_79B9_7F4A_7C15;

/// Finalizer so the identity-hashed map gets well-mixed low bits.
#[inline]
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x
}

/// Count every window of `n` words; leaves `sc.counts[i]` set to the number
/// of occurrences of window `i`. Window hashes roll in O(1): a polynomial in
/// `B` over the word hashes, modulo 2^64.
fn count_windows(words: &[u64], n: usize, sc: &mut NgramScratch) {
    let windows = words.len() + 1 - n;
    sc.hashes.clear();
    let top = B.wrapping_pow(n as u32 - 1);
    let mut h = words[..n].iter().fold(0u64, |a, &w| a.wrapping_mul(B).wrapping_add(w));
    for i in 0..windows {
        if i > 0 {
            h = h.wrapping_sub(words[i - 1].wrapping_mul(top)).wrapping_mul(B).wrapping_add(words[i + n - 1]);
        }
        sc.hashes.push(mix(h));
    }
    // A table sized for an earlier huge document would make every clear()
    // cost that size again: rebuild it when it is far too big.
    if sc.map.capacity() > 4 * windows.max(4096) {
        sc.map = HashMap::with_hasher(IdentityBuild);
    } else {
        sc.map.clear();
    }
    // One map operation per window: each distinct window gets a slot, and
    // the per-slot counts are read back by index.
    sc.slots.clear();
    sc.per_slot.clear();
    for &h in &sc.hashes {
        let next = sc.per_slot.len() as u32;
        let slot = *sc.map.entry(h).or_insert(next);
        if slot == next {
            sc.per_slot.push(0);
        }
        sc.per_slot[slot as usize] += 1;
        sc.slots.push(slot);
    }
    sc.counts.clear();
    sc.counts.extend(sc.slots.iter().map(|&s| sc.per_slot[s as usize]));
}

/// Characters inside repeated windows, each word once.
fn dup(sc: &NgramScratch, n: usize) -> u16 {
    let (mut marked, mut end) = (0u64, 0usize);
    for (i, &c) in sc.counts.iter().enumerate() {
        if c >= 2 {
            let start = i.max(end);
            marked += sc.prefix[i + n] - sc.prefix[start];
            end = i + n;
        }
    }
    basis_points_u64(marked, *sc.prefix.last().expect("prefix starts with 0"))
}

fn basis_points_u64(num: u64, den: u64) -> u16 {
    match (u32::try_from(num), u32::try_from(den)) {
        (Ok(n), Ok(d)) => basis_points(n, d),
        _ if den == 0 => 0,
        _ => ((u128::from(num) * 10_000 + u128::from(den) / 2) / u128::from(den)).min(10_000) as u16,
    }
}

/// `words[i]` identifies word `i` after folding, `lens[i]` is its length in
/// characters.
pub fn dup5gram(words: &[u64], lens: &[u32], sc: &mut NgramScratch) -> u16 {
    debug_assert_eq!(words.len(), lens.len());
    sc.prefix.clear();
    sc.prefix.push(0);
    let mut acc = 0u64;
    for &l in lens {
        acc += u64::from(l);
        sc.prefix.push(acc);
    }
    if acc == 0 || words.len() < 5 {
        return 0;
    }
    count_windows(words, 5, sc);
    let dup5 = dup(sc, 5);
    if words.len() > BIG_DOC {
        *sc = NgramScratch::default();
    }
    dup5
}

/// Above this many words a document's buffers are freed afterwards, so each
/// worker does not keep the tables of the largest book it has seen.
pub const BIG_DOC: usize = 1 << 16;

#[cfg(test)]
mod tests {
    use super::super::words::{WordScratch, scan_words};
    use super::*;

    fn dup5(text: &str) -> u16 {
        let mut w = WordScratch::default();
        scan_words(text, &mut w);
        dup5gram(w.hashes(), w.lens(), &mut NgramScratch::default())
    }

    #[test]
    fn degenerate_loop_is_all_repetition() {
        assert_eq!(dup5(&"And he said. ".repeat(200)), 10_000);
    }

    #[test]
    fn clean_prose_is_zero() {
        let raven = "Once upon a midnight dreary, while I pondered, weak and weary, over many a quaint and curious volume \
of forgotten lore, while I nodded, nearly napping, suddenly there came a tapping, as of some one gently rapping.";
        assert_eq!(dup5(raven), 0);
    }

    #[test]
    fn drift_into_a_loop_marks_the_second_half() {
        let clean: String = (0..100).map(|i| format!("w{i} ")).collect();
        // 100 words of 290 chars, then 110 words of 490 chars that all repeat
        assert_eq!(dup5(&(clean + &"the same eleven words keep coming back here again and again ".repeat(10))), 6282);
    }

    #[test]
    fn case_and_apostrophes_fold_and_overlaps_count_once() {
        assert_eq!(dup5("Don’t stop now, go home. don't STOP now, go HOME."), 10_000);
        // "a b c d e" twice, overlapping windows: every word marked once
        assert_eq!(dup5("a b c d e a b c d e"), 10_000);
        assert_eq!(dup5("a b c d e f g h i j"), 0);
        // a repeated 4-word phrase is not enough
        assert_eq!(dup5("a b c d x a b c d y"), 0);
    }

    #[test]
    fn big_documents_free_their_buffers() {
        let words: Vec<u64> = (0..BIG_DOC as u64 + 1).map(|i| i % 1000).collect();
        let lens = vec![1u32; words.len()];
        let mut sc = NgramScratch::default();
        assert_eq!(dup5gram(&words, &lens, &mut sc), 10_000);
        assert_eq!(sc.prefix.capacity(), 0);
    }

    #[test]
    fn short_and_empty_inputs() {
        assert_eq!(dup5(""), 0);
        assert_eq!(dup5("one"), 0);
        assert_eq!(dup5("a b c d a b c d"), 0, "under two 5-word windows alike");
        assert_eq!(basis_points_u64(1, 0), 0);
        assert_eq!(basis_points_u64(u64::from(u32::MAX) * 3, u64::from(u32::MAX) * 4), 7500);
        assert_eq!(basis_points_u64(u64::MAX / 2, u64::from(u32::MAX) * 4), 10_000);
    }
}
