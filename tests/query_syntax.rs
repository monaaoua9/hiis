mod common;
use common::*;

#[test]
fn group_with_or_and_negation() {
    let (_d, e) = loaded(small_cfg());
    // docs with dog: 0, 1, 6. cat only in 6, fox in 0 and 1
    let hits = e.search("(fox OR cat) -lazy", 10).unwrap();
    assert_eq!(sorted_ids(&hits), [1]);

    let hits = e.search("+(fox cat) +lazy", 10).unwrap();
    assert_eq!(sorted_ids(&hits), [0, 6]);
}

#[test]
fn and_or_not_keywords() {
    let (_d, e) = loaded(small_cfg());
    assert_eq!(sorted_ids(&e.search("quick AND fox", 10).unwrap()), [0, 1]);
    assert_eq!(sorted_ids(&e.search("quick AND dog NOT lazy", 10).unwrap()), [1]);
    assert_eq!(sorted_ids(&e.search("cat AND NOT fox", 10).unwrap()), [6]);
    // OR changes nothing, bare terms are already optional
    assert_eq!(
        sorted_ids(&e.search("cat OR segment", 10).unwrap()),
        sorted_ids(&e.search("cat segment", 10).unwrap())
    );
}

#[test]
fn nested_groups_and_junk() {
    let (_d, e) = loaded(small_cfg());
    let hits = e.search("+((quick OR lazy) AND (fox OR cat))", 10).unwrap();
    assert_eq!(sorted_ids(&hits), [0, 1, 6]);

    // unbalanced input shouldn't panic or hang
    for q in ["(", ")", "((fox", "fox))", "+(", "-(()", "AND", "NOT NOT fox", "(fox OR"] {
        let _ = e.search(q, 5).unwrap();
    }
    let deep = format!("{}fox{}", "(".repeat(500), ")".repeat(500));
    let _ = e.search(&deep, 5).unwrap();
}

#[test]
fn grouped_query_scores_survive_merging() {
    let (_d, e) = loaded(small_cfg());
    let before = e.search("(quick OR lazy) AND dog", 10).unwrap();
    e.force_merge().unwrap();
    let after = e.search("(quick OR lazy) AND dog", 10).unwrap();
    assert_eq!(before, after);
}

#[test]
fn fuzzy_finds_typos() {
    let (_d, e) = loaded(small_cfg());
    // quick: docs 0, 1
    assert_eq!(sorted_ids(&e.search("quikc~", 10).unwrap()), [0, 1]);
    assert_eq!(sorted_ids(&e.search("qiuck~1", 10).unwrap()), [0, 1]);
    // two edits needs an explicit ask or a long enough word
    assert!(e.search("quack~0", 10).unwrap().is_empty());
    assert_eq!(sorted_ids(&e.search("compresed~", 10).unwrap()), [3]);
    // first letter has to be right
    assert!(e.search("kuick~2", 10).unwrap().is_empty());
}

#[test]
fn fuzzy_matches_exact_and_variant_in_one_doc() {
    let (_d, e) = loaded(small_cfg());
    e.add_document("segment segmant").unwrap();
    e.commit().unwrap();
    let hits = e.search("segment~1", 10).unwrap();
    assert!(hits.len() >= 2);
    assert!(hits.iter().any(|h| h.doc_id == 8));
}

#[test]
fn fuzzy_composes_with_other_clauses() {
    let (_d, e) = loaded(small_cfg());
    assert_eq!(sorted_ids(&e.search("+quikc~ +lazy", 10).unwrap()), [0]);
    assert_eq!(sorted_ids(&e.search("dog -quikc~", 10).unwrap()), [6]);
    assert_eq!(sorted_ids(&e.search("(posting~ OR frmae~) AND compresed~", 10).unwrap()), [3]);
}

#[test]
fn fuzzy_survives_merge() {
    let (_d, e) = loaded(small_cfg());
    let before = e.search("lazzy~ brwn~", 10).unwrap();
    assert!(!before.is_empty());
    e.force_merge().unwrap();
    assert_eq!(before, e.search("lazzy~ brwn~", 10).unwrap());
}

#[test]
fn proximity_queries() {
    let dir = TmpDir::new();
    let e = hiis::Engine::open(&dir.0, small_cfg()).unwrap();
    e.add_document("alpha beta gamma").unwrap(); // 0
    e.add_document("alpha x y beta").unwrap(); // 1
    e.add_document("beta alpha").unwrap(); // 2
    e.add_document("alpha x y z w beta").unwrap(); // 3
    e.commit().unwrap();

    assert_eq!(sorted_ids(&e.search("\"alpha beta\"~0", 10).unwrap()), [0]);
    assert_eq!(sorted_ids(&e.search("\"alpha beta\"~2", 10).unwrap()), [0, 1, 2]);
    assert_eq!(sorted_ids(&e.search("\"alpha beta\"~4", 10).unwrap()), [0, 1, 2, 3]);
    assert_eq!(sorted_ids(&e.search("\"alpha beta\"~1", 10).unwrap()), [0]);
    assert_eq!(sorted_ids(&e.search("+\"alpha beta\"~2 -gamma", 10).unwrap()), [1, 2]);
}

#[test]
fn proximity_ranks_tighter_matches_higher() {
    let dir = TmpDir::new();
    let e = hiis::Engine::open(&dir.0, small_cfg()).unwrap();
    e.add_document("red fox").unwrap();
    e.add_document("red x x fox").unwrap();
    e.add_document("filler words here to pad this one out a bit more").unwrap();
    e.commit().unwrap();
    let hits = e.search("\"red fox\"~3", 10).unwrap();
    assert_eq!(sorted_ids(&hits), [0, 1]);
    assert_eq!(hits[0].doc_id, 0);
}
