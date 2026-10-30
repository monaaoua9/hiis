//! cargo run --release --example analyze_bench -- [docs] [reps]
//!
//! CPU-only timing of the analyzer (tokenize, fold, stopwords, stem), best of `reps`.
//! Two corpora: the synthetic `w123x45` words of `bench` (digits, so the stemmer skips them)
//! and random words from data/sample.txt (English and Arabic, so stemming and stopwords run).

use std::time::Instant;

use hiis::analysis::{Analyzer, AnalyzerConfig};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn corpus(words: &[String], n: usize, rng: &mut Rng) -> Vec<String> {
    (0..n)
        .map(|_| {
            let len = 40 + (rng.next() % 80) as usize;
            let mut doc = String::new();
            for _ in 0..len {
                doc.push_str(&words[(rng.next() % words.len() as u64) as usize]);
                doc.push(' ');
            }
            doc
        })
        .collect()
}

fn main() {
    let arg = |i: usize, d: usize| std::env::args().nth(i).and_then(|a| a.parse().ok()).unwrap_or(d);
    let (n, reps) = (arg(1, 20_000), arg(2, 5));
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    let synthetic: Vec<String> = (0..20_000).map(|i| format!("w{}x{}", i, i % 97)).collect();
    let sample: Vec<String> =
        include_str!("../data/sample.txt").split_whitespace().map(str::to_string).collect();
    let an = Analyzer::new(AnalyzerConfig::default());

    for (name, words) in [("synthetic", &synthetic), ("sample words", &sample)] {
        let docs = corpus(words, n, &mut rng);
        let bytes: usize = docs.iter().map(|d| d.len()).sum();
        let mut best = f64::MAX;
        let mut tokens = 0;
        for _ in 0..reps {
            let t = Instant::now();
            tokens = docs.iter().map(|d| an.analyze(d).len()).sum::<usize>();
            best = best.min(t.elapsed().as_secs_f64());
        }
        println!(
            "{name:13} {n} docs, {tokens} tokens: best {:.1} ms = {:.1} MB/s, {:.0} ns/token",
            best * 1e3,
            bytes as f64 / 1e6 / best,
            best * 1e9 / tokens as f64
        );
    }
}
