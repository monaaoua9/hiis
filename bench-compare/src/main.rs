// cargo run --release -- <beir dir> [index threads]
//
// Same collection, same queries, same qrels through hiis and tantivy.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::*;
use tantivy::tokenizer::*;
use tantivy::{doc, Index};

struct Corpus {
    ids: Vec<String>,
    texts: Vec<String>,
    queries: Vec<(String, String)>,
    qrels: HashMap<String, HashMap<String, u32>>,
}

fn load(root: &Path) -> Corpus {
    let mut ids = Vec::new();
    let mut texts = Vec::new();
    for line in fs::read_to_string(root.join("corpus.jsonl")).unwrap().lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        ids.push(v["_id"].as_str().unwrap().to_string());
        texts.push(format!("{}. {}", v["title"].as_str().unwrap_or(""), v["text"].as_str().unwrap_or("")));
    }
    let mut qrels: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for line in fs::read_to_string(root.join("qrels/test.tsv")).unwrap().lines().skip(1) {
        let p: Vec<&str> = line.split('\t').collect();
        qrels.entry(p[0].to_string()).or_default().insert(p[1].to_string(), p[2].trim().parse().unwrap_or(0));
    }
    let mut queries = Vec::new();
    for line in fs::read_to_string(root.join("queries.jsonl")).unwrap().lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let id = v["_id"].as_str().unwrap().to_string();
        if qrels.contains_key(&id) {
            // natural-language queries must not be read as operators
            let t: String = v["text"]
                .as_str()
                .unwrap()
                .chars()
                .map(|c| if "\"+-*:()^~".contains(c) { ' ' } else { c })
                .collect();
            queries.push((id, t));
        }
    }
    Corpus { ids, texts, queries, qrels }
}

fn dcg(rels: impl Iterator<Item = u32>) -> f64 {
    rels.enumerate().map(|(i, r)| (2f64.powi(r as i32) - 1.0) / (i as f64 + 2.0).log2()).sum()
}

// returns (ndcg@10, p50 us, p95 us)
fn score(c: &Corpus, mut run: impl FnMut(&str) -> Vec<usize>) -> (f64, f64, f64) {
    let (mut ndcg, mut lat) = (0.0, Vec::new());
    for (qid, text) in &c.queries {
        let rel = &c.qrels[qid];
        let t = Instant::now();
        let docs = run(text);
        lat.push(t.elapsed().as_micros() as f64);
        let got: Vec<u32> = docs.iter().map(|&d| rel.get(&c.ids[d]).copied().unwrap_or(0)).collect();
        let mut ideal: Vec<u32> = rel.values().copied().collect();
        ideal.sort_unstable_by(|a, b| b.cmp(a));
        let best = dcg(ideal.into_iter().take(10));
        if best > 0.0 {
            ndcg += dcg(got.into_iter().take(10)) / best;
        }
    }
    lat.sort_by(f64::total_cmp);
    (ndcg / c.queries.len() as f64, lat[lat.len() / 2], lat[lat.len() * 95 / 100])
}

fn dir_size(p: &Path) -> u64 {
    fs::read_dir(p).unwrap().flatten().map(|e| e.metadata().map(|m| m.len()).unwrap_or(0)).sum()
}

fn main() {
    let root = std::env::args().nth(1).expect("dataset dir");
    let threads: usize = std::env::args().nth(2).and_then(|a| a.parse().ok()).unwrap_or(1);
    let c = load(Path::new(&root));
    let raw: usize = c.texts.iter().map(|t| t.len()).sum();
    println!(
        "{} docs, {:.1} MB, {} queries, tantivy indexing thread(s): {}",
        c.texts.len(),
        raw as f64 / 1e6,
        c.queries.len(),
        threads
    );
    println!(
        "{:<10} {:>10} {:>10} {:>9} {:>9} {:>9} {:>9}",
        "engine", "index s", "docs/s", "MB disk", "nDCG@10", "p50 us", "p95 us"
    );

    {
        let dir = std::env::temp_dir().join("cmp-hiis");
        let _ = fs::remove_dir_all(&dir);
        let e = hiis::Engine::open(&dir, hiis::Config::default()).unwrap();
        let t = Instant::now();
        // add_documents spreads analysis over the cores itself
        for b in c.texts.chunks(2048) {
            e.add_documents(b).unwrap();
        }
        e.commit().unwrap();
        e.wait_for_merges();
        e.force_merge().unwrap();
        let secs = t.elapsed().as_secs_f64();
        let by_text: HashMap<&str, usize> = c.texts.iter().enumerate().map(|(i, t)| (t.as_str(), i)).collect();
        let (n, p50, p95) = score(&c, |q| {
            e.search(q, 10).unwrap().iter().map(|h| by_text[e.get(h.doc_id).unwrap().as_str()]).collect()
        });
        println!(
            "{:<10} {:>10.1} {:>10.0} {:>9.1} {:>9.4} {:>9.0} {:>9.0}",
            "hiis",
            secs,
            c.texts.len() as f64 / secs,
            dir_size(&dir) as f64 / 1e6,
            n,
            p50,
            p95
        );
        let _ = fs::remove_dir_all(&dir);
    }

    {
        let dir = std::env::temp_dir().join("cmp-tantivy");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut sb = Schema::builder();
        let opts = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("en")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        let body = sb.add_text_field("body", opts);
        let idx = sb.add_u64_field("idx", STORED);
        let index = Index::create_in_dir(&dir, sb.build()).unwrap();
        index.tokenizers().register(
            "en",
            TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(RemoveLongFilter::limit(64))
                .filter(LowerCaser)
                .filter(StopWordFilter::new(Language::English).unwrap())
                .filter(Stemmer::new(Language::English))
                .build(),
        );
        let t = Instant::now();
        let mut w = index.writer_with_num_threads(threads, 256_000_000).unwrap();
        for (i, text) in c.texts.iter().enumerate() {
            w.add_document(doc!(body => text.as_str(), idx => i as u64)).unwrap();
        }
        w.commit().unwrap();
        // merge down to one segment, like the hiis run
        let ids = index.searchable_segment_ids().unwrap();
        if ids.len() > 1 {
            w.merge(&ids).wait().unwrap();
        }
        w.wait_merging_threads().unwrap();
        let secs = t.elapsed().as_secs_f64();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let qp = QueryParser::for_index(&index, vec![body]);
        let (n, p50, p95) = score(&c, |q| {
            let (query, _) = qp.parse_query_lenient(q);
            searcher
                .search(&query, &TopDocs::with_limit(10).order_by_score())
                .unwrap()
                .into_iter()
                .map(|(_, addr)| {
                    let d: TantivyDocument = searcher.doc(addr).unwrap();
                    d.get_first(idx).and_then(|v| v.as_u64()).unwrap() as usize
                })
                .collect()
        });
        println!(
            "{:<10} {:>10.1} {:>10.0} {:>9.1} {:>9.4} {:>9.0} {:>9.0}",
            "tantivy",
            secs,
            c.texts.len() as f64 / secs,
            dir_size(&dir) as f64 / 1e6,
            n,
            p50,
            p95
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
