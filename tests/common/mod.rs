#![allow(dead_code)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use hiis::index::merge::MergePolicy;
use hiis::{Config, Engine};

pub struct TmpDir(pub PathBuf);

impl TmpDir {
    pub fn new() -> TmpDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "hiis-test-{}-c{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn small_cfg() -> Config {
    Config {
        flush_docs: 3,
        background_merge: false,
        merge: MergePolicy { factor: 2, max_at_once: 8, floor_docs: 2, ..MergePolicy::default() },
        ..Config::default()
    }
}

pub const CORPUS: &[&str] = &[
    "The quick brown fox jumps over the lazy dog",
    "A fast brown dog outpaces a quick fox",
    "Inverted indexes map every term to the documents containing it",
    "Posting lists are compressed with frame of reference encoding",
    "BM25 ranks documents by term frequency and inverse document frequency",
    "Segment merging rewrites many small segments into one large segment",
    "The lazy cat sleeps all day while the dog barks",
    "Phrase queries need term positions stored in the posting lists",
];

pub fn loaded(cfg: Config) -> (TmpDir, Engine) {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, cfg).unwrap();
    for d in CORPUS {
        e.add_document(d).unwrap();
    }
    e.commit().unwrap();
    (dir, e)
}

pub fn ids(hits: &[hiis::Hit]) -> Vec<u64> {
    hits.iter().map(|h| h.doc_id).collect()
}

pub fn sorted_ids(hits: &[hiis::Hit]) -> Vec<u64> {
    let mut v = ids(hits);
    v.sort_unstable();
    v
}
