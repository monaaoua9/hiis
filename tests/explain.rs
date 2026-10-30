mod common;
use common::*;

#[test]
fn explanation_total_equals_search_score() {
    let (_d, e) = loaded(small_cfg());
    for q in [
        "quick fox",
        "+quick -cat dog",
        "\"brown fox\"",
        "(lazy OR fast) dog^2",
        "quikc~ compres*",
        "\"quick fox\"~3 lazy",
    ] {
        let hits = e.search(q, 10).unwrap();
        assert!(!hits.is_empty(), "{q}");
        for h in &hits {
            let x = e.explain(q, h.doc_id).unwrap().unwrap();
            assert!(x.matched, "{q} doc {}", h.doc_id);
            assert_eq!(x.score, h.score, "{q} doc {}", h.doc_id);
        }
    }
}

#[test]
fn explains_why_a_document_is_missing() {
    let (_d, e) = loaded(small_cfg());
    // doc 6 has dog and lazy; -lazy knocks it out
    let x = e.explain("dog -lazy", 6).unwrap().unwrap();
    assert!(!x.matched);
    let lazy = x.clauses.iter().find(|c| c.label == "lazi" || c.label == "lazy").unwrap();
    assert!(lazy.score.is_some());
    assert_eq!(lazy.occur, hiis::search::query::Occur::MustNot);

    // doc 3 doesn't mention dog at all
    let x = e.explain("+dog", 3).unwrap().unwrap();
    assert!(!x.matched && x.clauses[0].score.is_none());
}

#[test]
fn term_note_carries_the_bm25_inputs() {
    let (_d, e) = loaded(small_cfg());
    let x = e.explain("segment", 5).unwrap().unwrap();
    let c = &x.clauses[0];
    assert!(c.note.contains("tf 3"), "{}", c.note);
    assert!(c.note.contains("idf"));
    let text = x.to_string();
    assert!(text.starts_with("doc 5 scores"), "{text}");
}

#[test]
fn groups_and_boosts_show_up() {
    let (_d, e) = loaded(small_cfg());
    let x = e.explain("(quick OR lazy)^2 fox", 0).unwrap().unwrap();
    assert_eq!(x.clauses[0].boost, 2.0);
    assert_eq!(x.clauses[0].children.len(), 2);
    assert!(x.clauses[0].children.iter().all(|c| c.score.is_some()));
    let inner: f32 = x.clauses[0].children.iter().map(|c| c.score.unwrap()).sum();
    assert!((x.clauses[0].score.unwrap() - inner * 2.0).abs() < 1e-4);
}

#[test]
fn missing_and_deleted_docs() {
    let (_d, e) = loaded(small_cfg());
    assert!(e.explain("fox", 999).unwrap().is_none());
    e.delete(0).unwrap();
    assert!(e.explain("fox", 0).unwrap().is_none());
    assert!(!e.explain("", 1).unwrap().unwrap().matched);
}
