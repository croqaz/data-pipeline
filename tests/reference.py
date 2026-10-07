#!/usr/bin/env python3
"""Independent reference for the version-1 features.

Generates tests/fixtures/golden.jsonl (the documents) and
tests/fixtures/golden_expected.json (what `pipeline` must compute for each),
using only the definitions in PROPOSAL.md section 5.4. Nothing here looks at
the Rust code. Run it once; the Rust test `golden` compares against the
output byte for byte.
"""
import json
import math
import re
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"

MIN_LEN = 100
MIN_UNIQ = 10
MAX_UNIQ = 132
MIN_WORDS = 3
MAX_LEN = 10_000_000

# Unicode White_Space property (Unicode 15) plus the BOM, as the TS tokenizer saw it.
WHITESPACE = set(
    [chr(c) for c in range(0x09, 0x0E)]
    + [" ", "\u0085", " ", " "]
    + [chr(c) for c in range(0x2000, 0x200B)]
    + [" ", " ", " ", " ", "　", "﻿"]
)

ALPHA_RE = re.compile(r"^[A-Za-z][A-Za-z'’\-]*$")
VOWEL_RE = re.compile(r"[aeiouy]", re.I)


def split_words(text):
    out = []
    cur = []
    for ch in text:
        if ch in WHITESPACE:
            if cur:
                out.append("".join(cur))
                cur = []
        else:
            cur.append(ch)
    if cur:
        out.append("".join(cur))
    return out


def strip_token(tok):
    i, j = 0, len(tok)
    while i < j and not tok[i].isalnum():
        i += 1
    while j > i and not tok[j - 1].isalnum():
        j -= 1
    return tok[i:j]


def fold(tok):
    return "".join(chr(ord(c) + 32) if "A" <= c <= "Z" else ("'" if c == "’" else c) for c in tok)


def word_stats(text):
    words = alpha = vowel = 0
    seen = set()
    for raw in split_words(text):
        tok = strip_token(raw)
        if not tok:
            continue
        words += 1
        seen.add(fold(tok))
        if ALPHA_RE.match(tok):
            alpha += 1
            if VOWEL_RE.search(tok):
                vowel += 1
    return words, len(seen), alpha, vowel


QUOTES = {b'"', b"'", "“".encode(), "”".encode(), "‘".encode(), "’".encode()}
WS = set(b" \t\n\r\x0b\x0c")


def quote_len(b, i):
    if i < len(b) and b[i : i + 1] in QUOTES:
        return 1
    if b[i : i + 3] in QUOTES:
        return 3
    return 0


def skip_ws(b, i):
    while i < len(b) and b[i] in WS:
        i += 1
    return i


def count_sentences(text):
    b = text.encode("utf-8")
    n = len(b)
    count = 0
    i = 0
    while i < n:
        c = b[i : i + 1]
        if c in (b".", b"!", b"?"):
            j = i + 1 + quote_len(b, i + 1)
            k = skip_ws(b, j)
            if k > j and k < n and (b"A"[0] <= b[k] <= b"Z"[0] or quote_len(b, k) > 0):
                count += 1
                i = k
            else:
                i = j
        elif c == b"\n":
            j = i + 1
            while j < n and b[j] in b" \t\r":
                j += 1
            if j < n and b[j : j + 1] == b"\n":
                k = skip_ws(b, j)
                if k < n and (chr(b[k]).isascii() and chr(b[k]).isalpha() or quote_len(b, k) > 0):
                    count += 1
                i = max(k, j + 1)
            else:
                i = j
        else:
            i += 1
    return count + 1


