# hiis: benchmarks

Measured 2026-10-30, on a laptop that was busy with other work (several terminals, a browser), so absolute numbers are well below the previous run of this file: the same ingest code measured 12.9k docs/s on 10-29 and 5.9-7.7k docs/s now. Treat the **old vs new** comparison as the result and the absolute values as a lower bound.

| | |
|---|---|
| CPU | Intel i5-8365U, 4 cores / 8 threads, 1.6 GHz base |
| RAM / disk | 8 GB, SSD |
| OS | Windows 11 |
| Toolchain | Rust 1.99.1, `--release` |

## Method

```
cargo run --release --example bench -- 100000
```

A synthetic skewed corpus of 100,000 documents (20,000 distinct terms, 67.0 MB of raw text). Six runs in total, **interleaved**: new build, old build, new, old, new, old, so a slow minute hits both. "Old" is commit `2d6649a` (before the query language, fields and format v5), "new" is 0.2.1. Tables show the median of three with min-max in brackets. Queries are timed over 100 repetitions each, top 10.

## Indexing

| | old | new |
|---|---|---|
| ingest, docs/s | 6,086 (6,086 - 7,733) | 5,905 (5,905 - 7,662) |
| merge down to one segment | 3.66 s (2.35 - 3.98) | 3.29 s (3.05 - 5.99) |
| index on disk | 70.9 MB | 71.0 MB |

Ingest is 1-4% slower in every pair (7,662 vs 7,733; 6,005 vs 6,261; 5,905 vs 6,086): each document now goes through the fielded-document check and carries a per-field length list. That is small but it showed up in all three pairs, so I'm reporting it as real. Merge times swing too much between runs (2.4 - 6.0 s) to say anything. The format v5 field section costs about a byte per plain document.

## Queries that existed before

Latency in µs, MaxScore pruning against exhaustive scoring on the same index.

| query | old pruned | new pruned | new exhaustive |
|---|---:|---:|---:|
| one very common term | 5,619 | 6,520 | 6,851 |
| two terms, one rare | 507 | 440 | 5,889 |
| three terms, mixed frequencies | 283 | 271 | 8,865 |
| two required terms (`+a +b`) | 4,607 | 6,585 | 6,513 |
| phrase of two common terms | 21,422 | 20,723 | 19,149 |

The min-max ranges of old and new overlap on every row; the machine was too noisy to separate them, so I see no regression and no gain. The two-term and three-term pruning wins are intact: about 13x and 33x against exhaustive on the same build.

## New query kinds

| query | new |
|---|---:|
| fuzzy `w1x1~1` (scans every term starting with `w`) | 6,298 |
| prefix `w500*` | 862 |
| proximity `"w0x0 w1x1"~5` | 50,653 |
| group `(w1x1 OR w500x15) AND w2x2` | 8,739 |
| boosted OR `w1x1^3 w500x15` | 1,175 (exhaustive: 5,669) |

These did not exist in the old build, so there is nothing to compare against. What they show:

- Boosts keep MaxScore pruning (a boosted plain OR is still about 5x faster than exhaustive), because the boost multiplies the bounds.
- Fuzzy on a short word is as expensive as a common term here because the synthetic vocabulary has 20,000 terms that all start with `w`; with a real vocabulary the first-letter restriction cuts the scan far more.
- Proximity is the slowest query: it decodes positions for both very common terms and sorts them per document. Phrase and group queries are exhaustive by design.

## Server

```
cargo run --release --example serve_bench -- 20000 <clients> 3
```

20,000 synthetic documents, `GET /api/v1/search?k=10` over keep-alive connections, a rotating mix of five queries (one common term, two terms, three terms, a phrase, a prefix). Three 3-second rounds per client count, median with min-max. Clients and server run on the same machine and compete for the same cores, so this understates what a separate client box would see. The three client counts were run one after another, not interleaved.

| clients | req/s | p50 µs | p95 µs | p99 µs |
|---:|---:|---:|---:|---:|
| 1 | 263 (182 - 297) | 1,022 (973 - 1,071) | 16,061 (15,650 - 19,379) | 22,910 (19,247 - 61,846) |
| 8 | 1,685 (1,239 - 1,868) | 1,103 (1,015 - 1,107) | 17,790 (16,041 - 29,520) | 25,038 (23,619 - 54,377) |
| 64 | 1,881 (1,876 - 1,983) | 1,270 (1,130 - 1,559) | 111,106 (85,193 - 128,394) | 573,687 (543,216 - 830,304) |

- The p50 is about 1 ms, but the mix contains a phrase and a prefix query that cost 15 ms or more, and one client waits for each response, so a single client only manages a few hundred requests a second. The p95 is those slow queries, not queueing.
- Throughput scales from 1 to 8 clients (about 6x) and then flattens near 1,900 req/s: that is where the 4 cores (8 threads, shared with the clients) run out. Past that, more clients only add queueing, which shows as the 64-client p95/p99.
- With 64 clients the p99 is over half a second. A thread per connection is simple and doesn't starve idle clients, but it does not shed load gracefully before `max_connections`. Behind a proxy that limits concurrency this isn't a problem; exposed directly, lower `--max-conns`.
- Writes (POST /documents) are not measured.

## Not measured

- Ranking quality on SciFact (nDCG@10 0.6827) and the Tantivy comparison in the README need the BEIR datasets and a separate build. They were not rerun today and come from an earlier session.
- Field-restricted queries (`title:x`) and per-field length scoring were not benchmarked; the synthetic corpus has no fields.
