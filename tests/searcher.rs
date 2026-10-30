mod common;
use common::*;
use hiis::Engine;

#[test]
fn view_ignores_later_commits_and_merges() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    for d in CORPUS {
        e.add_document(d).unwrap();
    }
    e.commit().unwrap();

    let s = e.searcher();
    let first = s.search_page("dog fox", 2, None).unwrap();
    for i in 0..30 {
        e.add_document(&format!("dog fox extra {i}")).unwrap();
    }
    e.commit().unwrap();
    e.force_merge().unwrap();

    let second = s.search_page("dog fox", 2, first.next).unwrap();
    let full = s.search("dog fox", 10).unwrap();
    let mut joined = first.hits.clone();
    joined.extend(second.hits);
    assert_eq!(joined, full[..joined.len()]);
    assert_eq!(s.count("dog").unwrap(), 3);
    assert_eq!(s.get(0).as_deref(), Some(CORPUS[0]));
    // the live engine sees the new documents
    assert_eq!(e.count("dog").unwrap(), 33);
}
