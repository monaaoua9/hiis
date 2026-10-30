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
