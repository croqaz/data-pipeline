# Benchmarks

Decision taken on 2026-10-05: fjall is the engine. RocksDB, LMDB and redb
were removed from the code the same day, together with the `--backend`
flag; the sections below are kept as the record of why, and the scripts
under `bench/` that pass `--backend` no longer run as written.

Measured 2026-10-05 on the target machine (Ryzen 7 9700X, 8 cores / 16
threads, 62 GiB RAM, Samsung 990 PRO on btrfs `compress=zstd:3`, Viper
VP4300L on ext4 at `/data`, tmpfs at `/tmp`). Release build with
`target-cpu=native`, fat LTO, mimalloc. Numbers are single runs; repeated
runs of the same case agreed within about 5%.

## Input

`/tmp/bench/short-1g.jsonl`: the first 1,000,000,000 bytes of
`training/data/Vintage-v1/short-docs.jsonl`, 386,677 lines, median text
about 1 KB. The last line is cut mid-record, which the TypeScript importer
silently ignores (no trailing newline) and `pipeline` reports as one
`bad_json` line. Both tools therefore process the same 386,676 records.
Input on tmpfs, so no disk read cost on the input side.

## Baseline: the TypeScript importer

`bun import.ts -i short-1g.jsonl -d /tmp/bench/ts-DB -s bench --vocab vocab2.json`,
default 16 workers, LevelDB on tmpfs.

| | |
|---|---|
| wall time | 10.0 s (15:32:22 to 15:32:32) |
| indexed | 386,659 (17 dropped by its quality filter) |
| store size | 772 MB |

It computes more per record than version 1 of `pipeline` (entropy, deflate
ratio, dictionary hits), so this is a baseline, not an equal-work
comparison.

## The bake-off: `bench/run.sh`

Same slice, both backends, four filesystem configurations. `stats` is the
full feature table, `count meta` is `words > 200 && vowel > 95` (reads only
the `meta` keyspace), `count text` is `text.contains("London")` (reads every
text). fjall persist now flushes memtables, which is why its scan times
below are better than the first run recorded in `bench/results-2026-10-05.tsv`.

| case | import | MB/s | disk | stats | count meta | count text |
|---|---|---|---|---|---|---|
| tmpfs, fjall | 2.7 s | 365 | 888 MB | 0.21 s | 0.21 s | 0.91 s |
| tmpfs, rocksdb | 5.9 s | 170 | 518 MB | 0.03 s | 0.02 s | 0.23 s |
| ext4, fjall | 4.4 s | 228 | 888 MB | 0.74 s* | 0.21 s | 0.69 s |
| ext4, rocksdb | 6.1 s | 163 | 518 MB | 0.05 s | 0.05 s | 0.25 s |
| btrfs CoW, fjall | 4.3 s | 230 | 888 MB | 0.74 s* | 0.20 s | 0.69 s |
| btrfs CoW, rocksdb | 6.1 s | 164 | 518 MB | 0.05 s | 0.05 s | 0.26 s |
| btrfs nocow, fjall | 4.1 s | 246 | 888 MB | 0.74 s* | 0.21 s | 0.68 s |
| btrfs nocow, rocksdb | 6.1 s | 164 | 518 MB | 0.05 s | 0.05 s | 0.26 s |

`*` measured before persist flushed memtables; on tmpfs the same scan took
0.21 s after the fix and the disk-backed cases will match it.

Both tools and both backends agree on the document count (386,675 indexed,
1 duplicate inside the slice).

## Where the time goes

`import --dry-run` parses, computes features and hashes, and writes nothing:

| workers | dry run | fjall (tmpfs) | rocksdb (tmpfs) |
|---|---|---|---|
| 16 | 1.2 s, 829 MB/s, 320k docs/s | 2.9 s | 5.9 s |
| 8 | 1.8 s, 570 MB/s | 2.8 s | 5.9 s |
| 4 | 3.4 s, 290 MB/s | | |

The feature side scales with threads and is not the limit. With a store,
the import time stops depending on the worker count, so the single writer
thread and the engine behind it set the pace: about 2.8 s for fjall, 5.9 s
for RocksDB. Turning RocksDB's write-ahead log off (`PIPELINE_ROCKS_WAL=off`)
changed nothing (5.8 s), so the log is not the cost. The remaining suspect
is zstd blob compression in RocksDB's flush thread; untested.

