# pipeline

JSONL text corpus indexer for training tiny language models. One way in
(`import`), one way out (`export`), a handful of commands to look at what
is inside. See `PROPOSAL.md` for the design and
`BENCHMARKS.md` for measurements on the one machine this targets.

## Build

Rust comes from mise only:

```sh
mise install            # rust 1.99 + cargo-nextest, cargo-llvm-cov
mise exec -- cargo build --release
```

The binary is `target/release/pipeline`. `.cargo/config.toml` compiles with
`target-cpu=native`, so a binary built here will not run on an older CPU.

## Commands

```sh
pipeline import --db DIR [-s LABEL] [-e key1,key2]
                [--max-len N] [-w N] [--offset N] [--limit N]
                [--rejects FILE] [--on-error skip|abort] [--nocow] [--dry-run] FILES...
pipeline stats  --db DIR [EXPR] [--limit N] [-t N]
pipeline count  --db DIR [EXPR] [-t N]
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
```

`import` reads `.jsonl`, `.jsonl.gz` and `.jsonl.zst`. Every line must be
a JSON object with a string under `--text-key` (default `text`). A string
under `--source-key` (default `source`) overrides `-s` for that record.
`-e` keeps the named keys as metadata; they come back out unchanged and are
reachable in expressions as `extra.<key>`.

The store engine is fjall, a pure-Rust LSM tree with the text keyspace
in separate value logs, chosen after a 20 GB bake-off against RocksDB,
LMDB and redb (`BENCHMARKS.md` has the tables and the verdicts; the
other engines were removed from the code on 2026-10-05). A store
directory carries a `pipeline.backend` marker naming the engine, so a
foreign directory is refused rather than overwritten. Two operating rules
come from the bake-off: run `compact` after every import, because a
fresh store serves point reads about 15x slower until its level-0 tables
are merged (the compaction itself takes seconds), and write exports to a
drive other than the store's, because fjall's blob reads always come from
disk and compete badly with large sequential writes on the same NVMe.
`--dry-run` parses and counts everything without opening a store, which
is the quickest way to validate a new dataset or to measure the CPU side
alone. The import report prints the process's peak resident memory, split
into heap and mapped file pages.

One environment variable exists for experiments:
`PIPELINE_FJALL_COMPRESSION=none` stores blocks and blobs uncompressed, for
a btrfs volume that compresses by itself.

## Expressions

```
words >= 50 && vowel > 95 && source == "gutenberg"
uniqWords / words < 0.2 || sentences < 2
extra.year < 1850 && has(dictHit) && text.contains("Lorem")
source in ["EEBO", "ECCO"] && extra.title.matches("(?i)^index")
```

Names: `len`, `bytes`, `words`, `uniqWords`, `sentences`, `uniqChars`,
`vowel`, `alpha`, `entropy`, `compress`, `dictHit` (phase 2, `null` for
now), `source`, `id`, `text`, `extra.<key>`. Functions: `abs`, `min`, `max`, `len`, `num`,
`lower`, `has`. String methods: `contains`, `startsWith`, `endsWith`,
`matches`, `lower`. A missing feature is `null`; any comparison with
`null` is `null`, and `null` never matches. Use `has(x)` to test presence.

Expressions that mention `text` or `extra.*` read those keyspaces; all
others scan only the small `meta` keyspace.

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
from its key, so the same options select the same documents on every run
and on both backends. `--top N` keeps the N best by score (word count for
now), best first. `--limit N` stops after N documents in key order.

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

## Looking inside

`info` reads no record: backend, approximate count, disk size, source
labels, the feature list and schema, then per-keyspace facts from the
engine (table and blob file counts, files at level 0, cache size).

`show` prints a document: id, source, every feature, the stored metadata,
then the text after a rule. An id is 32 hex characters, or an even-length
prefix of at least 4 that matches exactly one document; an ambiguous
prefix lists what it matched. `--head N` keeps the first N characters,
`--json` prints the same line `export` would.

`check` is the store's `fsck`. One parallel pass merges the three
keyspaces in key order, with no point reads, and reports metas that fail
to decode, texts and metadata missing or present without a meta
(orphans), metadata present or absent against the `has_extra` flag, and
source ids without a label. `--deep` also reads every text and checks that
it is UTF-8, that its BLAKE3 key matches, and that the stored features
equal a fresh computation, so a changed scanner shows up as "stale
features". Any problem makes the command exit 1 after printing the
counts and example ids. Run it after an interrupted import, after a
delete, or before cutting a training set.

`compact` flushes, rewrites every keyspace into its most compact form and
reports the size before and after; on the 20 GB store it took 6 s.
Nothing else ever compacts implicitly: it is the operator's call when to
pay for it, and the rule is to run it after every import, since a fresh
store serves point reads about 15x slower until then.

## Features

| name | what it is |
|---|---|
| `len`, `bytes` | characters (Unicode scalars) and UTF-8 bytes |
| `words`, `uniqWords` | whitespace-separated tokens after stripping punctuation; distinct ones after lowercasing |
| `sentences` | sentence boundaries, counted by punctuation plus a capital, or a blank line |
| `uniqChars` | distinct characters |
| `vowel` | percent of alphabetic words that contain a vowel |
| `alpha` | percent of words that are purely alphabetic (`[A-Za-z][A-Za-z'’-]*`) |
| `entropy` | order-0 Shannon entropy of the characters, bits per character; English prose sits around 4.1 to 4.6 |
| `compress` | LZ4 block output over raw size, percent; a repeated paragraph lands under 10, noise at 100 or a little above; on the short-docs corpus the median is 89 and the 5th percentile 73, since LZ4 only finds repeats inside the document and short documents have few |
| `dictHit` | phase 2, null until the wordlist lands |

`entropy` and `compress` are complementary, not opposites. Entropy sees
only how evenly characters are used and is blind to repetition: a
paragraph pasted fifty times has ordinary entropy. LZ4 has no entropy
coder, so `compress` reacts only to repeated byte strings. Random-looking
OCR noise is high on both; a looping generator is normal on entropy and
very low on compress; a page of one repeated letter is low on both.

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
| count `words > 1000` | 0.22 s on 16 threads |
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
pipeline explore --db /data/Vintage-DB
pipeline explore --db /data/Vintage-DB -p 9000 -t 8
```

## Counts

`import` prints, per file, lines, skipped, empty, dropped by reason,
duplicates (stored copy kept), replaced (new copy won) and indexed. They
always add up to the line count. `--rejects FILE` writes every dropped line
with its reason and raw text.

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
src/features/      scanners: chars, words, sentences; prefilter
src/expr/          lexer, parser, compiler, evaluator
src/meta.rs        CBOR record + feature registry
src/hash.rs        16-byte BLAKE3 key of normalized text
src/jsonl/         chunked reader (.gz/.zst aware)
src/store/         Store trait, the fjall engine, a null store for dry runs
src/import.rs      reader -> workers -> writer pipeline
src/stats.rs       parallel meta scan, histograms, table
src/count.rs
src/export/        header templates, field emitter, sharded sink, orchestration
src/check.rs       three-keyspace merge audit, optional deep re-hash
src/info.rs        info and compact
src/show.rs        id resolution and the readable document layout
src/delete.rs      selection, backup, batched deletes
src/explore/       the read-only browser: handlers, the tiny_http layer, index.html
bench/             the bake-off scripts and results (records; the engine flags they use are gone)
```
