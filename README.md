# Data-Pipeline

JSONL text corpus indexer for training language models. One way in
(`import`), one way out (`export`), a handful of commands to look at what
is inside.

## Commands

```sh
pipeline import --db DIR [-s LABEL] [--source-key KEY] [-k TEXT_KEY] [-e key1,key2]
                [--max-len N] [-w N] [--offset N] [--limit N] [--chunk-mb N] [--batch-size N]
                [--rejects FILE] [--on-error skip|abort] [--nocow] [--dry-run] FILES...
pipeline stats  --db DIR [EXPR] [--limit N] [-t N]
pipeline count  --db DIR [EXPR] [-t N]
pipeline query  --db DIR [EXPR] [-l N] [--sort FEATURE [--asc] | --random [--seed S]]
                [--head N | --full | --json | --ids] [-t N]
pipeline export --db DIR [EXPR] -o OUT.jsonl [-f FIELDS] [--limit N]
                [--sample F --seed S] [--top N]
                [--header TEMPLATE | --header-file FILE] [--header-sep SEP]
                [--shard N] [--zstd] [--zstd-level N] [-t N] [--verify] [--force]
pipeline check  --db DIR [--deep] [-t N] [--examples N]
pipeline info   --db DIR
pipeline show   --db DIR ID... [--json] [--head N]
pipeline compact --db DIR
pipeline delete --db DIR (EXPR | --id ID...) [--dry-run] [--limit N] [-y]
                [--backup FILE] [--force] [-t N]
pipeline explore --db DIR [-p PORT] [--bind ADDR] [-t N]
pipeline recompute --db DIR [--dry-run | --force] [-t N] [--batch-size N]
```

`import` reads `.jsonl`, `.jsonl.gz` and `.jsonl.zst`. Every line must be
a JSON object with a string under `--text-key` (default `text`). A string
`-s NAME` labels every document with that source and ignores the record's
own source key. Without `-s`, a string under `--source-key` (default
`source`) labels the record, and records without one get `cli`.
Re-importing a stored document with the same text updates its source and
`extra` to the new values (the report counts these as Updated), so a
re-import fixes wrong labels. Source names that no document uses any more
are dropped at the end of such an import, after a `delete`, by `compact`
and by `check` (only when no meta record is corrupt), so `info` lists only
live sources.
`-e` keeps the named keys as metadata; they come back out unchanged and are
reachable in expressions as `extra.<key>`. `KEY=NAME` stores a key under
another name: `-e Publication=title,titleCode=pubID,year` keeps
`Publication` as `title`, `titleCode` as `pubID` and `year` as it is. Each
key and each stored name may appear once.

The store engine is fjall, a pure-Rust LSM tree with the text keyspace
in separate value logs, chosen after a bake-off against RocksDB,
LMDB and redb (`BENCHMARKS.md` has the tables and the verdicts; the other
engines were removed from the code and live on in the git history). A
store directory carries a `pipeline.backend` marker naming the engine, so
a foreign directory is refused rather than overwritten. Two operating
rules come from the bake-off: run `compact` after every import,
because a fresh store serves point reads about 15x slower until its
level-0 tables are merged (the compaction itself takes seconds), and
write exports to a drive other than the store's, because fjall's blob
reads always come from disk and compete badly with large sequential
writes on the same NVMe.

The bake-off numbers here and in `BENCHMARKS.md` were measured by earlier
builds with a different feature set: they computed `compress` (an LZ4
ratio), `top4gram` (the most frequent 4-word phrase) and `dup10gram`
(repeated 10-word sequences), which are gone, and none of them had
`dictHit`, `quality` or `lines`. The first runs computed fewer features
still. The numbers are kept as they were measured.
`--dry-run` parses and counts everything without opening a store, which
is the quickest way to validate a new dataset or to measure the CPU side
alone. Progress goes to stderr: an `Importing:` line as each file is
opened, so a glob of many files shows where it is, then a count every
100,000 lines. The report on stdout has one `Imported:` block per file
with its counts, or `Not reached:` for a file that a `--limit` stopped
the import before, and ends with the process's peak resident memory,
split into heap and mapped file pages.

One environment variable:
`PIPELINE_FJALL_COMPRESSION=none` stores blocks and blobs uncompressed, for
a btrfs volume that compresses by itself.

## Looking inside

`info` reads no record: backend, approximate count, disk size, source
labels, the feature list and schema, then per-keyspace facts from the
engine (table and blob file counts, files at level 0, cache size).