def basis_points(num, den):
    if den == 0:
        return 0
    return min(10_000, (num * 10_000 + den // 2) // den)


def folded_words(text):
    out = []
    for raw in split_words(text):
        tok = strip_token(raw)
        if tok:
            out.append(fold(tok))
    return out


STOPWORDS = {
    w
    for line in (HERE.parent / "src" / "features" / "stopwords.txt").read_text(encoding="utf-8").splitlines()
    if not line.lstrip().startswith("#")
    for w in line.split()
}


def mean_word_len(words):
    """Mean characters per word in hundredths, rounded half up."""
    if not words:
        return 0
    return min(65_535, (sum(len(w) for w in words) * 100 + len(words) // 2) // len(words))


def stop_share(words):
    return basis_points(sum(w in STOPWORDS for w in words), len(words))


def ngram_counts(words, n):
    grams = [tuple(words[i : i + n]) for i in range(len(words) - n + 1)]
    return grams, Counter(grams)


def dup_ngram(words, n):
    """Characters of the words inside any n-gram occurring more than once,
    each word counted once, over all word characters."""
    total = sum(len(w) for w in words)
    grams, counts = ngram_counts(words, n)
    marked = [False] * len(words)
    for i, g in enumerate(grams):
        if counts[g] >= 2:
            for j in range(i, i + n):
                marked[j] = True
    if total == 0:
        return 0
    return basis_points(sum(len(w) for w, m in zip(words, marked) if m), total)


def char_entropy(text):
    """Order-0 Shannon entropy as basis points of 4.47 bits per character,
    summed over the counts in ascending order (the Rust code sums the same way)."""
    counts = sorted(Counter(text).values())
    n = sum(counts)
    if n == 0:
        return 0
    h = math.log2(n) - sum(c * math.log2(c) for c in counts) / n
    return min(65_535, round(max(h, 0.0) / 4.47 * 10_000))


LATIN_PUNCT = set(".,;:!?'\"-()\u2018\u2019\u201c\u201d\u2013\u2014")


def quality_weight(ch):
    """Twice the quality weight of one character."""
    o = ord(ch)
    if ("a" <= ch <= "z") or ("A" <= ch <= "Z"):
        return 4
    if ch.isdigit() and o < 128 or ch in " \n\r":
        return 2
    if ch in LATIN_PUNCT:
        return 1
    in_range = 0x80 <= o <= 0x24F or 0x370 <= o <= 0x3FF or 0x1E00 <= o <= 0x1EFF or 0xFB00 <= o <= 0xFB06
    if in_range and ch.isalpha():
        return 4
    return -1


def quality(text):
    """Mean weight minus 0.75, in hundredths, clamped to 0..12500, half up."""
    n = len(text)
    if n == 0:
        return 0
    num = sum(quality_weight(c) for c in text) * 5_000 - 7_500 * n
    if num <= 0:
        return 0
    return min(12_500, (num + n // 2) // n)


def count_lines(text):
    """Lines (split on \\n or \\r) with a character other than ASCII whitespace."""
    return sum(1 for line in re.split(r"[\r\n]", text) if line.strip(" \t\x0b\x0c"))


def compute(text, max_len=MAX_LEN):
    nbytes = len(text.encode("utf-8"))
    if nbytes <= MIN_LEN:
        return {"drop": "too_short"}
    if nbytes > max_len * 4:
        return {"drop": "too_long"}
    length = len(text)
    if length <= MIN_LEN:
        return {"drop": "too_short"}
    if length > max_len:
        return {"drop": "too_long"}
    uniq = len(set(text))
    if uniq <= MIN_UNIQ or uniq > MAX_UNIQ:
        return {"drop": "uniq_chars"}
    words, uniq_words, alpha, vowel = word_stats(text)
    if words < MIN_WORDS:
        return {"drop": "few_words"}
    folded = folded_words(text)
    return {
        "len": length,
        "bytes": nbytes,
        "words": words,
        "uniqWords": uniq_words,
        "lines": count_lines(text),
        "sentences": count_sentences(text),
        "uniqChars": uniq,
        "vowel": basis_points(vowel, alpha),
        "alpha": basis_points(alpha, words),
        "entropy": char_entropy(text),
        "quality": quality(text),
        "meanWordLen": mean_word_len(folded),
        "stopWords": stop_share(folded),
        "dup5gram": dup_ngram(folded, 5),
    }


PROSE = (
    "It was the best of times, it was the worst of times, it was the age of wisdom, it was the age of "
    "foolishness, it was the epoch of belief, it was the epoch of incredulity, it was the season of Light, "
    "it was the season of Darkness, it was the spring of hope, it was the winter of despair."
)

RAVEN = """Once upon a midnight dreary, while I pondered, weak and weary,
Over many a quaint and curious volume of forgotten lore,
While I nodded, nearly napping, suddenly there came a tapping,
As of some one gently rapping, rapping at my chamber door.
"'Tis some visitor," I muttered, "tapping at my chamber door,
Only this, and nothing more."

Ah, distinctly I remember it was in the bleak December,
And each separate dying ember wrought its ghost upon the floor.
Quoth the Raven, "Nevermore."
Quoth the Raven, "Nevermore."
"""

OCR = (
    "THE GENTLEMAN'S MAGAZINE.\n"
    "Tlie wliole of tbe coun- try was in a ferment ; tbe people assembled in great nurn- bers, and tbe "
    "magistrates were at a loss how to act. [Illegible] ** 1 2 3 ,, '' tbe end.\n"
    "THE GENTLEMAN'S MAGAZINE.\n"
)

ARCHAIC = (
    "Of Man’s firſt diſobedience, and the fruit of that forbidden tree, whoſe mortal taſte brought "
    "death into the World, and all our woe, with loſs of Eden, till one greater Man reſtore us, and "
    "regain the bliſsful ſeat. ﬁrst ﬂight, café naïve coöperate."
)

LEDGER = "1842 .... Rent 12 4 6\n1843 .... Tithe 3 10 0\n" * 6 + "Total ...... 94 12 9 £ s. d.\n"

DEGENERATE = "And he said. " * 40

MIXED = (
    "He said “wait.” Then　nothing.\tLater, ‘yes’ came the reply! Really? "
    "日本語の文章 \U0001f600 emoji and tabs\there; don’t stop, won't go, "
    "well-known facts, 1,000 items, x9 and 9x, I a.\r\n\r\nNew paragraph after CRLF. \"Quoted start.\" End."
)

CASES = [
    {"name": "prose", "text": PROSE},
    {"name": "raven", "text": RAVEN},
    {"name": "ocr_noise", "text": OCR},
    {"name": "archaic_long_s", "text": ARCHAIC},
    {"name": "ledger", "text": LEDGER},
    {"name": "degenerate", "text": DEGENERATE},
    {"name": "mixed_unicode", "text": MIXED},
    {"name": "long_prose", "text": (PROSE + "\n\n") * 200},
    {"name": "too_short", "text": "Just a few words, under the limit."},
    {"name": "exactly_100_chars", "text": "x" * 100},
    {"name": "101_chars_one_word", "text": "y" * 101},
    {"name": "few_unique_chars", "text": "ab ab ab " * 20},
    {"name": "two_words", "text": "wordone " + "abcdefghijklmnopqrstuvwxyz0123456789" * 4},
    {"name": "many_unique_chars", "text": "".join(chr(0x100 + i) for i in range(200)) + " spread out words here"},
    {"name": "whitespace_only_plus", "text": " \n\t" * 50 + "one two three"},
    {"name": "json_escapes", "text": "Line one.\nLine two with a tab\there and a quote \" and a backslash \\ and é escaped. " * 3},
    {"name": "no_sentence_end", "text": "words " * 30},
    {"name": "all_caps_headline", "text": "THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG. THE FIVE BOXING WIZARDS JUMP QUICKLY. PACK MY BOX WITH FIVE DOZEN JUGS."},
    {"name": "numbers_and_symbols", "text": "#### 1234 5678 9012 3456 7890 ##### $$$$ %%%% &&&& **** ((( ))) [[[ ]]] {{{ }}} <<< >>> ~~~ ^^^ 42 43 44 45 46 47 48 49 50 51 52 53"},
    {"name": "drift_into_loop", "text": PROSE + " " + "the same eleven words keep coming back here again and again " * 12},
    {"name": "refrain", "text": RAVEN * 3},
    {"name": "cjk_only", "text": "これは日本語の文章です。" * 12 + " 単語 単語 単語"},
]


def main():
    FIXTURES.mkdir(parents=True, exist_ok=True)
    with open(FIXTURES / "golden.jsonl", "w", encoding="utf-8") as f:
        for c in CASES:
            f.write(json.dumps({"name": c["name"], "text": c["text"]}, ensure_ascii=False) + "\n")
    expected = {c["name"]: compute(c["text"]) for c in CASES}
    with open(FIXTURES / "golden_expected.json", "w", encoding="utf-8") as f:
        json.dump(expected, f, indent=1, ensure_ascii=False, sort_keys=True)
        f.write("\n")
    kept = sum(1 for e in expected.values() if "drop" not in e)
    print(f"{len(CASES)} cases, {kept} kept, {len(CASES) - kept} dropped")


if __name__ == "__main__":
    main()
