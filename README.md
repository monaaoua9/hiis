# HIIS: High-Throughput Inverted Index Search Storage Engine

[![ci](https://github.com/monaaoua9/hiis/actions/workflows/ci.yml/badge.svg)](https://github.com/monaaoua9/hiis/actions/workflows/ci.yml) [![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A full-text search storage engine in safe Rust, with no dependencies.

* Text analysis: tokenizer, normalization, stopwords, Porter stemmer (Arabic folding included)
* Inverted index with positions (phrase queries)
* Posting-list compression: Frame-of-Reference bit-packing + variable-byte, with skip tables
* Immutable, checksummed segments; tiered background merging; tombstone deletes
* MaxScore top-k pruning (bit-identical to exhaustive scoring)
* Okapi BM25 ranking, `+must -not "phrase" prefix*` query syntax, top-k heap
* Write-ahead log with crash replay (optional fsync per add, or per batch via `add_documents`)
* Streamed merges, a merge size cap (`max_merged_docs`), and segments past `resident_limit` read from disk instead of RAM
* Exclusive directory lock; checksum-valid-but-corrupt segments give errors, not panics
* Lock-free reads on `Arc` snapshots; crash-safe manifest (tmp + fsync + rename)

Design details and diagrams: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The decisions and their trade-offs, shorter: [docs/DESIGN.md](docs/DESIGN.md).

## Use

```rust
use hiis::{Config, Engine};

let engine = Engine::open("./idx", Config::default())?;
let id = engine.add_document("the quick brown fox")?;
engine.commit()?;                       // flush + persist: now searchable and durable
for hit in engine.search("\"quick brown\" -dog", 10)? {
    println!("{} {:.3} {:?}", hit.doc_id, hit.score, engine.get(hit.doc_id));
}
```

CLI:

```
cargo run --release -- index  ./idx data/sample.txt
cargo run --release -- search ./idx "\"posting lists\""  -k 5
cargo run --release -- search ./idx "+segment merging -posting"
cargo run --release -- stats  ./idx
```

## Test and measure

```
cargo test                                   # 63 unit + 20 integration + 1 disk-backed + 8 robustness + doctest
cargo run --release --example bench -- 100000 [threads]
```

The integration tests cover ranking, phrases (including stopword gaps), boolean
clauses, deletes (buffered and flushed), merge equivalence (same hits and
scores before/after), reopen, WAL crash recovery (torn tail, no double replay), corruption detection, background-merge
convergence, concurrent indexing + searching, and the directory lock.

`tests/robustness.rs` adds seeded randomised round-trips for every codec and posting list (checked against a naive model), garbage/mutation tests that fix the CRC so corrupt data reaches the decoders, orphan-file cleanup, and a real crash test that `kill`s the CLI mid-indexing five times and audits that the recovered index is a gap-free prefix with intact text.

Sample bench run (synthetic skewed corpus, 100k docs, one noisy Windows
machine, so treat as order of magnitude): ~13k docs/s ingest (run-to-run spread; the WAL accounts for under 3% of it), 1.6-2.4 s to merge
down to one segment, 2-6 ms for single-term queries matching ~95% of docs,
~13-27 ms for a phrase over two such terms. Pruning on multi-term OR queries: 3.6x less total time than exhaustive on TREC-COVID (see below).

## Ranking quality

`examples/eval.rs` runs a BEIR-layout collection and reports nDCG@10, MRR@10
and Recall@100. On SciFact (5,183 abstracts, 300 test queries, default
analyzer, title + text indexed together):

```
nDCG@10 0.6827   MRR@10 0.6455   Recall@100 0.9276   p50 0.7 ms, p95 2.5 ms
```

For reference, the BEIR paper reports 0.665 (Elasticsearch BM25) and Anserini's
flat BM25 baseline is about 0.679 on the same split.

```
cargo run --release --example eval -- path/to/scifact
```

## Against Tantivy

`bench-compare/` is a separate crate (the main one stays dependency-free) that
runs the same BEIR collection, queries and qrels through hiis and Tantivy 0.27,
with an equivalent analyzer (lowercase, English stopwords, Porter stemmer),
OR queries, top-10, stored text on both, and each index merged down to one
segment. One 8-core Windows machine, so numbers move around by 20% or more
between runs:

| collection | engine | index time | docs/s | on disk | nDCG@10 | query p50 / p95 |
|---|---|---|---|---|---|---|
| SciFact, 5k docs | hiis | 0.5 s | 10,500 | 7.7 MB | 0.6827 | 0.47 / 0.75 ms |
| | tantivy | 1.0 s | 5,100 | 22 MB | 0.6863 | 0.40 / 0.93 ms |
| TREC-COVID, 171k docs, 192 MB | hiis | 18.8 s | 9,100 | 181 MB | 0.5798 | 2.5 / 8.3 ms |
| | tantivy | 9.0 s | 19,100 | 170 MB | 0.5796 | 2.5 / 7.1 ms |

Ranking quality is the same, queries are on par, and index sizes are within
about 7% of each other (both compress the stored text). Tantivy still indexes
about twice as fast on the large set. The query timings include fetching the
top 10 stored documents.

```
cd bench-compare && cargo run --release -- path/to/trec-covid 8
```

## Layout

```
src/analysis/   tokenizer, stemmer, stopwords, Analyzer
src/codec/      vbyte, bitpack (FoR), crc32
src/index/      postings (writer + cursor), builder, segment, merge
src/search/     bm25, query parser, execution
src/engine.rs   write path, manifest, merger thread, snapshots
```