`stats` scans the documents matching EXPR (all of them without one) and
prints the count per source, a table of every feature (present, mean, min,
P5, P50, P95, max), then the totals of `len`, `bytes`, `words`, `lines`
and `sentences`.

`show` prints a document: id, source, every feature, the stored metadata,
then the text after a rule. An id is 32 hex characters, or an even-length
prefix of at least 4 that matches exactly one document; an ambiguous
prefix lists what it matched. `--head N` keeps the first N characters,
`--json` prints the same line `export` would.

`query` prints the documents an expression matches, where `count` gives
only their number. It shows 20 by default (`-l N`), each in the `show`
layout under a `=== Match 3 of 20, scanned 1,234 ===` line (without
`--sort`, the number of documents read to reach that match); a text over 16,000
characters shows its first and last 8,000. `--head N` keeps the first N
characters, `--full` the whole text, `--json` prints the export line and
`--ids` only the ids, ready for `delete --id`. The count of what was shown
and how much was scanned goes to stderr.

Without `--sort` it reads in key order and stops at the limit, so the
answer is immediate. Keys are hashes of the text, so key order has nothing
to do with source, length or content and the first N matches are a fair
sample; `--random` starts at a random key and wraps around for a different
sample each run (`--seed S` repeats one). `--sort FEATURE` scans the whole
store in parallel and shows the highest values first, `--asc` the lowest;
the summary then gives the total number of matches. On a 343k-document
store a sorted query takes 18 ms; on 98.5 M documents it takes under 4 s
on 16 threads.

`delete` with the same expression and `--limit N` removes exactly the first
N matches a plain `query` showed, and stops scanning at the same place. Its
`--limit` counts matches, not documents scanned: pass the match number, not
the scanned number.

```sh
pipeline query --db /data/corpus 'dup5gram > 30'                 # 20 examples, quickly
pipeline query --db /data/corpus 'dup5gram > 30' --sort dup5gram # the worst first
pipeline query --db /data/corpus 'dup5gram > 30' --sort dup5gram --asc --head 2000
                                                              # the borderline cases
pipeline query --db /data/corpus 'dup5gram > 50' -l 1000 --ids > ids.txt
```

`check` is the store's `fsck`. One parallel pass merges the three
keyspaces in key order, with no point reads, and reports metas that fail
to decode, texts and metadata missing or present without a meta
(orphans), metadata present or absent against the `has_extra` flag, and
source ids without a label. `--deep` also reads every text and checks that
it is UTF-8, that its BLAKE3 key matches, and that the stored features
equal a fresh computation, so a changed scanner shows up as "stale
features". Any problem makes the command exit 1 after printing the
counts and example ids. Run it after an interrupted import, after a
delete, or before cutting a training set. Its one write: after a pass
with no corrupt meta, it removes source names that no document uses.

`compact` flushes, rewrites every keyspace into its most compact form and
reports the size before and after; on the bake-off store (4.1 M
documents) it took 6 s, and on a 100 GB store it can take 200 s.
Nothing else ever compacts implicitly: it is the operator's call when to
pay for it, and the rule is to run it after every import, since a fresh
store serves point reads about 15x slower until then.

## Features

