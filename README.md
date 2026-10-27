# HIIS: High-Throughput Inverted Index Search Storage Engine

A full-text search storage engine in safe Rust, with no dependencies.

* Text analysis: tokenizer, normalization, stopwords, Porter stemmer (Arabic folding included)
* Inverted index with positions (phrase queries)
* Posting-list compression: Frame-of-Reference bit-packing + variable-byte, with skip tables
* Immutable, checksummed segments; tiered background merging; tombstone deletes
* WAND + block-max top-k pruning (bit-identical to exhaustive scoring)
* Okapi BM25 ranking, `+must -not "phrase"` query syntax, top-k heap
* Write-ahead log with crash replay (optional fsync per add)
* Exclusive directory lock; checksum-valid-but-corrupt segments give errors, not panics
* Lock-free reads on `Arc` snapshots; crash-safe manifest (tmp + fsync + rename)

Design details and diagrams: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

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
cargo test                                   # 56 unit + 18 integration + 8 robustness + doctest
cargo run --release --example bench -- 100000
```

The integration tests cover ranking, phrases (including stopword gaps), boolean
clauses, deletes (buffered and flushed), merge equivalence (same hits and
scores before/after), reopen, WAL crash recovery (torn tail, no double replay), corruption detection, background-merge
convergence, concurrent indexing + searching, and the directory lock.

`tests/robustness.rs` adds seeded randomised round-trips for every codec and posting list (checked against a naive model), garbage/mutation tests that fix the CRC so corrupt data reaches the decoders, orphan-file cleanup, and a real crash test that `kill`s the CLI mid-indexing five times and audits that the recovered index is a gap-free prefix with intact text.

Sample bench run (synthetic skewed corpus, 100k docs, one noisy Windows
machine, so treat as order of magnitude): 3-9k docs/s ingest (run-to-run spread; the WAL accounts for under 3% of it), 1.6-2.4 s to merge
down to one segment, 2-6 ms for single-term queries matching ~95% of docs,
~13-27 ms for a phrase over two such terms. WAND pruning on 2-3 term OR queries with a rare term: ~0.5 ms vs ~2-3.5 ms exhaustive (4-6x).

## Layout

```
src/analysis/   tokenizer, stemmer, stopwords, Analyzer
src/codec/      vbyte, bitpack (FoR), crc32
src/index/      postings (writer + cursor), builder, segment, merge
src/search/     bm25, query parser, execution
src/engine.rs   write path, manifest, merger thread, snapshots
```
