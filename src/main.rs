use std::fs;
use std::process::ExitCode;
use std::time::Instant;

use hiis::{Config, Engine, HighlightOpts, Result};

const USAGE: &str = "\
usage:
  hiis index  <dir> <file>        one document per non-empty line
  hiis search <dir> <query> [-k N]
  hiis explain <dir> <query> <doc-id>
  hiis complete <dir> <prefix> [-k N]
  hiis get    <dir> <doc-id>
  hiis delete <dir> <doc-id>
  hiis merge  <dir>               merge all segments into one
  hiis stats  <dir>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn bad_usage() -> hiis::Error {
    hiis::Error::Invalid(USAGE.to_string())
}

fn run(args: &[String]) -> Result<()> {
    let [cmd, dir, rest @ ..] = args else { return Err(bad_usage()) };
    let engine = Engine::open(dir, Config::default())?;

    match (cmd.as_str(), rest) {
        ("index", [file]) => {
            let text = fs::read_to_string(file)?;
            let t = Instant::now();
            let mut n = 0usize;
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            for chunk in lines.chunks(256) {
                engine.add_documents(chunk)?;
                n += chunk.len();
            }
            engine.commit()?;
            let secs = t.elapsed().as_secs_f64();
            println!("indexed {n} docs in {secs:.2}s ({:.0} docs/s)", n as f64 / secs.max(1e-9));
        }
        ("search", [query, opts @ ..]) => {
            let k = match opts {
                [] => 10,
                [flag, n] if flag == "-k" => n.parse().map_err(|_| bad_usage())?,
                _ => return Err(bad_usage()),
            };
            let t = Instant::now();
            let hits = engine.search(query, k)?;
            let us = t.elapsed().as_micros();
            // matched words in bold (ansi)
            let opts = HighlightOpts {
                pre: "\x1b[1m".into(),
                post: "\x1b[0m".into(),
                max_chars: 110,
                escape_html: false,
            };
            for (rank, h) in hits.iter().enumerate() {
                let snip = engine.highlight(h.doc_id, query, &opts).map(|s| s.text).unwrap_or_default();
                println!("{:>2}. {:>7.3}  #{:<6} {}", rank + 1, h.score, h.doc_id, snip.replace('\n', " "));
            }
            println!("-- {} hits in {us}us", hits.len());
            if hits.is_empty() {
                if let Some(q) = engine.did_you_mean(query) {
                    println!("did you mean: {q}");
                }
            }
        }
        ("explain", [query, id]) => {
            let id = id.parse().map_err(|_| bad_usage())?;
            match engine.explain(query, id)? {
                Some(x) => print!("{x}"),
                None => println!("no such document"),
            }
        }
        ("complete", [prefix, opts @ ..]) => {
            let k = match opts {
                [] => 10,
                [flag, n] if flag == "-k" => n.parse().map_err(|_| bad_usage())?,
                _ => return Err(bad_usage()),
            };
            for s in engine.complete(prefix, k) {
                println!("{:<24} {}", s.text, s.doc_freq);
            }
        }
        ("get", [id]) => {
            let id = id.parse().map_err(|_| bad_usage())?;
            match engine.get(id) {
                Some(t) => println!("{t}"),
                None => println!("no such document"),
            }
        }
        ("delete", [id]) => {
            let id = id.parse().map_err(|_| bad_usage())?;
            let gone = engine.delete(id)?;
            engine.commit()?;
            println!("{}", if gone { "deleted" } else { "not found" });
        }
        ("merge", []) => {
            engine.force_merge()?;
            println!("{:?}", engine.stats());
        }
        ("stats", []) => {
            let s = engine.stats();
            println!(
                "segments {}  live {}  deleted {}  terms(sum) {}  disk {} bytes",
                s.segments, s.live_docs, s.deleted_docs, s.terms, s.disk_bytes
            );
        }
        _ => return Err(bad_usage()),
    }
    Ok(())
}