Scan scaling, `stats` on tmpfs:

| threads | fjall | rocksdb |
|---|---|---|
| 1 | 0.33 s | 0.11 s |
| 16 | 0.21 s | 0.03 s |

fjall gains 1.6x from 16 threads, RocksDB 3.7x. Something in fjall's
iterator path serializes; untested.

## Disk and btrfs compression

Apparent size from `du`, real use from the change in free space on `/home`.

| store | apparent | used on btrfs |
|---|---|---|
| fjall, LZ4 (default) | 808 MB | 717 MB |
| fjall, no compression, btrfs zstd:3 does it (`PIPELINE_FJALL_COMPRESSION=none`) | 991 MB | 529 MB |
| rocksdb, zstd blobs | 518 MB | not measured on btrfs; 518 MB on tmpfs |

btrfs zstd:3 on uncompressed fjall data lands at the same size as RocksDB's
own zstd. LZ4 output barely compresses further, so stacking the two wastes
200 MB per GB of input. With `chattr +C` (nocow) there is no btrfs
compression, so the LZ4 or zstd choice inside the engine is the only one.

Projected for the whole current corpus (about 200 GB of text): fjall LZ4
about 160 GB, RocksDB zstd or fjall-none-on-btrfs about 105 GB.

## Against the TypeScript tool

| | TS (LevelDB, tmpfs) | pipeline, fjall, tmpfs | pipeline, rocksdb, tmpfs |
|---|---|---|---|
| import 1 GB | 10.0 s | 2.7 s | 5.9 s |
| store | 772 MB | 888 MB (LZ4) | 518 MB (zstd) |
| stats | reads every record with text | 0.21 s | 0.03 s |

Version 1 computes fewer features than the TS tool, so part of the 3.7x on
fjall is work not done yet. Repetition and dictionary features will cost
more CPU; the dry run shows there is headroom (1.2 s of the 2.7 s).

## Export: `bench/export-bench.sh`

Same two tmpfs stores (386,675 documents), release binary, output on tmpfs,
16 threads unless noted. Wall time of the whole command, in seconds.

| case | fjall | rocksdb | output |
|---|---|---|---|
| all fields, 1 thread | 16.42 | 3.02 | 958.4 MB |
| all fields, 4 threads | 3.94 | 0.74 | 958.4 MB |
| all fields, 16 threads | 2.04 | 0.34 | 958.4 MB |
| `-f text` | 1.95 | 0.32 | 893.7 MB |
| `-f text,source --header` (3 placeholder lines) | 2.02 | 0.32 | 917.6 MB |
| `-f id,source,features` (no text reads) | 1.31 | 0.08 | 65.4 MB |
| `--zstd` (level 3) | 2.51 | 0.87 | 428.2 MB |
| `--zstd-level 9` | 5.20 | 3.67 | 383.0 MB |
| `--verify` | 2.06 | 0.42 | 958.4 MB |
| `--shard 100000` (4 files) | 2.01 | 0.32 | 958.4 MB |
| `--sample 0.1` (38,785 docs) | 0.40 | 0.08 | 95.6 MB |
| `--top 50000` | 0.54 | 0.20 | 503.5 MB |
| `words > 200 && vowel > 90` (165,340 docs) | 0.99 | 0.25 | 763.6 MB |

TypeScript `export.ts` on its LevelDB store, all fields, 386,659
documents: 6.75 s, 971.0 MB.

What the table says:

- The key scan is never the cost: 0.01 to 0.04 s on both engines, even
  for `--top`, which sorts every match. Export time is point reads plus
  JSON serialization.
- RocksDB point reads are much faster than fjall's. The meta-only case
  reads 386,675 small values and nothing else: 1.10 s of fetch time on
  fjall against 0.06 s on RocksDB, 18x. The fjall `meta` keyspace holds
  only four table files, so this is not a pile of unmerged segments; the
  suspects are the 32 MiB default block cache (the text blobs evict the
  meta blocks), LZ4 decompression of every data block touched, and
  per-read overhead in fjall 3's `get` path.
- Both engines scale about 8x from 1 to 16 fetch threads, so the parallel
  fetch with an ordered writer is doing its job; single-threaded fjall
  would take 16 s per GB.
