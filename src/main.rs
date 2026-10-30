use std::fs;
use std::io::{BufWriter, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use hiis::json::Json;
use hiis::server::{Server, ServerConfig};
use hiis::{Config, Engine, HighlightOpts, Result};

const USAGE: &str = "\
usage:
  hiis index  <dir> <file>        one document per non-empty line
  hiis search <dir> <query> [-k N]
  hiis similar <dir> <doc-id> [-k N]
  hiis explain <dir> <query> <doc-id>
  hiis complete <dir> <prefix> [-k N]
  hiis get    <dir> <doc-id>
  hiis delete <dir> <doc-id>
  hiis merge  <dir>               merge all segments into one
  hiis verify <dir> [--deep]      check posting lists (and stored text with --deep)
  hiis backup <dir> <dest>        consistent copy of the committed index
  hiis serve  <dir> [--addr HOST:PORT] [--max-conns N] [--commit-ms N] [--token T] [--read-only]
                                  HTTP API; the token can also come from HIIS_TOKEN
  hiis export <dir> [file]        live documents as JSON lines {id, text} (stdout by default)
  hiis load   <dir> <file>        add documents from JSON lines (each needs a text field)
  hiis reindex <dir> <dest>       rebuild into a fresh index with the current analyzer (ids restart at 0)
  hiis terms  <dir> [-k N]        most common indexed terms
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
    let read_only = cmd == "serve" && rest.iter().any(|a| a == "--read-only");
    let engine = Engine::open(dir, Config { read_only, ..Config::default() })?;

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
        ("similar", [id, opts @ ..]) => {
            let id = id.parse().map_err(|_| bad_usage())?;
            let k = match opts {
                [] => 5,
                [flag, n] if flag == "-k" => n.parse().map_err(|_| bad_usage())?,
                _ => return Err(bad_usage()),
            };
            for h in engine.similar(id, k)? {
                let text = engine.get(h.doc_id).unwrap_or_default();
                let head: String = text.chars().take(100).collect();
                println!("{:>7.3}  #{:<6} {}", h.score, h.doc_id, head.replace('\n', " "));
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
        ("verify", opts) => {
            let deep = match opts {
                [] => false,
                [flag] if flag == "--deep" => true,
                _ => return Err(bad_usage()),
            };
            let rep = engine.verify(deep);
            println!("{rep}");
            if !rep.is_ok() {
                return Err(hiis::Error::Corrupt("verify found problems"));
            }
        }
        ("backup", [dest]) => {
            engine.backup(dest)?;
            println!("backed up to {dest}");
        }
        ("serve", opts) => {
            let mut cfg = ServerConfig { token: std::env::var("HIIS_TOKEN").ok(), ..Default::default() };
            let mut it = opts.iter();
            while let Some(flag) = it.next() {
                if flag == "--read-only" {
                    continue;
                }
                let val = it.next().ok_or_else(bad_usage)?;
                match flag.as_str() {
                    "--addr" => cfg.addr = val.clone(),
                    "--token" => cfg.token = Some(val.clone()),
                    "--max-conns" => cfg.max_connections = val.parse().map_err(|_| bad_usage())?,
                    "--commit-ms" => {
                        let ms: u64 = val.parse().map_err(|_| bad_usage())?;
                        cfg.commit_interval = (ms > 0).then(|| Duration::from_millis(ms));
                    }
                    _ => return Err(bad_usage()),
                }
            }
            let server = Server::start(Arc::new(engine), cfg)?;
            eprintln!("listening on http://{}", server.local_addr());
            server.wait();
        }
        ("export", opts) => {
            let mut out: Box<dyn Write> = match opts {
                [] => Box::new(BufWriter::new(std::io::stdout().lock())),
                [file] => Box::new(BufWriter::new(fs::File::create(file)?)),
                _ => return Err(bad_usage()),
            };
            engine.for_each_record(|id, fields, text| {
                let mut line = vec![("id", Json::from(id)), ("text", text.into())];
                if let Some(f) = fields {
                    line.push(("fields", hiis::fields::to_json(f)));
                }
                let line = Json::Obj(line.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
                writeln!(out, "{line}")?;
                Ok(())
            })?;
            out.flush()?;
        }
        ("load", [file]) => {
            let raw = fs::read_to_string(file)?;
            let t = Instant::now();
            let mut batch: Vec<String> = Vec::new();
            let mut n = 0usize;
            for (i, line) in raw.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
                let doc =
                    Json::parse(line).map_err(|e| hiis::Error::Invalid(format!("line {}: {e}", i + 1)))?;
                if let Some(f) = doc.get("fields") {
                    // keep the order of the file: plain documents queued so far go in first
                    engine.add_documents(&batch)?;
                    n += batch.len();
                    batch.clear();
                    let pairs = hiis::fields::from_json(f)?;
                    let refs: Vec<(&str, &str)> =
                        pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    engine.add_fields(&refs)?;
                    n += 1;
                    continue;
                }
                let Some(text) = doc.get("text").and_then(Json::as_str) else {
                    return Err(hiis::Error::Invalid(format!("line {}: no \"text\" string", i + 1)));
                };
                batch.push(text.to_string());
                if batch.len() == 256 {
                    engine.add_documents(&batch)?;
                    n += batch.len();
                    batch.clear();
                }
            }
            engine.add_documents(&batch)?;
            engine.commit()?;
            println!("loaded {} docs in {:.2}s", n + batch.len(), t.elapsed().as_secs_f64());
        }
        ("reindex", [dest]) => {
            let target = Engine::open(dest, Config::default())?;
            if target.stats().live_docs > 0 {
                return Err(hiis::Error::Invalid(format!("{dest} already holds documents")));
            }
            let mut batch: Vec<String> = Vec::new();
            let mut n = 0usize;
            engine.for_each_record(|_, fields, text| {
                if let Some(f) = fields {
                    target.add_documents(&batch)?;
                    n += batch.len();
                    batch.clear();
                    let refs: Vec<(&str, &str)> = f.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    target.add_fields(&refs)?;
                    n += 1;
                    return Ok(());
                }
                batch.push(text.to_string());
                if batch.len() == 256 {
                    target.add_documents(&batch)?;
                    n += batch.len();
                    batch.clear();
                }
                Ok(())
            })?;
            target.add_documents(&batch)?;
            target.commit()?;
            println!("reindexed {} docs into {dest}", n + batch.len());
        }
        ("terms", opts) => {
            let k = match opts {
                [] => 20,
                [flag, n] if flag == "-k" => n.parse().map_err(|_| bad_usage())?,
                _ => return Err(bad_usage()),
            };
            for (term, df) in engine.top_terms(k) {
                println!("{df:>9}  {term}");
            }
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
