//! cargo run --release --example bench -- [n_docs] [threads]
//!
//! Synthetic skewed corpus: not a substitute for a real dataset, but enough
//! to see indexing rate, on-disk size vs raw text, and query latency.

use std::time::Instant;

use hiis::{Config, Engine};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn main() -> hiis::Result<()> {
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let dir = std::env::temp_dir().join(format!("hiis-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let vocab: Vec<String> = (0..20_000).map(|i| format!("w{}x{}", i, i % 97)).collect();
    let threads: usize = std::env::args().nth(2).and_then(|a| a.parse().ok()).unwrap_or(1);
    let engine = Engine::open(&dir, Config { flush_docs: 20_000, ..Config::default() })?;

    let t = Instant::now();
    let raw: usize = std::thread::scope(|sc| {
        let jobs: Vec<_> = (0..threads)
            .map(|th| {
                let (engine, vocab) = (&engine, &vocab);
                sc.spawn(move || -> hiis::Result<usize> {
                    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((th as u64 + 1) * 0x1234_5678_9ABC));
                    let mut raw = 0;
                    for _ in 0..n / threads {
                        let len = 40 + (rng.next() % 80) as usize;
                        let mut doc = String::new();
                        for _ in 0..len {
                            // cubing a uniform sample skews picks toward low ranks
                            let u = (rng.next() % 1_000_000) as f64 / 1_000_000.0;
                            doc.push_str(&vocab[(u * u * u * vocab.len() as f64) as usize]);
                            doc.push(' ');
                        }
                        raw += doc.len();
                        engine.add_document(&doc)?;
                    }
                    Ok(raw)
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).sum::<hiis::Result<usize>>()
    })?;
    engine.commit()?;
    let secs = t.elapsed().as_secs_f64();
    println!(
        "ingest: {n} docs in {secs:.2}s = {:.0} docs/s, {:.1} MB/s",
        n as f64 / secs,
        raw as f64 / 1e6 / secs
    );

    let t = Instant::now();
    engine.wait_for_merges();
    engine.force_merge()?;
    println!("merge to 1 segment: {:.2}s", t.elapsed().as_secs_f64());

    let st = engine.stats();
    println!(
        "index: {} segments, {} terms, {:.1} MB on disk (incl. stored text) vs {:.1} MB raw",
        st.segments,
        st.terms,
        st.disk_bytes as f64 / 1e6,
        raw as f64 / 1e6
    );

    let queries = [
        vocab[0].clone(),
        format!("{} {}", vocab[1], vocab[500]),
        format!("{} {} {}", vocab[0], vocab[3000], vocab[9000]),
        format!("+{} +{}", vocab[2], vocab[3]),
        format!("\"{} {}\"", vocab[0], vocab[1]),
    ];
    println!("{:<30} {:>12} {:>12}", "query", "pruned us", "exhaustive us");
    for q in &queries {
        let reps = 100;
        let mut times = [0.0f64; 2];
        for (slot, pruned) in [true, false].into_iter().enumerate() {
            let t = Instant::now();
            for _ in 0..reps {
                let _ = if pruned { engine.search(q, 10)? } else { engine.search_unpruned(q, 10)? };
            }
            times[slot] = t.elapsed().as_micros() as f64 / reps as f64;
        }
        println!("{q:<30} {:>12.0} {:>12.0}", times[0], times[1]);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