| name | what it is |
|---|---|
| `len`, `bytes` | characters (Unicode scalars) and UTF-8 bytes |
| `uniqChars` | distinct characters |
| `words`, `uniqWords` | whitespace-separated tokens after stripping punctuation; distinct ones after lowercasing |
| `meanWordLen` | mean characters per word, two decimals: English prose reads about 4.6 to 4.7 at the median. Words split on whitespace only, so "potter—Andrew" is one long word: lists joined by dashes read above 7, number tables below 3.5 |
| `stopWords` | percent of words in English stop-words: modern function words plus early modern forms (thou, hath, doth, unto) and spellings (haue, vnto, vpon). English prose reads about 52 in both the 16th and the 19th century; lists, tables and catalogues read under 15, Latin, Law French and Welsh under 5 |
| `lines` | lines (split on `\n` or `\r`) with any character other than whitespace. `words / lines` is the mean words per line: Gutenberg books keep their hard line breaks and read about 11 at the median, newspaper text 40 to 70 |
| `sentences` | sentence boundaries, counted by punctuation plus a capital, or a blank line |
| `vowel` | percent of alphabetic words that contain a vowel. Prose reads 99 to 100; 80 to 92 is lists of names and addresses and catalogue entries; under 80 is stock and price tables, where most "words" are abbreviations like `Mond.` or `Ct.` |
| `alpha` | percent of words that are purely alphabetic (`[A-Za-z][A-Za-z'’-]*`), the rest being numbers, prices, dates and codes. Prose reads about 99 at the median; 70 to 90 is prose with many dates, sums or addresses (notices, catalogue entries); under 70 is tables and subscription lists |
| `dictHit` | percent of alphabetic words in a 98,304 most frequent words of a corpus of historical English (newspapers, books, parliamentary debates). Words are folded like `uniqWords` before the lookup, so `God`, `GOD` and `god` all match; `X's` also matches when `X` is listed. The median is 99.7 for Hansard, 99.0 for NCSE, 98.4 for Gutenberg, 98.2 for EEBO and 94.8 for American Stories OCR; tables and lists of names read under 70 |
| `dup5gram` | percent of the words inside a 5-word sequence that occurs more than once in the document (words folded like `uniqWords`). Most documents read 0 and 95% read under about 10. 5 to 15 is normal: a heading or a title repeated in the body. 15 to 40 is verse refrains, market reports and proclamations with set formulas; 40 to 70 is rules, charters and advertisements built from one repeated phrase; above 70 the document is mostly the same text more than once. Short texts reach high values from a single repeated phrase. |
| `entropy` | order-0 Shannon entropy of the characters as a percent of 4.47 bits per character; 4.47 bits is typical of English prose, which reads about 99.5 at the median; OCR noise reads higher |
| `quality` | how much of the text is letters, as opposed to digits, punctuation, symbols, tabs or other scripts; higher is better. Prose reads 100 to 106; 80 to 95 is prose broken up by numbers and headings (market reports, notices); under 80 is tables, subscription lists and contents pages with dot leaders. Text made only of letters would read 125, and a page of symbols 0 |

### Recomputing features

`recompute` is how a store catches up with the scanners: add a feature or
change a definition, build, run it, and every record carries the new
values. No re-import. It reads every text, computes the features exactly
as `import` does, and rewrites the `meta` record only where the result
differs; the text, the metadata, the key, the source and the flags are
never touched, and a record that already matches is left alone, so a
second run writes nothing. A document that today's prefilter would
reject keeps its old record and is counted under `Rejected` with the
reason; removing such documents is `delete`'s job. Records with a
problem (no text, a text that is not UTF-8, a meta that does not decode)
are counted and skipped for `check` to report. `--dry-run` prints the
same counts and writes nothing. `--force` rewrites every readable
record even when nothing changed, which re-encodes a store after a
record layout change. Rewritten records sit in fresh tables until the
next `compact`, which the report reminds you of. On the bake-off store
(4.1 M documents, 18 GB of text) a full recompute takes 21 s on 16
threads whether it writes nothing or every record, and the compaction
after it 12 s (`BENCHMARKS.md`).

The two word lists, `src/features/stopwords.txt` and
`src/features/vocab.txt`, are compiled into the binary with
`include_str!`: the executable can be copied anywhere and needs no files
beside it. Editing either list changes `stopWords` or `dictHit`, so
rebuild and `recompute` afterwards. `check --deep` reports records whose
stored features differ from this build's, including features missing from
the record.

```sh
pipeline recompute --db /data/corpus --dry-run
pipeline recompute --db /data/corpus && pipeline compact --db /data/corpus
```

## Expressions

```
words >= 50 && vowel > 95 && source == "gutenberg"
uniqWords / words < 0.2 || sentences < 2
extra.year < 1850 && has(dictHit) && text.contains("Lorem")
source in ["EEBO", "ECCO"] && extra.title.matches("(?i)^index")
```

Names: `len`, `bytes`, `uniqChars`, `words`, `uniqWords`, `meanWordLen`, `stopWords`, `lines`, `sentences`, `vowel`, `alpha`, `dictHit`, `dup5gram`, `entropy`, `quality`, `source`, `id`, `text`, `extra.<key>`.
Functions: `abs`, `min`, `max`, `len`, `num`, `lower`, `has`.
String methods: `contains`, `startsWith`, `endsWith`, `matches`, `lower`.
A missing feature is `null`; any comparison with `null` is `null`, and `null` never matches; use `has(x)` to test presence!

Expressions that mention `text` or `extra.*` read those keyspaces; all others scan only the small `meta` keyspace.

### Advanced expressions

Expressions do arithmetic on features, so a ratio of two stored counts
needs no column of its own. Division by zero gives `null`, and `num(x)`
turns a comparison into 1 or 0, so conditions can be counted.