- A header template costs nothing measurable (2.02 s against 1.95 s for
  the same fields without one).
- zstd level 3 halves the output (958 to 428 MB) for 0.5 s on both
  engines; on RocksDB that makes compression the bottleneck. Level 9 saves
  another 45 MB for 3 s more and is not worth it for a training set.
- `--verify` re-parses 958 MB in about 0.1 s on 16 threads; always use it.
- Against the TypeScript exporter writing the same bytes: 3.3x faster on
  fjall, 20x on RocksDB.

## check, info, show, compact: `bench/admin-bench.sh`

Same stores, release binary, wall time of the whole command (which
includes opening the store: 0.25 s on fjall, 0.01 s on RocksDB).

| command | fjall | rocksdb |
|---|---|---|
| `info` | 0.26 s | 0.01 s |
| `check` (structure, 16 threads) | 0.32 s | 0.23 s |
| `check` (structure, 1 thread) | 0.61 s | 1.71 s |
| `check --deep` (every text re-hashed and re-featured, 16 threads) | 2.55 s | 1.30 s |
| `show ID --head 80` | 0.37 s | 0.01 s |
| `compact` (on a copy) | 0.8 s, 794.6 to 771.9 MB | 13.2 s, 521.0 to 508.3 MB |
| `export` after `compact`, all fields | 0.63 s | 0.31 s |
| `check` after `compact` | 0.24 s | 0.17 s |

What the table says:

- Compaction is the missing piece of the fjall export story. Before it,
  `info` showed the meta keyspace with a table at level 0 and the text
  keyspace with three; every point read probed them in turn. After it (one
  table each, none at level 0) the all-fields export dropped from 2.04 s
  to 0.63 s, within 2x of RocksDB instead of 6x. An import should end with
  a compaction, or `import` should trigger one; that is the next change.
- RocksDB's manual compaction is slow because the blob garbage-collection
  cutoff is raised to 1.0 for the call, so every text blob is rewritten
  and recompressed with zstd. That is the point of the command after a
  `delete`, and it is a rare operation, but 13 s per 0.5 GB means about 15
  minutes for the full corpus.
- A structural `check` reads only keys and small values: 0.05 s of scan on
  fjall for 386,675 documents. The deep check costs about what the import's
  CPU phase costs, since it recomputes every feature.
- `info` on the fjall store reported the `extra` keyspace at 7.1 MB with
  "about 386,675 keys" although no document has metadata: `put_batch`
  wrote a tombstone into `extra` for every document without it, and the
  RocksDB `extra` family carried 10.4 MB of the same. Fixed the same day:
  a `Record` now carries `replace`, set only when the import writer
  overwrites a stored document, and only then is a missing `extra`
  deleted. Re-imported (`bench/reimport-bench.sh`): `extra` is 0 keys and
  0.0 MB on both engines, import time unchanged (fjall 2.6 s, RocksDB
  5.8 s), stores 750 MB and 486 MB.
- fjall's approximate key count overshoots by 7% on this store (413,350
  reported for 386,675 documents), compacted or not. `count` and `check`
  are exact; `info` says "about" for a reason.
- A fresh fjall import still exports in 2.03 s until it is compacted: the
  level-0 table is there from the start. `compact` stays a separate
  command on purpose; the user's LevelDB stores took an hour to compact at
  50 GB, so nothing runs it implicitly until the chosen engine proves
  cheap at scale.

## Four engines, two more features: `bench/smoke4.sh`, `bench/rss4.sh`

LMDB (heed 0.22) and redb (4.3) joined fjall and RocksDB, and `entropy`
and `compress` joined the features. Same 1 GB slice, 16 threads, release
binary. This is a smoke run to prove the four work end to end; the real
bake-off comes after.

| | fjall | rocksdb | lmdb | redb |
|---|---|---|---|---|
| import, tmpfs | 2.6 s | 5.8 s | 2.0 s | 3.9 s |
| import, ext4 (`/data`) | 3.7 s | 6.6 s | 3.0 s | 4.7 s |
| store on disk | 750 MB | 486 MB | 1,303 MB | 2,156 MB, 1,486 after `compact` |
| `check` | 0.20 s | 0.18 s | 0.03 s | 0.18 s |
| `check --deep` | 2.6 s | 1.3 s | 1.3 s | 1.3 s |
| `export`, all fields | 2.0 s fresh, 0.6 s compacted | 0.31 s | 0.22 s | 0.49 s |
| `compact` | 0.8 s | 13.2 s | 0.3 s (copy) | 0.9 s |
| peak RSS during import, ext4 | 788 MB | 791 MB | 1,603 MB | 1,294 MB |
| of which heap at the end | 287 MB | 396 MB | 65 MB | 1,152 MB |
| of which mapped file pages | 15 MB | 17 MB | 1,145 MB | 15 MB |

