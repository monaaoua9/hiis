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
header 16B | postings | term dictionary (sorted) | doc table | stored text | footer (offsets, CRC32, magic)
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


## Top-k pruning (WAND + block-max)

Plain multi-term queries (`a b c`) skip documents that cannot reach the current k-th score. Each skip-table entry stores the block's `max_tf`, which gives a score upper bound (BM25 rises with tf and falls with length, so length 0 is the ceiling). WAND picks a pivot doc from term-level bounds, then a block-level bound lets whole 128-doc blocks be skipped. Pruning only drops docs whose bound is strictly below the heap minimum, and survivors are summed in clause order, so results are **bit-identical** to exhaustive scoring (a randomized test compares 180 query/k combinations across merged segments with deletes). MUST/NOT/phrase queries use the exhaustive path.

## Known limits (deliberate scope)

* Segments are read fully into memory (no mmap, since the crate is `unsafe`-free
  and dependency-free). Swapping `Vec<u8>` for a mapped slice is local to `segment.rs`.
* Doc length is exact (not Lucene's 1-byte lossy norm); document frequencies
  include tombstoned docs until the next merge.
* Prefix queries (`foo*`) match folded, unstemmed terms against the stemmed
  dictionary, so `runn*` misses `run`; expansion is capped at 256 terms and
  these queries skip WAND.
* A merge holds its inputs and its output in memory; `max_merged_docs` is the
  only bound on that.
* English stemmer/stopwords only; Arabic gets orthographic folding but no stemming.
