# Changelog

## Unreleased

* Prefix queries (`compres*`), expanded against the sorted term dictionary of every segment.
* `MergePolicy::max_merged_docs` (default 5M) caps merge size and so peak merge memory; segments at the cap are left alone.
* Analyzer allocates once per token instead of three times (tokenizer no longer collects chars, stemmer takes the string by value).
* Per-document term grouping happens before the buffer lock is taken, so concurrent `add_document` calls only serialise on the hash-map merge and the WAL append. `bench` takes a thread count as second argument.
* `Engine::add_documents`: batch ingest with a single WAL write and fsync; the CLI `index` command uses it.
* `examples/eval.rs`: nDCG/MRR/recall on BEIR-format collections (SciFact nDCG@10 = 0.683).

## 0.1.0

* Text analysis: tokenizer, normalization, stopwords, Porter stemmer, Arabic orthographic folding.
* Inverted index with positions; FoR + variable-byte posting-list compression with skip tables.
* Immutable checksummed segments (format v2: per-block `max_tf`), tiered background merging, tombstone deletes.
* Okapi BM25 ranking; `+must -not "phrase"` queries; WAND + block-max top-k pruning, bit-identical to exhaustive scoring.
* Write-ahead log with crash replay; atomic manifest; orphan cleanup on open.
* Exclusive directory lock (`Error::Locked`) so two engines can't open one index.
* Posting decoding validates skip entries and doc-id ranges, so a checksum-valid but corrupt segment yields errors, not panics.
* Randomised codec tests, mutation tests on checksum-fixed segments, and a kill-the-indexer crash test.
