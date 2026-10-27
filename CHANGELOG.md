# Changelog

## 0.1.0

* Text analysis: tokenizer, normalization, stopwords, Porter stemmer, Arabic orthographic folding.
* Inverted index with positions; FoR + variable-byte posting-list compression with skip tables.
* Immutable checksummed segments (format v2: per-block `max_tf`), tiered background merging, tombstone deletes.
* Okapi BM25 ranking; `+must -not "phrase"` queries; WAND + block-max top-k pruning, bit-identical to exhaustive scoring.
* Write-ahead log with crash replay; atomic manifest; orphan cleanup on open.
* Exclusive directory lock (`Error::Locked`) so two engines can't open one index.
* Posting decoding validates skip entries and doc-id ranges, so a checksum-valid but corrupt segment yields errors, not panics.
* Randomised codec tests, mutation tests on checksum-fixed segments, and a kill-the-indexer crash test.
