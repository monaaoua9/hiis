mod common;
use std::process::Command;

use common::*;
use hiis::json::Json;
use hiis::{Config, Engine};

fn collect(e: &Engine) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    e.for_each_document(|id, t| {
        out.push((id, t.to_string()));
        Ok(())
    })
    .unwrap();
    out
}

#[test]
fn walks_live_documents_in_id_order() {
    let (_d, e) = loaded(small_cfg());
    e.delete(2).unwrap();
    e.delete(5).unwrap();
    // merging shuffles segment order, the walk must not care
    e.force_merge().unwrap();
    e.add_document("late arrival").unwrap();
    e.commit().unwrap();

    let docs = collect(&e);
    let ids: Vec<u64> = docs.iter().map(|d| d.0).collect();
    assert_eq!(ids, [0, 1, 3, 4, 6, 7, 8]);
    assert_eq!(docs[0].1, CORPUS[0]);
    assert_eq!(docs.last().unwrap().1, "late arrival");
}

#[test]
fn buffered_documents_are_not_part_of_the_walk() {
    let (_d, e) = loaded(small_cfg());
    e.add_document("still in the buffer").unwrap();
    assert_eq!(collect(&e).len(), CORPUS.len());
}

#[test]
fn callback_error_stops_the_walk() {
    let (_d, e) = loaded(small_cfg());
    let mut seen = 0;
    let r = e.for_each_document(|_, _| {
        seen += 1;
        if seen == 3 {
            return Err(hiis::Error::Invalid("enough".into()));
        }
        Ok(())
    });
    assert!(r.is_err());
    assert_eq!(seen, 3);
}

fn hiis(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_hiis")).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn export_load_roundtrip_through_the_cli() {
    let src = TmpDir::new();
    {
        let e = Engine::open(&src.0, small_cfg()).unwrap();
        for d in CORPUS {
            e.add_document(d).unwrap();
        }
        e.add_document("tricky \"quotes\"\nand a newline, plus مرحبا").unwrap();
        e.commit().unwrap();
        e.delete(1).unwrap();
        e.commit().unwrap();
    }
    let file = std::env::temp_dir().join(format!("hiis-export-{}.jsonl", std::process::id()));
    let (ok, _) = hiis(&["export", src.0.to_str().unwrap(), file.to_str().unwrap()]);
    assert!(ok);

    let text = std::fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), CORPUS.len()); // 9 added, 1 deleted
    let first = Json::parse(lines[0]).unwrap();
    assert_eq!(first.get("text").unwrap().as_str(), Some(CORPUS[0]));
    let last = Json::parse(lines.last().unwrap()).unwrap();
    assert_eq!(last.get("text").unwrap().as_str(), Some("tricky \"quotes\"\nand a newline, plus مرحبا"));

    let dst = TmpDir::new();
    let (ok, out) = hiis(&["load", dst.0.to_str().unwrap(), file.to_str().unwrap()]);
    assert!(ok, "{out}");
    let e = Engine::open(&dst.0, small_cfg()).unwrap();
    assert_eq!(e.stats().live_docs as usize, CORPUS.len());
    assert_eq!(e.search("مرحبا", 5).unwrap().len(), 1);
    std::fs::remove_file(&file).unwrap();
}

#[test]
fn load_rejects_bad_lines() {
    let dst = TmpDir::new();
    let file = std::env::temp_dir().join(format!("hiis-badload-{}.jsonl", std::process::id()));
    std::fs::write(&file, "{\"text\": \"fine\"}\n{\"nope\": 1}\n").unwrap();
    let (ok, _) = hiis(&["load", dst.0.to_str().unwrap(), file.to_str().unwrap()]);
    assert!(!ok);
    std::fs::remove_file(&file).unwrap();
}

#[test]
fn reindex_applies_the_current_analyzer() {
    let src = TmpDir::new();
    {
        // an index built without stemming, e.g. before an analyzer change
        let cfg = Config {
            analyzer: hiis::analysis::AnalyzerConfig { stem: false, ..Default::default() },
            ..small_cfg()
        };
        let e = Engine::open(&src.0, cfg).unwrap();
        e.add_document("running dogs").unwrap();
        e.add_document("the dog ran away").unwrap();
        e.commit().unwrap();
        // no stemming: "run" finds nothing
        assert!(e.search("run", 5).unwrap().is_empty());
    }

    let dst = TmpDir::new();
    let (ok, out) = hiis(&["reindex", src.0.to_str().unwrap(), dst.0.to_str().unwrap()]);
    assert!(ok, "{out}");
    let e = Engine::open(&dst.0, Config::default()).unwrap();
    assert_eq!(e.search("run", 5).unwrap().len(), 1);
    assert_eq!(e.search("dogs", 5).unwrap().len(), 2);

    drop(e); // the lock, or the second run would fail for the wrong reason
             // refuses to pour into an index that already has documents
    let (ok, _) = hiis(&["reindex", src.0.to_str().unwrap(), dst.0.to_str().unwrap()]);
    assert!(!ok);
}

#[test]
fn fields_survive_export_load_and_reindex() {
    let src = TmpDir::new();
    {
        let e = Engine::open(&src.0, small_cfg()).unwrap();
        e.add_document("plain one").unwrap();
        e.add_fields(&[("title", "Rust Book"), ("body", "ownership")]).unwrap();
        e.add_document("plain two").unwrap();
        e.commit().unwrap();
    }
    let file = std::env::temp_dir().join(format!("hiis-fields-{}.jsonl", std::process::id()));
    assert!(hiis(&["export", src.0.to_str().unwrap(), file.to_str().unwrap()]).0);
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), 3);
    assert!(text.lines().nth(1).unwrap().contains("\"fields\":{\"title\":\"Rust Book\""));

    let a = TmpDir::new();
    assert!(hiis(&["load", a.0.to_str().unwrap(), file.to_str().unwrap()]).0);
    let b = TmpDir::new();
    assert!(hiis(&["reindex", src.0.to_str().unwrap(), b.0.to_str().unwrap()]).0);
    for dir in [&a, &b] {
        let e = Engine::open(&dir.0, small_cfg()).unwrap();
        assert_eq!(e.stats().live_docs, 3);
        assert_eq!(sorted_ids(&e.search("title:rust", 5).unwrap()), [1]);
        assert!(e.search("title:plain", 5).unwrap().is_empty());
        assert_eq!(e.get_fields(1).unwrap()[1].1, "ownership");
        assert_eq!(e.get(0).as_deref(), Some("plain one"));
    }
    std::fs::remove_file(&file).unwrap();
}
