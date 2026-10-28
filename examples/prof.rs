use std::time::Instant;

use hiis::{Config, Engine};

fn field(line: &str, key: &str) -> String {
    let pat = format!("\"{key}\": \"");
    let Some(i) = line.find(&pat) else { return String::new() };
    let rest = &line[i + pat.len()..];
    let mut end = 0;
    let b = rest.as_bytes();
    while end < b.len() && b[end] != b'"' {
        end += if b[end] == b'\\' { 2 } else { 1 };
    }
    rest[..end.min(rest.len())].replace("\\n", " ").replace("\\\"", "\"")
}

fn main() -> hiis::Result<()> {
    let root = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let dir = std::path::PathBuf::from("D:/hiis-covid-idx");
    let fresh = !dir.exists();
    let e = Engine::open(&dir, Config::default())?;
    if fresh {
        let text = std::fs::read_to_string(root.join("corpus.jsonl")).unwrap();
        let docs: Vec<String> =
            text.lines().map(|l| format!("{}. {}", field(l, "title"), field(l, "text"))).collect();
        for c in docs.chunks(2048) {
            e.add_documents(c)?;
        }
        e.commit()?;
        e.wait_for_merges();
        e.force_merge()?;
    }
    println!("{:?}", e.stats());

    let qs = std::fs::read_to_string(root.join("queries.jsonl")).unwrap();
    let (mut tp, mut tu) = (0.0, 0.0);
    for l in qs.lines() {
        let q: String = field(l, "text").chars().filter(|c| !"\"+-*:()^~".contains(*c)).collect();
        let mut best = [f64::MAX; 2];
        for (slot, pruned) in [true, false].into_iter().enumerate() {
            for _ in 0..5 {
                let t = Instant::now();
                let _ = if pruned { e.search(&q, 10)? } else { e.search_unpruned(&q, 10)? };
                best[slot] = best[slot].min(t.elapsed().as_micros() as f64);
            }
        }
        tp += best[0];
        tu += best[1];
        println!("{:>7.0} {:>7.0}  {q}", best[0], best[1]);
    }
    println!("total pruned {tp:.0} us, exhaustive {tu:.0} us");
    Ok(())
}