What the table says:

- LMDB is the fastest importer and the fastest reader of the four, by a
  margin: a 1 GB export in 0.22 s, a structural check in 0.03 s. It pays
  with disk: no compression and 4 KiB overflow pages for every text over
  about 2 KiB, so the store is 1.45x the raw JSONL. On btrfs with
  compression the file would shrink; on `/data` (ext4) it would not.
- The memory question. LMDB's peak resident size is 1.6 GB for a 1 GB
  import, but the split tells the story: 65 MB of heap and 1.1 GB of
  mapped file pages. The heap is the process's own memory; the mapped
  pages are page cache the kernel evicts under pressure, the same bytes
  any program reading the file would show. heed does not enable LMDB's
  write-map mode, so dirty pages never accumulate in the mapping; writes
  go through ordinary file writes and the kernel's dirty-page throttling.
  That is the opposite of the setup that used to kill imports. The 34 GB
  run will show it at scale: the resident size will read as tens of GB
  and the heap will stay flat.
- redb's memory is real: its 1 GiB page cache is heap
  (`PIPELINE_REDB_CACHE_MB` sets it). Its file is 2.2x the raw data after
  an import and still 1.5x after `compact`, with 528 MB reported as
  fragmented in the text table either way. Reads are between RocksDB and
  LMDB; the import is between fjall and RocksDB.
- fjall and RocksDB sit at about 300 to 400 MB of heap, most of it the
  import pipeline's buffers plus their write buffers.
- `entropy` and `compress` cost about 0.1 s per GB on 16 threads (the
  dry run went from 1.2 to 1.3 s). On this corpus entropy runs from 3.3
  to 5.5 bits per character (median 4.66) and compress from 26% to 102%
  (median 89%): short OCR newspaper documents have few internal repeats,
  so LZ4 gains little on most of them, and the lower tail is where the
  repetitive documents live.

## The 20 GB bake-off on `/data`: `bench/bench20.sh`

The run the engine choice rests on. Input: all of `gutenberg.jsonl`
(6.57 GB), then 10.07 GB of `short-docs.jsonl` and 3.36 GB of
`long-docs.jsonl` (75/25 of the rest), cut at line boundaries and
assembled once on `/data`: 19,996,844,182 bytes, 4,137,630 lines, of which
4,137,434 indexed, 93 replaced, 103 duplicates, 0 dropped, the same on all
four engines. Stores on `/data` (ext4), 16 threads, release binary. Each
engine ran the same sequence: import (memory sampled every second), size,
`stats`, `check --deep`, `export` with all fields, `compact`, size again,
`export` again. The output of an export is 20 GB written back to `/data`
and deleted afterwards.

| | fjall | rocksdb | lmdb | redb |
|---|---|---|---|---|
| import | 62 s, 323 MB/s | 109 s, 184 MB/s | 305 s, 65 MB/s | 231 s, 87 MB/s |
| peak resident size | 3.8 GB | 4.0 GB | 13.9 GB | 3.8 GB |
| of which heap, maximum | 3.5 GB | 3.6 GB | 1.7 GB | 3.6 GB |
| of which mapped file pages, maximum | 14 MB | 15 MB | 11.2 GB | 14 MB |
| available RAM, minimum (of 62 GB) | 36.6 GB | 36.5 GB | 40.5 GB | 39.6 GB |
| store, no compact (share of the 20.0 GB input) | 13.77 GB, 69% | 9.81 GB, 49% | 22.99 GB, 115% | 29.19 GB, 146% |
| `stats` | 0.2 s | 0.2 s | 0.4 s | 0.1 s |
| `check --deep` | 27 s | 31 s | 27 s | 26 s |
| `export`, right after import | 123 s | 7.5 s | 4.7 s | 30 s |
| `compact` | 6 s | 288 s | 39 s (copy) | 18 s |
| store after compact | 13.68 GB | 9.04 GB | 22.93 GB | 29.19 GB |
| `export`, right after compact (see note) | 8.2 s | 100 s | 42 s | 9.0 s |

