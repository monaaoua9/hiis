# hiis: benchmarks

Measured 2026-10-29.

| | |
|---|---|
| CPU | Intel i5-8365U, 4 cores / 8 threads, 1.6 GHz base |
| RAM / disk | 8 GB, SSD |
| OS | Windows 11 |
| Toolchain | Rust 1.99.1, `--release` |

## Indexing and query timing

```
cargo run --release --example bench -- 100000
```

A synthetic skewed corpus of 100,000 documents (20,000 distinct terms, 67.0 MB of raw text). Three back-to-back runs; median with min–max in brackets.

| | |
|---|---|
| ingest | 12,921 docs/s (12,485 – 13,275) |
| merge down to one segment | 1.68 s (1.66 – 1.79) |
| index on disk | 70.9 MB, including the stored text |

Query latency in µs, MaxScore pruning against exhaustive scoring on the same index:

| query | pruned | exhaustive |
|---|---:|---:|
| one very common term | 2,311 (2,009 – 2,456) | 2,662 (2,393 – 2,895) |
| two terms, one rare | 256 (233 – 304) | 1,969 (1,679 – 2,172) |
| three terms, mixed frequencies | 144 (130 – 148) | 3,432 (3,014 – 3,692) |
| two required terms (`+a +b`) | 1,741 (1,637 – 2,071) | 1,702 (1,701 – 2,005) |
| phrase of two common terms | 8,773 (8,642 – 10,751) | 9,029 (8,573 – 10,980) |

## What these say

- Pruning pays off when terms differ a lot in frequency: about 8× on the two-term query and about 24× on the three-term one. For a single term that matches most documents there's little to skip, so the gain is small (about 13%).
- Required-term and phrase queries use the exhaustive path by design, so the two columns match there. Differences in those rows are noise.
- Ingest here is higher than the 3–9k docs/s in the README. That text came from a noisier machine state; three runs now agree within about 6%, so I trust this file for the current code.

## Not rerun today

- **Ranking quality on SciFact** (nDCG@10 0.6827) and the **comparison against Tantivy** in the README need the BEIR datasets and a separate crate build, and the comparison takes minutes per collection. I didn't repeat them; treat those README numbers as an earlier run on this machine, with the "20% or more between runs" caveat the README already gives.
- This file compares hiis with nothing else.
