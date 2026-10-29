# hiis: design decisions

The short list of choices behind the engine. Analysis pipeline, file formats, merge and WAL details: [ARCHITECTURE.md](ARCHITECTURE.md).

## Scope

An embedded full-text search engine: BM25 ranking, phrase and boolean queries, deletes, immutable segments merged in the background, a WAL. The crate is `unsafe`-free and has no dependencies; the Tantivy comparison lives in a separate crate so that stays true.

## Decisions

**Immutable segments plus an atomic manifest.** A flush writes a segment to a temp file, fsyncs and renames; the `MANIFEST` is the single source of truth for what's searchable. Readers clone an `Arc<Segment>` snapshot and run without locks, so a flush or merge never blocks or invalidates a query. The merge streams term dictionaries in lock step, drops tombstoned docs and renumbers survivors without re-analysing any text.

**Positions are assigned before stopword removal.** "quick the fox" keeps a gap where "the" was, so a phrase query still knows the words weren't adjacent. The cost is that stopped tokens still consume position numbers.

**Postings in 128-doc frame-of-reference blocks with a skip table.** Doc ids are delta-coded and bit-packed per block; positions sit behind the doc/freq section so ranking-only queries never decode them. The skip table lets `advance` jump whole blocks, which makes AND and phrase queries cost proportional to the rarest term.

**Scores don't depend on segment boundaries.** IDF and average document length come from the whole snapshot, not from each segment. That costs a bit of bookkeeping and buys a testable property: results are identical before and after a merge.

**MaxScore pruning that is bit-identical to exhaustive scoring.** Each skip entry stores the block's `max_tf`, which bounds the score. Terms whose bounds can't reach the current k-th score only get probed, never drive iteration. Pruning only drops documents strictly below the heap minimum and survivors are summed in clause order, so the answer matches the exhaustive path exactly. A randomized test compares 180 query/k combinations across merged segments with deletes.

**Deletes are tombstone bits, reconciled during merges.** A delete that arrives while a merge runs is applied to the merged segment under the same write lock that swaps it in, so none are lost. Document frequencies include tombstoned docs until the next merge, which slightly skews IDF in between.

**WAL before the RAM buffer.** Every `add_document` is appended and CRC-framed first; a flush empties the log only after the segment and manifest are durable. On open the log is replayed and a torn tail is cut. `wal_fsync` trades throughput for power-loss safety.

**No mmap, no unsafe.** Segments up to `resident_limit` (512 MB) are held in memory. Bigger ones keep the dictionary and doc table resident and read postings with positional reads. That mode costs roughly 1.5–2× query latency on TREC-COVID. I accepted it to keep the crate dependency-free and `unsafe`-free.

## How it's checked

Seeded randomised round-trips for every codec and posting list against a naive model; garbage and mutation tests that fix the CRC so corrupt data reaches the decoders; a segment test that flips every byte and requires `Corrupt`, never a wrong answer; merge-equivalence tests; a real crash test that kills the CLI mid-indexing five times and audits that the recovered index is a gap-free prefix. Ranking quality is measured on SciFact (nDCG@10 0.6827, see the README).

## Known limits

English stemmer and stopwords only (Arabic gets folding, no stemming). Prefix queries match unstemmed terms against the stemmed dictionary and skip pruning. A merge keeps its merged term and doc tables in memory. Stored text uses a hand-written LZ77 that is slower and weaker than a real codec. Tantivy indexes about twice as fast on the large collection (see the README).
