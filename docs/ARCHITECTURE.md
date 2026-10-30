# Architecture

## From text to posting lists

```
 "The Quick Foxes are Running!"
          │
          ▼
 ┌──────────────────┐  split on non-word chars, keep "don't", keep Arabic marks
 │ 1. Tokenizer     │  → The | Quick | Foxes | are | Running      (positions 0..4)
 └────────┬─────────┘
          ▼
 ┌──────────────────┐  lowercase, drop possessive/apostrophes, Arabic folding
 │ 2. Normalizer    │  → the | quick | foxes | are | running
 └────────┬─────────┘
          ▼
 ┌──────────────────┐  positions were assigned BEFORE this step, so gaps survive
 │ 3. Stopwords     │  → quick@1 | foxes@2 | running@4
 └────────┬─────────┘
          ▼
 ┌──────────────────┐
 │ 4. Porter stem   │  → quick@1 | fox@2 | run@4
 └────────┬─────────┘
          ▼
 ┌──────────────────┐  term → (doc, freq, positions[])   grouped by sorting token indices
 │ 5. SegmentBuilder│  RAM buffer; flushed at N docs / M bytes or on commit()
 └────────┬─────────┘
          ▼
 ┌──────────────────┐  sort terms, encode each posting list (below), add footer + CRC32
 │ 6. SegmentWriter │  → seg_00000007.hiis   (write tmp, fsync, rename)
 └──────────────────┘
```

## Posting list encoding

```
n_docs | n_blocks | skip table [(last_doc, block_len)]* | block*

block (128 docs):  df_len | FoR(doc deltas) FoR(freqs) | positions (vbyte gaps per doc)
tail block (<128): df_len | vbyte(doc deltas) vbyte(freqs) | positions
```

* **Doc ids** are segment-local and delta-coded; **FoR** packs 128 deltas at
  `bits(max-min)` bits each. A block of all-ones frequencies costs 2 bytes.
* **Skip table** lets `advance(target)` jump whole blocks, which is what makes
  phrase/AND queries cost proportional to the *rarest* term.
* **Positions** sit behind the doc/freq section; ranking-only queries never decode them.

## Segment file

```
header 8B | postings | term dictionary (sorted) | stored text (LZ blocks) | doc table + block table | footer (counts, offsets, CRC32, magic)
```

Opening a segment verifies magic, version, section bounds and the CRC of the
whole file; a flipped byte anywhere is reported as `Corrupt`, never as a wrong
answer. (Tested exhaustively on a small segment.)

## Lifecycle

```
add_document ─► RAM buffer ─ flush ─► segment ─┐
                                               ├─► MANIFEST (atomic rename) ─► searchable
 background merger ◄── wake ───────────────────┘
   │ plan (tiered policy) → stream-merge off-lock → swap under write lock → delete old files
```

* **Readers** clone an `Arc<Segment>` snapshot and run lock-free; a merge or
  flush never blocks or invalidates a running query.
* **Merging** streams term dictionaries in lock step, drops tombstoned docs,
  renumbers survivors, and re-encodes. It never re-analyses text.
* **Deletes** are tombstone bits per segment (persisted on `commit`). Deletes
  that arrive *during* a merge are reconciled onto the merged segment under
  the same write lock that swaps it in, so none are lost.
