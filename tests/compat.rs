// indexes written by older releases must keep opening; the fixtures were built from
// data/sample.txt by the commit before format v5 (v4) and by the current writer (v5).
// The last two sample lines are Arabic and the v4 writer predates Arabic stopwords, so
// those two documents carry one more token there than the current analyzer gives them:
// scores can differ slightly and `verify` flags exactly those two until a reindex.
mod common;
use std::path::Path;

use common::*;
use hiis::Engine;

fn copy_fixture(version: &str) -> TmpDir {
    let dst = TmpDir::new();
    std::fs::create_dir_all(&dst.0).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(version);
    for f in std::fs::read_dir(src).unwrap().flatten() {
        std::fs::copy(f.path(), dst.0.join(f.file_name())).unwrap();
    }
    dst
}

fn segment_versions(dir: &Path) -> Vec<u32> {
    let mut v: Vec<u32> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|f| f.file_name().to_string_lossy().starts_with("seg_"))
        .map(|f| {
            let b = std::fs::read(f.path()).unwrap();
            u32::from_le_bytes(b[4..8].try_into().unwrap())
        })
        .collect();
    v.sort_unstable();
    v
}

#[test]
fn old_and_new_fixtures_answer_the_same() {
    let (d4, d5) = (copy_fixture("v4"), copy_fixture("v5"));
    assert_eq!(segment_versions(&d4.0), [4]);
    assert_eq!(segment_versions(&d5.0), [5]);

    let (e4, e5) = (Engine::open(&d4.0, small_cfg()).unwrap(), Engine::open(&d5.0, small_cfg()).unwrap());
    assert_eq!(e4.stats().live_docs, 12);
    for q in ["fox", "quick fox", "\"brown dog\"", "+fox -lazy", "fox*", "quik~1", "(fox OR dog) AND quick"] {
        let (a, b) = (e4.search(q, 10).unwrap(), e5.search(q, 10).unwrap());
        assert!(!a.is_empty(), "{q}");
        let ids = |h: &[hiis::Hit]| h.iter().map(|x| x.doc_id).collect::<Vec<_>>();
        assert_eq!(ids(&a), ids(&b), "{q}");
    }
    assert_eq!(e4.get(0).as_deref(), Some("The quick brown fox jumps over the lazy dog"));
    assert!(e5.verify(true).is_ok(), "{}", e5.verify(true));

    let report = e4.verify(true).to_string();
    assert!(report.contains("doc 10") && report.contains("doc 11"), "{report}");
    assert_eq!(report.matches("PROBLEM").count(), 2, "{report}");
}

#[test]
fn reindexing_a_v4_index_makes_it_verify_clean() {
    let old = Engine::open(&copy_fixture("v4").0, small_cfg()).unwrap();
    let fresh_dir = TmpDir::new();
    let fresh = Engine::open(&fresh_dir.0, small_cfg()).unwrap();
    old.for_each_document(|_, text| fresh.add_document(text).map(|_| ())).unwrap();
    fresh.commit().unwrap();
    assert_eq!(fresh.stats().live_docs, 12);
    assert!(fresh.verify(true).is_ok(), "{}", fresh.verify(true));
}

#[test]
fn a_v4_index_is_upgraded_by_a_merge() {
    let dir = copy_fixture("v4");
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    let before = e.search("quick fox", 10).unwrap();

    e.add_document("one more fox for the road").unwrap();
    e.commit().unwrap();
    e.force_merge().unwrap();

    assert_eq!(e.stats().live_docs, 13);
    assert_eq!(e.search("fox", 20).unwrap().len(), e.count("fox").unwrap() as usize);
    for h in &before {
        assert!(e.search("quick fox", 20).unwrap().iter().any(|x| x.doc_id == h.doc_id));
    }
    assert_eq!(segment_versions(&dir.0), [5]);
}
