# Changelog

## Unreleased

* Server: fixed idle keep-alive connections starving new ones (a fixed worker pool let a few idle clients block everyone for the idle timeout). Each connection now has its own thread, capped by `ServerConfig::max_connections` (default 256, then 503 `OVERLOADED`); `--threads` became `--max-conns`. Idle connections notice shutdown within 200 ms.

## 0.2.0 - 2026-10-30

Segment format is now v5. v4 indexes still open and are upgraded by merges, but older binaries can't read v5. Arabic terms changed (new stemmer): run `hiis reindex` on existing Arabic content.

* Per-field BM25 length normalisation: segment format v5 stores each document's token count per field, and `title:x` is normalised by title length and the average title length. v4 segments still open (they fall back to whole-document length); merges rewrite them as v5. Deletes keep the per-field averages live.
* `Engine::searcher()`: frozen view of the committed segments (search, search_page, count, get) so pagination stays consistent across commits and merges. Later deletes still show.
* Named fields: `Engine::add_fields`, `title:fox`, `body:"a b"`, `title:(a OR b)`, `title:fox^3`. Field terms are extra postings (`title:fox`) next to the ordinary ones, so unfielded queries are unchanged and the segment format stays v4. HTTP `{"fields": {..}}`, export/load/reindex keep fields.
* `hiis serve --read-only` serves an existing index without taking the write lock.
* `Config::read_only`: open an index for searching only. Shared lock so several readers can use one directory, nothing on disk is created or removed, the log is ignored, writes return an error.
* `Engine::top_terms` / `hiis terms`: most widespread indexed terms.
* Randomised query tests (parser, pruned vs exhaustive, count, explain, pagination all agree on random syntax soup).
* `Engine::count(query)`: number of matching documents without ranking, and `count=1` on the HTTP search for a `total`.
* `Engine::for_each_document` and CLI `export` (JSON lines), `load`, `reindex <dir> <dest>` to rebuild an index with the current analyzer, for when analysis rules change.
* `hiis serve`: std-only HTTP/1.1 + JSON API (`/api/v1/documents`, `search` with cursor pagination and snippets, `suggest`, `similar`, `explain`, `commit`, `stats`). Keep-alive, thread pool, optional bearer token, background commit so writes show up without calling commit. Embeddable as `hiis::server::Server`.
* Arabic: light stemmer (article/clitic prefixes, plural and pronoun suffixes; `AnalyzerConfig::arabic_stem`) and an Arabic stopword list. **Terms change for Arabic text, so re-index existing Arabic content.** Autocomplete offers words without their article.
* `Engine::similar` / `hiis similar`: more-like-this. The document becomes a weighted OR query of its 25 highest tf-idf terms (terms nobody else has are ignored), run through the normal pruned search.
* `Engine::search_page` with a text `Cursor`: next-k-after-this-hit pagination instead of offsets; pruning still applies. Pages concatenate to exactly the full ranking.
* `Engine::verify(deep)` / `hiis verify`: walks every posting list (order, counts, positions, per-document token totals) and, with `--deep`, re-analyses all stored text. Catches damage that still passes the segment checksum.
* `Engine::backup(dest)` / `hiis backup`: consistent copy of the committed index while searches and writes continue; merges wait for the copy. The result opens as an independent index.
* `Engine::explain` / `hiis explain`: per-clause score breakdown for one document (idf, tf, lengths for terms), also for documents that did not make it, which shows what excluded them. The total equals the score `search` reports.
* `Engine::complete` (autocomplete) and `Engine::did_you_mean`: both answered from the term dictionary, and mapped from stems back to words that occur in live documents. CLI: `complete`, and a suggestion when a search finds nothing.
* `Engine::highlight`: snippet of the densest `max_chars` window with the query's words marked (stemmed, prefix and fuzzy matches included, negated terms not). HTML-escaped by default. The CLI `search` prints snippets with the hits in bold.
* Clause boosts: `title^2`, `"a b"^1.5`, `(x y)^3`. `^0` turns a clause into a pure filter. MaxScore applies the boost to its bounds, results stay identical to exhaustive scoring.
* Proximity queries: `"quick fox"~3` matches the words within a window of the phrase, in any order (a swap costs 2). Ranked by window count.
* Fuzzy terms: `serch~`, `serch~2`. Optimal-string-alignment distance over the term dictionary (same first letter, up to 64 variants), closer variants score higher.
* Grouped queries: `(a OR b) AND NOT c`, nested to 16 levels. `AND`/`OR`/`NOT` keywords (capitals only) next to `+`/`-`.
* Prefix queries (`compres*`), expanded against the sorted term dictionary of every segment.
* `MergePolicy::max_merged_docs` (default 5M) caps merge size and so peak merge memory; segments at the cap are left alone.
* Analyzer allocates once per token instead of three times (tokenizer no longer collects chars, stemmer takes the string by value).
* Per-document term grouping happens before the buffer lock is taken, so concurrent `add_document` calls only serialise on the hash-map merge and the WAL append. `bench` takes a thread count as second argument.
* `Engine::add_documents`: batch ingest with a single WAL write and fsync; the CLI `index` command uses it.
* `examples/eval.rs`: nDCG/MRR/recall on BEIR-format collections (SciFact nDCG@10 = 0.683).
* Multi-term pruning is now MaxScore instead of WAND: 3.6x less query time on TREC-COVID, still bit-identical to exhaustive scoring.
* Per-thread stem cache; `add_documents` analyses a batch across cores.
* Stored text is compressed (8 KiB LZ blocks, no dependencies): TREC-COVID index 246 MB -> 181 MB. Segment format v4.
* Segment format v3: counts moved to the footer so merges can stream their output. Merged segments are written straight to disk.
* `Config::resident_limit`: segments bigger than this are read from disk with positional reads instead of being held in memory.
* `bench-compare/`: side-by-side run against Tantivy.

## 0.1.0

* Text analysis: tokenizer, normalization, stopwords, Porter stemmer, Arabic orthographic folding.
* Inverted index with positions; FoR + variable-byte posting-list compression with skip tables.
* Immutable checksummed segments (format v2: per-block `max_tf`), tiered background merging, tombstone deletes.
* Okapi BM25 ranking; `+must -not "phrase"` queries; WAND + block-max top-k pruning, bit-identical to exhaustive scoring.
* Write-ahead log with crash replay; atomic manifest; orphan cleanup on open.
* Exclusive directory lock (`Error::Locked`) so two engines can't open one index.
* Posting decoding validates skip entries and doc-id ranges, so a checksum-valid but corrupt segment yields errors, not panics.
* Randomised codec tests, mutation tests on checksum-fixed segments, and a kill-the-indexer crash test.
