// Same index twice, once resident and once with every segment read from disk
// per query (resident_limit = 0). Results have to be identical, through
// flushes, streamed merges, deletes and a reopen.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use hiis::index::merge::MergePolicy;
use hiis::{Config, Engine};

struct TmpDir(PathBuf);

impl TmpDir {
    fn new() -> TmpDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "hiis-disk-{}-{}",
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

fn cfg(resident_limit: u64) -> Config {
    Config {
        flush_docs: 40,
        background_merge: false,
        merge: MergePolicy { factor: 3, max_at_once: 8, floor_docs: 20, ..MergePolicy::default() },
        resident_limit,
        ..Config::default()
    }
}

fn docs() -> Vec<String> {
    let words =
        ["alpha", "beta", "gamma", "delta", "index", "segment", "merge", "posting", "phrase", "query"];
    (0..300u32)
        .map(|i| {
            let n = 5 + i % 9;
            (0..n).map(|k| words[((i * 7 + k * 3 + k * k) % 10) as usize]).collect::<Vec<_>>().join(" ")
                + &format!(" doc{i} \u{0639}\u{0631}\u{0628}\u{064A}")
        })
        .collect()
}

const QUERIES: &[&str] = &[
    "alpha",
    "alpha beta gamma",
    "\"index segment\"",
    "+merge -posting",
    "delta*",
    "query phrase index",
    "\u{0639}\u{0631}\u{0628}\u{064A}",
];

fn run(e: &Engine) -> Vec<Vec<hiis::Hit>> {
    QUERIES.iter().map(|q| e.search(q, 15).unwrap()).collect()
}

#[test]
fn disk_backed_matches_resident() {
    let (a_dir, b_dir) = (TmpDir::new(), TmpDir::new());
    let a = Engine::open(&a_dir.0, cfg(u64::MAX)).unwrap();
    let b = Engine::open(&b_dir.0, cfg(0)).unwrap();
    let docs = docs();
    for part in docs.chunks(40) {
        a.add_documents(part).unwrap();
        b.add_documents(part).unwrap();
    }
    a.commit().unwrap();
    b.commit().unwrap();
    assert!(a.stats().segments > 1);
    assert_eq!(run(&a), run(&b));

    for id in [3u64, 77, 150, 299] {
        assert_eq!(a.delete(id).unwrap(), b.delete(id).unwrap());
    }
    assert_eq!(run(&a), run(&b));

    a.force_merge().unwrap();
    b.force_merge().unwrap();
    assert_eq!(b.stats().segments, 1);
    assert_eq!(run(&a), run(&b));
    assert_eq!(a.get(10), b.get(10));
    assert_eq!(b.get(10).as_deref(), Some(docs[10].as_str()));
    assert!(b.get(77).is_none());

    b.commit().unwrap();
    drop(b);
    let b = Engine::open(&b_dir.0, cfg(0)).unwrap();
    assert_eq!(run(&a), run(&b));
    assert_eq!(b.get(299), None);
}
