//! Word n-gram repetition: `top4gram`, `dup5gram`, `dup10gram`.
//!
//! The words are the tokens of `words`, folded like `uniqWords`, each
//! weighing its character count. `top4gram` is the share of characters
//! covered by every occurrence of the most frequent n-gram (ties go to the
//! one seen first), 0 unless it occurs at least twice. `dupNgram` is the
//! share of characters inside any n-gram that occurs at least twice, each
//! word counted once. Both are basis points, capped at 10000.

use hashbrown::HashMap;

use super::basis_points;
use super::words::IdentityBuild;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NgramStats {
    pub top4: u16,
    pub dup5: u16,
    pub dup10: u16,
}

/// Scratch reused across documents: window hash to slot, per-window slots
/// and counts, the counts per slot, the "repeats" flags of the previous
/// size, and the prefix sums of word lengths.
#[derive(Default)]
pub struct NgramScratch {
    map: HashMap<u64, u32, IdentityBuild>,
    hashes: Vec<u64>,
    slots: Vec<u32>,
    per_slot: Vec<u32>,
    counts: Vec<u32>,
    rep4: Vec<bool>,
    rep5: Vec<bool>,
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

/// Count every window of `n` words for which `keep(i)` holds; leaves
/// `sc.counts[i]` set (0 for windows not kept). Window hashes roll in O(1):
/// a polynomial in `B` over the word hashes, modulo 2^64.
fn count_windows(words: &[u64], n: usize, sc: &mut NgramScratch, keep: impl Fn(usize) -> bool) {
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
    for i in 0..windows {
        if keep(i) {
            let next = sc.per_slot.len() as u32;
            let slot = *sc.map.entry(sc.hashes[i]).or_insert(next);
            if slot == next {
                sc.per_slot.push(0);
            }
            sc.per_slot[slot as usize] += 1;
            sc.slots.push(slot);
        } else {
            sc.slots.push(u32::MAX);
        }
    }
    sc.counts.clear();
    sc.counts.extend(sc.slots.iter().map(|&s| if s == u32::MAX { 0 } else { sc.per_slot[s as usize] }));
}

/// Most frequent window (first seen on a tie), as basis points of the text.
fn top(sc: &NgramScratch, n: usize) -> u16 {
    let (mut best, mut at) = (1u32, 0usize);
    for (i, &c) in sc.counts.iter().enumerate() {
        if c > best {
            (best, at) = (c, i);
        }
    }
    if best < 2 {
        return 0;
    }
    let covered = u64::from(best) * (sc.prefix[at + n] - sc.prefix[at]);
    basis_points_u64(covered, *sc.prefix.last().expect("prefix starts with 0"))
}

/// Characters inside repeated windows, each word once; records the
/// per-window "repeats" flags in `rep` when given.
fn dup(sc: &NgramScratch, n: usize, mut rep: Option<&mut Vec<bool>>) -> u16 {
    if let Some(r) = rep.as_deref_mut() {
        r.clear();
    }
    let (mut marked, mut end) = (0u64, 0usize);
    for (i, &c) in sc.counts.iter().enumerate() {
        if let Some(r) = rep.as_deref_mut() {
            r.push(c >= 2);
        }
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
///
/// Every 4-gram is counted. A repeated 5-gram is two overlapping repeated
/// 4-grams and a repeated 10-gram two repeated 5-grams, so the longer sizes
/// only count windows whose parts already repeat. Every occurrence of a repeated window passes
/// that test, so its count stays exact.
pub fn ngram_stats(words: &[u64], lens: &[u32], sc: &mut NgramScratch) -> NgramStats {
    debug_assert_eq!(words.len(), lens.len());
    sc.prefix.clear();
    sc.prefix.push(0);
    let mut acc = 0u64;
    for &l in lens {
        acc += u64::from(l);
        sc.prefix.push(acc);
    }
    let mut st = NgramStats::default();
    if acc == 0 || words.len() < 4 {
        return st;
    }
    count_windows(words, 4, sc, |_| true);
    st.top4 = top(sc, 4);
    let mut rep4 = std::mem::take(&mut sc.rep4);
    dup(sc, 4, Some(&mut rep4));
    if words.len() >= 5 {
        count_windows(words, 5, sc, |i| rep4[i] && rep4[i + 1]);
        let mut rep5 = std::mem::take(&mut sc.rep5);
        st.dup5 = dup(sc, 5, Some(&mut rep5));
        if words.len() >= 10 {
            count_windows(words, 10, sc, |i| rep5[i] && rep5[i + 5]);
            st.dup10 = dup(sc, 10, None);
        }
        sc.rep5 = rep5;
    }
    sc.rep4 = rep4;
    if words.len() > BIG_DOC {
        *sc = NgramScratch::default();
    }
    st
}

/// Above this many words a document's buffers are freed afterwards, so each
/// worker does not keep the tables of the largest book it has seen.
pub const BIG_DOC: usize = 1 << 16;

#[cfg(test)]
mod tests {
    use super::super::words::{WordScratch, scan_words};
    use super::*;

    fn stats(text: &str) -> NgramStats {
        let mut w = WordScratch::default();
        scan_words(text, &mut w);
        ngram_stats(w.hashes(), w.lens(), &mut NgramScratch::default())
    }

    #[test]
    fn degenerate_loop_is_all_repetition() {
        // "and he said" x 200: 600 words of 3+2+4 = 9 chars per loop, 1800 total.
        let s = stats(&"And he said. ".repeat(200));
        // 4-grams "and he said and" (12 chars) occur 199 or 198 times: capped
        assert_eq!(s.top4, 10_000);
        assert_eq!(s.dup5, 10_000);
        assert_eq!(s.dup10, 10_000);
    }

    #[test]
    fn clean_prose_is_near_zero() {
        let s = stats(
            "Once upon a midnight dreary, while I pondered, weak and weary, over many a quaint and curious volume \
of forgotten lore, while I nodded, nearly napping, suddenly there came a tapping, as of some one gently rapping.",
        );
        assert_eq!((s.top4, s.dup5, s.dup10), (0, 0, 0));
    }

    #[test]
    fn drift_into_a_loop_marks_the_second_half() {
        let clean: String = (0..100).map(|i| format!("w{i} ")).collect();
        let s = stats(&(clean + &"the same eleven words keep coming back here again and again ".repeat(10)));
        // 100 words of 290 chars, then 110 words of 490 chars that all repeat
        assert_eq!(s.dup10, 6282);
        assert_eq!(s.dup5, s.dup10);
    }

    #[test]
    fn case_and_apostrophes_fold_and_overlaps_count_once() {
        assert_eq!(stats("Don’t stop now. don't STOP now."), stats("don't stop now don't stop now"));
        // "a b c d e" twice, overlapping windows: every word marked once
        assert_eq!(stats("a b c d e a b c d e").dup5, 10_000);
        assert_eq!(stats("a b c d e f g h i j").dup5, 0);
    }

    #[test]
    fn big_documents_free_their_buffers() {
        let words: Vec<u64> = (0..BIG_DOC as u64 + 1).map(|i| i % 1000).collect();
        let lens = vec![1u32; words.len()];
        let mut sc = NgramScratch::default();
        let s = ngram_stats(&words, &lens, &mut sc);
        assert_eq!(s.dup10, 10_000);
        assert_eq!(sc.prefix.capacity(), 0);
    }

    #[test]
    fn short_and_empty_inputs() {
        assert_eq!(stats(""), NgramStats::default());
        assert_eq!(stats("one"), NgramStats::default());
        let s = stats("one two one two");
        assert_eq!((s.top4, s.dup5), (0, 0), "one 4-gram window, seen once");
        let s = stats("a b c d a b c d");
        assert_eq!((s.top4, s.dup5), (10_000, 0), "\"a b c d\" twice covers every word");
        assert_eq!(basis_points_u64(1, 0), 0);
        assert_eq!(basis_points_u64(u64::from(u32::MAX) * 3, u64::from(u32::MAX) * 4), 7500);
        assert_eq!(basis_points_u64(u64::MAX / 2, u64::from(u32::MAX) * 4), 10_000);
    }
}