| expression | what it measures, what it is for |
|---|---|
| `uniqWords / words` | type-token ratio: distinct words over words. Low means repetitive text or a looping generator; it also falls as documents get longer, so compare documents of similar size |
| `words / sentences` | mean sentence length in words. Under 8 is lists, headlines and captions; very high is text without sentence punctuation (tables, OCR runs) |
| `words / lines` | mean words per line. Under 6 is verse, lists, tables and indexes; hard-wrapped books read about 11, prose paragraphs on one line 40 and up |
| `len / lines` | mean characters per line, the same signal in characters |
| `len / uniqChars` | characters per distinct character. Low for short or symbol-heavy text, where many characters appear once |
| `bytes / len` | 1 for pure ASCII; above 1 when the text has accented letters, typographic quotes or other scripts |
| `100 - alpha` | percent of words that are not purely alphabetic: numbers, prices, dates, codes. High in tables, accounts and price lists |
| `100 - dictHit` | percent of alphabetic words outside the vocabulary: OCR errors, rare or archaic words, other languages |
| `entropy > 105` | many capital letters or symbols: headlines, notices, OCR noise |
| `quality < 90` | punctuation and symbols crowding out letters: tables, dot leaders, markup |
| `dup5gram > 30` | loops, refrains and copied passages of 5 words or more |
| `num(100 - alpha > 10) + num(entropy > 105) + num(words / lines < 6) + num(words / sentences < 8) + num(quality < 90) >= 2` | back matter and other non-prose: two or more of the signals above. Finds notices, advertisements and captions; the thresholds are starting points to check with `query` |

## Exploring

`explore` serves a one-page browser over the store on localhost and
answers GET only, so it cannot change anything. Open the printed address,
type an expression and press Random: the page shows a random document
that matches, with every feature, the stored metadata and the text. The
arrow keys step to the previous and next key, `r` draws again, the
"match" buttons step to the previous and next match of the expression in
key order, List shows fifty matches with a text snippet and a More
button, and Count runs the full scan `count` would. An id or prefix
pasted into the jump box opens that document. The address bar carries
`#/id/...`, so the back button and bookmarks work, and the expression
survives a reload.

Random is cheap because keys are uniform hashes: sixteen random bytes
and a seek land on a random document. With an expression the seek is
followed by a forward scan of up to 10,000 documents, which finds a
match whenever the expression keeps more than about one document in a
few thousand. A rarer filter gets "no match in 10,000 documents" and the
advice to loosen it or raise `tries`. Count decodes every meta record,
and reads every text as well when the expression mentions `text`.

Measured on the bake-off store (4,137,434 documents, 13.5 GB), one
request at a time:

| request | time |
|---|---|
| random, next, prev, a document by id | under 1 ms (49 ms for the first, cold) |
| random with `text.contains("Nevermore")` | 110 ms, 4,147 documents looked at |
| list 5 matches of `uniqWords / words < 0.1` | 12 ms, 10,056 scanned |
| count `words > 1000` | 0.22 s on 16 threads (3.9 s on 98.5 M documents) |
| count `text.contains("Nevermore")` | 12.3 s, every text read |

The JSON API, for scripts:

```
GET /api/info                    store facts; reads no record
GET /api/doc/{id}                one document, by full id or prefix
GET /api/random?expr=&tries=     a random document, matching expr when given
GET /api/next[/{id}]?n=1         the n-th document after id (from the start without one)
GET /api/prev[/{id}]?n=1         the n-th document before id (from the end without one)
GET /api/query?expr=&from=&dir=&limit=&scan=&head=
                                 matches in key order from a cursor, with a text snippet
GET /api/count?expr=             the full scan
```

Errors come back as `{"error": "..."}`: 400 for a bad expression or
parameter, 404 for a missing document or an empty draw, 405 for anything
but GET. The server binds 127.0.0.1 unless `--bind` says otherwise, `-p 0`
picks a free port and prints it, and `-t` sets the threads Count uses.

```sh
pipeline explore --db /data/corpus
pipeline explore --db /data/corpus -p 9000 -t 8
```

## Export

`export` scans `meta` for the documents matching EXPR, fetches them on
`-t` threads and writes one JSON object per line in key order. Keys are
hashes, so that order is a fixed shuffle: two exports with the same options
produce the same file, and a training set needs no further mixing.

**Fields.** The default line holds `id`, `source`, every stored feature,
`text`, then the imported metadata keys flattened at the top level. `-f`
picks and orders a subset from `id`, `source`, `text`, `features`, any
feature name, `extra` (the metadata as one nested object), `extra.*` (all
keys flattened) and `extra.<key>` (one key). A flattened metadata key whose
name belongs to a built-in field is skipped and counted in the report. A
field list without `text` never reads the text keyspace.