* **Write-ahead log** (`wal.log`): every `add_document` appends a CRC-framed record before it enters the RAM buffer; a flush empties the log only after the segment and MANIFEST are durable. On open the log is replayed (records below the manifest's `flushed_upto` are skipped, a torn tail is cut), so uncommitted documents survive a crash. `wal_fsync` trades throughput for power-loss safety.
* **Directory lock**: `LOCK` holds an OS advisory lock for the engine's lifetime (released by the OS on crash); a second `Engine::open` on the same directory fails with `Error::Locked`.
* **Recovery**: the MANIFEST is the single source of truth; leftover `.tmp`
  and unreferenced segment files from a crash are removed on open.

## Ranking

Okapi BM25 (`k1=1.2`, `b=0.75`), Lucene-style non-negative IDF. IDF and average
document length are computed over the whole snapshot, so scores don't depend
on segment boundaries (a test checks results are identical before/after a merge).
Phrase clauses score with the phrase frequency as `tf` and the sum of the terms' IDFs.


## Top-k pruning (MaxScore)

Plain multi-term queries (`a b c`) skip documents that cannot reach the current k-th score. Each skip-table entry stores the block's `max_tf`, which gives a score upper bound (BM25 rises with tf and falls with length, so length 0 is the ceiling). Terms are sorted by their bound; the cheap prefix whose bounds add up to less than the k-th score cannot produce a top-k doc by itself, so only the remaining terms drive the iteration and the prefix is probed with `advance` for the docs they find (a doc is dropped early once its partial score plus the remaining bounds falls below the threshold). A single term skips whole 128-doc blocks using the block bound. Pruning only drops docs whose bound is strictly below the heap minimum, and survivors are summed in clause order, so results are **bit-identical** to exhaustive scoring (a randomized test compares 180 query/k combinations across merged segments with deletes). MUST/NOT/phrase queries use the exhaustive path.

## Query evaluation extras

Every clause kind is evaluated into a doc-sorted `(doc, score)` list per segment, so new kinds slot
in without touching the combine step: prefixes and fuzzy terms expand against the dictionary and
union their variants (fuzzy weights a variant by `1 - edits / (len + 1)`), `"a b"~n` runs the phrase
leapfrog and then slides a window over the positions (each position shifted back by its offset in
the phrase, so an exact phrase is a window of width 0 and a swap costs 2), groups recurse into the
same function. Boosts multiply a clause's list; MaxScore multiplies its per-term bounds the same way,
so pruned and exhaustive results stay bit-identical. Pagination passes the previous last hit down
as an exclusive bound that `offer` enforces. `explain` re-runs each clause against one document with
the same statistics, so its total is the score search reports.

## Fields

A fielded document is stored as ` name  value` records and indexed twice over: every token under
its plain term (so unfielded queries see all the text, exactly as before) and under `name:term`. The
parser rewrites `title:x` clauses to those terms. Plain prefix / fuzzy expansion skips terms with a colon
and field queries only look at them. Positions restart per field with a gap of 50, so phrases and
proximity don't cross fields. BM25 uses whole-document length for field terms too, and the length
counted in the segment is the plain-token one, which keeps `verify` consistent. `Segment::text` hands
readers the values joined by newlines; merges copy the raw bytes.

## Known limits (deliberate scope)

* Segment files up to `Config::resident_limit` (512 MB) are held in memory; bigger
  ones keep only the term dictionary and doc table resident and read postings and
  stored text with positional reads (no mmap, since the crate is `unsafe`-free and
  dependency-free). That mode costs query latency, roughly 1.5-2x on TREC-COVID.
* Stored text is compressed in 8 KiB blocks with a small hand-written LZ77 (`codec/lz.rs`, about 60 MB/s to compress, 170 MB/s to decompress, ratio near 1.7 on abstracts): a `get` decompresses the blocks its text touches. A better codec would shrink the files and speed up merges.
* Doc length is exact (not Lucene's 1-byte lossy norm); document frequencies
  include tombstoned docs until the next merge.
* Prefix queries (`foo*`) match folded, unstemmed terms against the stemmed
  dictionary, so `runn*` misses `run`; expansion is capped at 256 terms and
  these queries skip pruning.
* A merge streams its output to disk, but it still keeps the merged term and doc
  tables in memory; `max_merged_docs` bounds that.
* English uses Porter; Arabic uses a light stemmer (strip article/clitic prefixes and common
  suffixes, never below three letters), no root extraction. Changing analyzer rules changes the
  terms, so existing indexes need `hiis reindex`.
* Fuzzy matching only considers terms with the same first letter, which keeps it to one
  dictionary range per query; and it compares against stems, not surface words.
* Autocomplete maps stems back to words by opening up to 8 documents per suggestion, one word per stem.
* Pagination cursors are the last hit, so concurrent writes can shift idf and make pages
  overlap a little; they never move backwards.
* The HTTP server is plain HTTP/1.1 (no TLS, no chunked request bodies).
