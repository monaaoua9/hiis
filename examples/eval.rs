//! cargo run --release --example eval -- path/to/beir-dataset
//!
//! Ranking quality on a BEIR-layout collection (corpus.jsonl, queries.jsonl,
//! qrels/test.tsv), e.g. SciFact. Prints nDCG@10, MRR@10, Recall@100 and
//! query latency.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use hiis::{Config, Engine};

// pulls `"key": "..."` out of one JSON line; enough for BEIR files
fn field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\": \"");
    let mut rest = &line[line.find(&pat)? + pat.len()..];
    let mut out = String::new();
    loop {
        let i = rest.find(['"', '\\'])?;
        out.push_str(&rest[..i]);
        if rest.as_bytes()[i] == b'"' {
            return Some(out);
        }
        let esc = rest[i + 1..].chars().next()?;
        rest = &rest[i + 1 + esc.len_utf8()..];
        match esc {
            'n' | 't' | 'r' => out.push(' '),
            'u' => {
                let code = u32::from_str_radix(rest.get(..4)?, 16).ok()?;
                rest = &rest[4..];
                // surrogate halves can't be chars, a space is fine for ranking
                out.push(char::from_u32(code).unwrap_or(' '));
            }
            c => out.push(c),
        }
    }
}

fn dcg(rels: impl Iterator<Item = u32>) -> f64 {
    rels.enumerate().map(|(i, r)| (2f64.powi(r as i32) - 1.0) / (i as f64 + 2.0).log2()).sum()
}

fn main() -> hiis::Result<()> {
    let root = std::env::args().nth(1).expect("usage: eval <dataset dir>");
    let root = Path::new(&root);

    let dir = std::env::temp_dir().join(format!("hiis-eval-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let engine = Engine::open(&dir, Config::default())?;

    let mut names: Vec<String> = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let t = Instant::now();
    for line in fs::read_to_string(root.join("corpus.jsonl"))?.lines() {
        let (Some(id), Some(text)) = (field(line, "_id"), field(line, "text")) else { continue };
        let title = field(line, "title").unwrap_or_default();
        names.push(id);
        batch.push(format!("{title}. {text}"));
        if batch.len() == 512 {
            engine.add_documents(&batch)?;
            batch.clear();
        }
    }
    engine.add_documents(&batch)?;
    engine.commit()?;
    println!("indexed {} docs in {:.2}s", names.len(), t.elapsed().as_secs_f64());

    let mut qtext = HashMap::new();
    for line in fs::read_to_string(root.join("queries.jsonl"))?.lines() {
        if let (Some(id), Some(text)) = (field(line, "_id"), field(line, "text")) {
            qtext.insert(id, text);
        }
    }
    let mut qrels: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for line in fs::read_to_string(root.join("qrels").join("test.tsv"))?.lines().skip(1) {
        let mut p = line.split('\t');
        if let (Some(q), Some(d), Some(r)) = (p.next(), p.next(), p.next()) {
            qrels.entry(q.to_string()).or_default().insert(d.to_string(), r.trim().parse().unwrap_or(0));
        }
    }

    let (mut ndcg, mut mrr, mut recall, mut n) = (0.0, 0.0, 0.0, 0usize);
    let mut lat = Vec::new();
    for (qid, rel) in &qrels {
        let Some(text) = qtext.get(qid) else { continue };
        // natural-language queries must not be read as operators
        let text: String = text.chars().map(|c| if "\"+-*".contains(c) { ' ' } else { c }).collect();

        let t = Instant::now();
        let hits = engine.search(&text, 100)?;
        lat.push(t.elapsed().as_micros() as f64);

        let got: Vec<u32> =
            hits.iter().map(|h| rel.get(&names[h.doc_id as usize]).copied().unwrap_or(0)).collect();
        let mut ideal: Vec<u32> = rel.values().copied().collect();
        ideal.sort_unstable_by(|a, b| b.cmp(a));
        let best = dcg(ideal.iter().copied().take(10));
        if best > 0.0 {
            ndcg += dcg(got.iter().copied().take(10)) / best;
        }
        mrr += got.iter().take(10).position(|&r| r > 0).map_or(0.0, |i| 1.0 / (i as f64 + 1.0));
        let found = got.iter().filter(|&&r| r > 0).count();
        let total = rel.values().filter(|&&r| r > 0).count().max(1);
        recall += found as f64 / total as f64;
        n += 1;
    }

    lat.sort_by(f64::total_cmp);
    let nf = n.max(1) as f64;
    println!("{n} queries");
    println!("nDCG@10 {:.4}   MRR@10 {:.4}   Recall@100 {:.4}", ndcg / nf, mrr / nf, recall / nf);
    println!(
        "latency: p50 {:.0} us, p95 {:.0} us",
        lat[lat.len() / 2],
        lat[(lat.len() * 95 / 100).min(lat.len() - 1)]
    );
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}