Note on the last row: a compaction that rewrites files (RocksDB rewrites
every blob, LMDB copies the whole file) leaves the new files out of the
page cache, so the next export reads the store from disk while writing
20 GB of output to the same drive. RocksDB's 100 s is 15.8 s of cold
reads plus the mixed-load penalty measured in the third pass below; with
the store warm and the drive free the same export takes 4.4 s. fjall's
8.2 s is the opposite case: its compaction output was still in the cache.

A second pass (`bench/bench20-post.sh`) evicted every store's files from
the page cache (`posix_fadvise`, no root needed) before each measurement:

| | fjall | rocksdb | lmdb | redb |
|---|---|---|---|---|
| `export`, cold cache | 62 s | 79 s | 45 s | 92 s |
| `export` again, straight after | 65 s | 84 s | 41 s | 68 s |
| `check --deep`, cold cache | 28 s | 35 s | 27 s | 54 s |

What the tables say:

- **Import.** fjall at 323 MB/s is the engine to beat; RocksDB takes 1.8x
  as long, redb 3.7x, LMDB 4.9x. LMDB was the fastest importer at 1 GB and
  is the slowest at 20 GB: random inserts into a copy-on-write B+tree get
  dearer as the tree outgrows the cache, and the full corpus is three
  times bigger again. The LSM engines write sequentially and do not care.
- **Memory.** The user's old LMDB failure does not reproduce. LMDB used the
  least real memory of the four (1.7 GB of heap at most) and left the
  most RAM available (40.5 GB); its 11 GB of resident pages are the
  mapped file, which the kernel reclaims at will, and dirty pages never
  exceeded 255 MB because heed does not use write-map mode. The 3.5 GB
  heap peak shared by the other three engines is not theirs: the samples
  show 1.1 to 2.1 GB through gutenberg and short-docs on every engine and
  a jump to about 3 GB when the long-docs section arrives. The import
  pipeline bounds its channels and batches by count (2,000 records per
  batch, `workers * 2` chunks in flight), and 2,000 long documents are a
  gigabyte. Byte-based bounds are the fix, engine-independent, and the
  next change.
- **Disk.** RocksDB's zstd blobs make the store 49% of the input, fjall's
  LZ4 69%. LMDB stores raw text in 4 KiB overflow pages, 115%; redb 146%
  with nothing to reclaim. Compaction changes little on any of them here
  (fjall 13.77 to 13.68 GB) because a store that has only been imported
  into holds no dead data; it matters after deletes and replacements. On a btrfs volume with compression the two B-trees
  would shrink; on `/data` they do not. For the whole corpus that is
  roughly 30 GB (RocksDB), 41 GB (fjall), 69 GB (LMDB) or 88 GB (redb).
- **Compact.** fjall rewrites 13.6 GB in 6 s; redb's 18 s and LMDB's 39 s
  copy reclaim nothing on a store that has only been imported into;
  RocksDB's 288 s is the price of rewriting every zstd blob (0.8 GB
  saved). At 60 GB that is about half an hour on RocksDB and 30 s on
  fjall. The command stays separate; on fjall it is cheap enough to run
  after every import.
- **Sequential reads** (`check --deep`, which also recomputes every
  feature): 26 to 31 s for fjall, RocksDB and LMDB whether the cache is
  cold or warm, so the CPU is the limit and the engines' iterators keep
  up. redb is twice as slow cold.
- **Point reads** (`export`) are where the engines differ, and the page
  cache decides most of it. With the store in RAM: LMDB 4.7 s, RocksDB
  7.5 s, fjall 8.2 s (after compaction), redb 9.0 s. The fjall export
  straight after import (123 s) is the level-0 effect, cured by the 6 s
  compaction. The slow exports after compaction on RocksDB (100 s) and
  LMDB (42 s) are the first read of freshly rewritten files that had left
  the cache. With the cache evicted on purpose: LMDB 45 s, fjall 62 s,
  RocksDB 79 s, redb 92 s. A second export straight after the cold one is
  not faster on any engine. The `/dev/null` pass below separates the
  effects.

