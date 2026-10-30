# hiis: design decisions

The short list of choices behind the engine. Analysis pipeline, file formats, merge and WAL details: [ARCHITECTURE.md](ARCHITECTURE.md).

## Scope

An embedded full-text search engine: BM25 ranking, a query language (phrases, groups, fuzzy, proximity, boosts, named fields), deletes, immutable segments merged in the background, a WAL, and a small HTTP front end. The crate is `unsafe`-free and has no dependencies; the Tantivy comparison lives in a separate crate so that stays true.

## Decisions

**Immutable segments plus an atomic manifest.** A flush writes a segment to a temp file, fsyncs and renames; the `MANIFEST` is the single source of truth for what's searchable. Readers clone an `Arc<Segment>` snapshot and run without locks, so a flush or merge never blocks or invalidates a query. The merge streams term dictionaries in lock step, drops tombstoned docs and renumbers survivors without re-analysing any text.

**Positions are assigned before stopword removal.** "quick the fox" keeps a gap where "the" was, so a phrase query still knows the words weren't adjacent. The cost is that stopped tokens still consume position numbers.

**Postings in 128-doc frame-of-reference blocks with a skip table.** Doc ids are delta-coded and bit-packed per block; positions sit behind the doc/freq section so ranking-only queries never decode them. The skip table lets `advance` jump whole blocks, which makes AND and phrase queries cost proportional to the rarest term.

**Scores don't depend on segment boundaries.** IDF and average document length come from the whole snapshot, not from each segment. That costs a bit of bookkeeping and buys a testable property: results are identical before and after a merge.

**MaxScore pruning that is bit-identical to exhaustive scoring.** Each skip entry stores the block's `max_tf`, which bounds the score. Terms whose bounds can't reach the current k-th score only get probed, never drive iteration. Pruning only drops documents strictly below the heap minimum and survivors are summed in clause order, so the answer matches the exhaustive path exactly. A randomized test compares 180 query/k combinations across merged segments with deletes.

**Deletes are tombstone bits, reconciled during merges.** A delete that arrives while a merge runs is applied to the merged segment under the same write lock that swaps it in, so none are lost. Document frequencies include tombstoned docs until the next merge, which slightly skews IDF in between.

**WAL before the RAM buffer.** Every `add_document` is appended and CRC-framed first; a flush empties the log only after the segment and manifest are durable. On open the log is replayed and a torn tail is cut. `wal_fsync` trades throughput for power-loss safety.

**Every query feature is a list of `(doc, score)` per segment.** Prefix, fuzzy, proximity and groups all evaluate into the same doc-sorted list, so the combine step (must / should / must-not) never learned about them. The price: only plain OR queries get MaxScore pruning; everything else is exhaustive. The benchmarks show what that costs.

**Fields are extra postings, not a second index.** A fielded document is indexed under its plain terms and again as `title:term`, so unfielded queries are unchanged and the segment layout barely moved (v5 only adds per-field token counts, so BM25 can normalise by field length). The cost is a larger dictionary, and phrases are kept from crossing fields by a position gap instead of a real boundary.

**Fuzzy matching only keeps terms with the same first letter.** That turns a dictionary scan into one sorted range. Typos on the first letter are missed on purpose.

**One thread per connection, capped.** The first server used a fixed worker pool and a few idle keep-alive clients could starve everyone else for the idle timeout; a load test caught it. Now idle connections cost a thread each up to `max_connections`, then get a 503. No TLS: put a proxy in front.

**No mmap, no unsafe.** Segments up to `resident_limit` (512 MB) are held in memory. Bigger ones keep the dictionary and doc table resident and read postings with positional reads. That mode costs roughly 1.5–2× query latency on TREC-COVID. I accepted it to keep the crate dependency-free and `unsafe`-free.

## How it's checked

Random query strings built from the syntax characters check that the parser never panics and that pruned, exhaustive, counted, paginated and explained results all agree. Seeded randomised round-trips for every codec and posting list against a naive model; garbage and mutation tests that fix the CRC so corrupt data reaches the decoders; a segment test that flips every byte and requires `Corrupt`, never a wrong answer; merge-equivalence tests; a real crash test that kills the CLI mid-indexing five times and audits that the recovered index is a gap-free prefix. Ranking quality is measured on SciFact (nDCG@10 0.6827, see the README).

## Known limits

English uses Porter and Arabic a light affix stripper (no roots), so Arabic recall is approximate. Changing analyzer rules changes the terms and needs `hiis reindex`. Prefix queries match unstemmed terms against the stemmed dictionary and skip pruning. A merge keeps its merged term and doc tables in memory. Stored text uses a hand-written LZ77 that is slower and weaker than a real codec. Tantivy indexes about twice as fast on the large collection (see the README).