**Headers.** `--header` prepends a rendered template to each text, with
`--header-sep` between them (default: a blank line). A placeholder is any
expression: `{extra.title}`, `{source}`, `{words}`, `{lower(extra.author)}`.
A line whose placeholders are all `null` is dropped, and when every
placeholder line is dropped so is the whole header, so framing lines never
stand alone. `{{` and `}}` are literal braces; `\n` and `\t` in the
argument are turned into the characters they name. `--header-file` reads
the template from a file, the comfortable way to write one like this:

```
---
year = {extra.year}
title = {extra.title}
author = {extra.author}
---
```

**Selection.** `--sample 0.1 --seed 7` keeps a tenth, decided per document
from its key, so the same options select the same documents on every run.
`--top N` keeps the N with the most words, most first. `--limit N` stops
after N documents in key order.

**Output.** Every file is written as `name.partial` and renamed when
complete, so a file with the final name is always whole; a failed run
leaves the `.partial` behind for inspection. `--shard N` writes
`name-0001.jsonl`, `name-0002.jsonl`, ... of N documents each. `--zstd`, or
a `.zst` suffix on the name, compresses with the multi-threaded zstd
encoder (`--zstd-level`, default 3). `--verify` re-reads every output file
and parses every line. Existing output files are never overwritten without
`--force`. `-o -` streams to stdout and prints the report on stderr.

The report lists scanned, matched, selected and written counts, the time
of each phase and every file; written must equal selected or the command
fails with exit code 1.

## Deleting

`delete` removes documents selected by an expression or by `--id` (full
ids or unambiguous prefixes, repeatable). It selects first and tells you
how many documents matched, then asks you to type `yes` before touching
anything; `-y` skips the question for scripts, and `--dry-run` prints the
selected ids on stdout and changes nothing. `--limit N` keeps the first N
in key order. `--backup FILE` exports the selected documents with every
field to a JSONL file before they go, so a wrong expression is a
re-import away from undone; an existing file is never overwritten without
`--force`, and if the backup fails nothing is deleted. Deleted documents
disappear from every keyspace at once; the disk space comes back at the
next `compact`.

```sh
pipeline delete --db DATA/x --dry-run 'source == "gemma" && uniqWords / words < 0.1'
pipeline delete --db DATA/x --backup /data/trash/gemma-loops.jsonl 'source == "gemma" && uniqWords / words < 0.1'
pipeline delete --db DATA/x -y --id 000015e75a51d648d84a58a946361a8f
```

## Build

Rust comes from mise:

```sh
mise install            # rust 1.99 + cargo-nextest, cargo-llvm-cov
mise exec -- cargo build --release
```

The binary is `target/release/pipeline`. `.cargo/config.toml` compiles with
`target-cpu=native`, so a binary built here will not run on an older CPU.

## Tests and coverage

```sh
mise exec -- cargo fmt --check
mise exec -- cargo clippy --all-targets -- -D warnings
mise exec -- cargo test
mise exec -- cargo llvm-cov --tests --ignore-filename-regex 'src/main\.rs' --summary-only
```

`rustfmt.toml` sets the style (140 columns, "Max" heuristics); run
`cargo fmt` before committing.

`tests/explore.rs` starts the explorer on a free port and hits every
route over a real socket; the handlers themselves are tested without one.

`tests/reference.py` is an independent Python implementation of the
features; `tests/golden.rs` checks the Rust scanners against its output.
Regenerate the fixture with `python3 tests/reference.py` after changing a
definition in both places.

## Layout

```
src/main.rs        three-line shim
src/lib.rs         run(), command dispatch
src/cli.rs         clap definitions
src/features/      scanners: chars (with quality and lines), words (with stopWords and dictHit), sentences, ngrams (dup5gram); the word lists; prefilter
src/expr/          lexer, parser, compiler, evaluator
src/meta.rs        CBOR record + feature registry
src/hash.rs        16-byte BLAKE3 key of normalized text
src/jsonl/         chunked reader (.gz/.zst aware)
src/store/         Store trait, the fjall engine, a null store for dry runs
src/import.rs      reader -> workers -> writer pipeline
src/stats.rs       parallel meta scan, histograms, table, totals
src/count.rs
src/query.rs       first N matches in key order, or the top N by a feature
src/export/        header templates, field emitter, sharded sink, orchestration
src/check.rs       three-keyspace merge audit, optional deep re-hash
src/info.rs        info and compact
src/show.rs        id resolution and the readable document layout
src/delete.rs      selection, backup, batched deletes
src/recompute.rs   re-read every text, rewrite the features that changed
src/explore/       the read-only browser: handlers, the tiny_http layer, index.html
```