The third pass (`bench/bench20-cache.sh`) exported to `/dev/null` twice per
engine, cold then warm, with the NVMe's sectors-read counter around each
run:

| | fjall | rocksdb | lmdb | redb |
|---|---|---|---|---|
| `export` to /dev/null, cold | 8.7 s, 13.8 GB read | 15.8 s, 9.1 GB | 14.9 s, 22.9 GB | 54.9 s, 36.4 GB |
| again, warm | 7.4 s, 8.6 GB read | 4.4 s, 0.2 GB | 2.5 s, 0.0 GB | 34.5 s, 23.0 GB |

- Without output writes every engine but redb exports 20 GB in 9 to 16 s
  from a cold cache; the NVMe serves the random reads at 1 to 1.6 GB/s. The
  60 to 90 s figures of the file-writing passes are the same reads
  competing with 20 GB of sequential writes on the same drive: writes
  alone take about 8 s (the drive sustains 3.7 GB/s, measured with `dd`
  after the run), reads alone 9 to 16 s, the two together 60 to 90 s.
  That is a property of consumer NVMe under mixed load, not of any engine,
  and the remedy is to write exports to the other drive.
- Warm, RocksDB and LMDB read nothing from disk; fjall still reads 8.6 GB,
  so its blob reads do not populate the page cache (or it drops them). Its
  point reads come from disk every time, which is fine while the drive is
  free (8.7 s) and is the reason its file-writing export did not improve
  on a second run. The fast 8.2 s after compaction was the compaction
  output still sitting in the cache.
- redb reads more than its own file size on a cold export and 23 GB even
  warm; its reads are the weakest of the four by a wide margin.

## Verdicts, so the same ground is not covered twice

**LMDB (heed): rejected, 2026-10-05.** Worth checking because of the
memory failures of the TypeScript era, and the memory part came out
clean: no write-map mode in heed, 1.7 GB of heap at most on a 20 GB
import, 40.5 GB of RAM still available, dirty pages under 255 MB; the
14 GB resident figure is the mapped file. It is also the fastest reader
(20 GB export in 14.9 s cold, 2.5 s warm; `check --deep` 27 s). It is
rejected on writes and on disk: the import is a copy-on-write B+tree
taking random 16-byte keys, 2.0 s at 1 GB (fastest of all) but 305 s at
20 GB (slowest of all, 65 MB/s, 4.9x fjall), and the trend only worsens as
the tree outgrows the cache, so the 60 GB corpus would take well over
15 minutes per import and every re-import after a scanner change. The
file is uncompressed, 115% of the input (about 69 GB for the corpus), it
never shrinks in place, and compaction is a full copy. Do not revisit
unless the workload becomes read-only.

**redb: rejected, 2026-10-05.** Pure Rust and simple to embed, but the
weakest on every axis that matters here: import 231 s (3.7x fjall), file
146% of the input with 528 MB reported fragmented that `compact` never
reclaims (29.19 GB before and after), and reads that touch more bytes
than the file holds (36 GB read for a cold 20 GB export, 23 GB even
warm; 54.9 s cold to `/dev/null`, 92 s to a file). Its 1 GiB page cache is
real heap. Nothing about it is likely to change by tuning.

**RocksDB: fallback.** Smallest store (49% of the input), reads on par
with fjall when warm, but 1.8x slower to import, a 288 s compaction per
10 GB because the blob garbage collector rewrites every zstd blob, and
the slowest random reads under mixed load of the three survivors. Minutes
to compile. Keep it in mind only if disk size becomes the constraint.

**fjall: chosen.** See the section above. The two operating rules: run
`compact` after every import, and write exports to a drive other than
the store's.

## What this says about the store decision

- fjall: faster import, pure Rust (21 s release build), slower scans, bigger
  on disk unless btrfs compresses underneath it.
- RocksDB: 2x slower import for reasons not yet pinned down, 7x faster
  scans, zstd built in, minutes to compile.

The 20 GB run settles it in favour of **fjall**:

- It imports fastest by a wide margin (323 MB/s; RocksDB 1.8x slower,
  redb 3.7x, LMDB 4.9x, and LMDB's gap grows with store size).
- Its store is 69% of the input, second only to RocksDB's 49%; for the
  whole corpus that is about 41 GB against 30 GB.
- Compacted, it reads as fast as RocksDB and faster than redb: 8.7 s for a
  20 GB export with the drive free, 62 s under mixed load where RocksDB
  takes 79 s. Compaction costs 6 s per 13 GB against RocksDB's 288 s.
- It is pure Rust, builds in 20 s, and its memory use is the pipeline's,
  not its own.

Two rules come with it: run `compact` after every import (a fresh fjall
store exports 15x slower until then), and write exports to the other
NVMe, because fjall's blob reads always come from disk and compete badly
with large writes on the same drive. LMDB keeps the honours for read
speed and for the lowest real memory of the four, which answers the old
worry, but a B+tree import that is already 5x slower at 20 GB has no
future at 60 GB and the store would be 69 GB uncompressed. redb is out.
RocksDB stays the fallback if disk size ever matters more than import
time. Earlier, the running notes:

Export tilted the balance toward RocksDB (6x faster point reads), then
`compact` took most of it back: on a compacted fjall store the export gap
is 2x, and fjall wins import, structural check, build time and binary
size. LMDB then beat both on every speed number in the smoke run and lost
on disk size and on the shape of its memory use. The experiments that were
open before the 20 GB run:

1. RocksDB flush: try LZ4 blob compression with zstd only at the bottom
   level, more write buffers, and `max_background_jobs` higher, to see if
   the import catches fjall.
2. fjall scans: profile the 16-thread scan to find the serialized part;
   check whether `data_block_size_policy` or cache settings move it.
3. The 34 GB `short-docs.jsonl` end to end on both, on `/data`, then
   `stats`, a text scan and a full export. The scale run decides.
4. fjall point reads on a compacted store: raise the block cache, turn
   block compression off, and try range reads instead of `get` for dense
   selections, to see whether the remaining 2x export gap is configuration.
5. Time `compact` on the 34 GB store for every engine. The decision to
   keep it a separate command stands until that number is known.
6. The 34 GB run on all four, on `/data`, with the heap and mapped-file
   lines of the import report, `check --deep`, a full export and `stats`.
   Done as the 20 GB run above; it picked fjall.
7. Byte-based bounds for the import pipeline's batches and channels, so a
   run of long documents cannot hold gigabytes in flight.

## explore on the 20 GB store

Release build, the compacted fjall store from the bake-off
(`DATA/bench20/fjall`, 4,137,434 documents, 13.5 GB), one request at a
time through curl, 2026-10-05 evening, page cache warm from the earlier
runs except where noted.

| request | time | notes |
|---|---|---|
| `/api/info` | 1 ms | no record read |
| `/api/random`, first call | 49 ms | cold index block |
| `/api/random`, again | 1.4 ms | |
| `/api/random?expr=text.contains("Nevermore")` | 110 ms | 4,147 documents looked at, each text read |
| `/api/random?expr=source == "gutenberg" && words > 50000` | 4 ms, 404 | no such label in this store; 10,000 metas decoded and given up |
| `/api/next/{id}?n=10`, `/api/prev/{id}` | 0.3 ms | |
| `/api/query?expr=uniqWords / words < 0.1&limit=5` | 12 ms | 10,056 scanned |
| `/api/count?expr=words > 1000` | 0.22 s | 4.1 M metas on 16 threads |
| `/api/count?expr=text.contains("Nevermore")` | 12.3 s | every text read, 161 matches |

The last line is the one to remember. A text filter in Count reads the
whole store, 13 GB here, so on the 50 GB corpus it will take about a
minute. Everything else answers before the eye notices.

## recompute on the 20 GB store

Release build, the same compacted fjall store (`DATA/bench20/fjall`,
4,137,434 documents, 13.5 GB on disk, 18.4 GB of raw text), 16 threads,
2026-10-05 evening. The store already carried every feature, so the
plain run found nothing to write; `--force` rewrote every record with
the same values to time the write path.

| step | time | notes |
|---|---|---|
| `recompute --dry-run` | 21.2 s | every text read and scanned, 869 MB/s of text, nothing written |
| `recompute --force` | 21.3 s | 4,137,434 meta records rewritten; the writes cost nothing visible |
| `compact` after it | 11.6 s | 13,564 MB to 13,526 MB; the meta keyspace had grown from 238 MB to 265 MB |
| `check` after that | 0.28 s | OK, structure only |

Reading and scanning dominate: 18.4 GB of text through the feature
scanners in 21 s is 869 MB/s, close to the import's CPU side without the
JSON parsing. Rewriting 4.1 M small records on top of that is lost in the
noise, and the compaction that merges them is seconds. On the 50 GB
corpus a full recompute is therefore about a minute, plus a minute to
compact.

One thing to know: right after a rewrite fjall's document estimate
counts old and new versions until compaction merges them (6.08 M for
this 4.14 M store), so the recompute report prints the exact count it
walked and the disk size, not the estimate.

## Artifacts

- `bench/results-2026-10-05.tsv`: the raw import matrix (first run, before
  the persist fix).
- `bench/export-results-2026-10-05.tsv`: the export matrix above, with the
  scan and write phase times from each report.
- `bench/admin-results-2026-10-05.tsv` and `/tmp/bench/admin/*.log`: the
  check/info/show/compact runs; `/tmp/bench/db/{fjall,rocks}-compact` are
  the compacted copies (1.3 GB of tmpfs), deletable.
- `bench/reimport-results-2026-10-05.log`: the tombstone-free re-import;
  `/tmp/bench/db/{fjall,rocks}-v2` are those stores (1.2 GB of tmpfs),
  deletable.
- `bench/bench20-2026-10-05.log`, `bench/bench20-results-2026-10-05.tsv`,
  `bench/bench20-coldwarm-2026-10-05.tsv`, `bench/bench20-cache-2026-10-05.tsv`:
  the 20 GB bake-off; per-second memory samples in
  `DATA/bench20/results/*-mem.csv`. The input (20 GB) and the four stores
  (14, 9, 23 and 29 GB) are in `DATA/bench20/`, deletable.
- `bench/four-engines-2026-10-05.log`: the smoke and memory runs above;
  `/tmp/bench/db/{lmdb,redb}-smoke` and `/tmp/bench/db/rss-lmdb` (4 GB of
  tmpfs) and `DATA/smoke-*`, `DATA/rss-*` (6 GB on `/data`) are the
  stores, all deletable.
- `/tmp/bench/export/`: the export logs, the TypeScript output and one full
  export (`fjall-default16.jsonl`, 958 MB of tmpfs), all deletable.
- Stores left behind for inspection, all deletable: `/tmp/bench/*`,
  `DATA/bench/*` (about 3 GB on `/data`). The stores under `bench/db/`
  in this repository were deleted on the evening of 2026-10-05; the
  scripts and result files in `bench/` stay.

## Cost of the n-gram features

Release build, 16 threads, 2026-10-05 night. Inputs: the user's Vintage-1
store (1,141,724 documents, 7.9 GB on disk, median 221 words, largest
1.75 M words) and `/tmp/v1-sample.jsonl`, a 30% `export --sample` of it
(342,997 documents, 4.1 GB). Imports went to a fresh store on tmpfs.

`recompute --dry-run` on Vintage-1 only reads and computes, so it shows the
CPU cost undiluted:

| build | time | rate |
|---|---|---|
| before the n-grams | 18.1 s | 748 MB/s |
| first version: foldhash per window, full tables for every size | 75.7 s | 179 MB/s |
| rolling window hash, 5- and 10-grams only where 4-grams repeat | 30.1 s | 450 MB/s |
| plus 4-grams only where 2-grams repeat, one table operation per window | 26.5 s | 511 MB/s |
| n-gram call switched off in that build, for reference | 15.9 s | 850 MB/s |

What is left is the full 2-gram table, about 70 ns per word over 2.3 B
words. A book of a million words makes a table that does not fit in cache,
so that cost is mostly memory latency.

`import` of the sample, the same build with the n-gram call switched on and
off:

| n-grams | elapsed | rate | peak RSS |
|---|---|---|---|
| off | 11.9 s | 348 MB/s | 2.48 GB |
| on, buffers kept | 12.6 s | 328 MB/s | 3.10 GB |
| on, buffers freed after documents over 65,536 words | 13.0 to 13.3 s | 310 to 317 MB/s | 2.27 to 2.42 GB |

The import loses 6 to 10%, because its single writer thread was the limit
and the workers had slack. Each worker kept the tables of the largest book
it had seen, so peak memory rose by 600 MB until the scanner started
freeing them after big documents.
