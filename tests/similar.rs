mod common;
use common::*;
use hiis::Engine;

fn topic_corpus(e: &Engine) {
    let docs = [
        "rust ownership borrowing lifetimes compiler memory safety",
        "borrow checker enforces rust ownership and lifetimes",
        "rust compiler errors about lifetimes and borrowing",
        "sourdough bread needs flour water salt and a long fermentation",
        "baking bread at home flour hydration and fermentation time",
        "bread crust flour oven steam baking",
        "the weather today is mild with some clouds",
    ];
    for d in docs {
        e.add_document(d).unwrap();
    }
    e.commit().unwrap();
}

#[test]
fn neighbours_come_from_the_same_topic() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    topic_corpus(&e);

    let rust = sorted_ids(&e.similar(0, 2).unwrap());
    assert_eq!(rust, [1, 2]);
    let bread = sorted_ids(&e.similar(3, 2).unwrap());
    assert_eq!(bread, [4, 5]);
}

#[test]
fn never_returns_itself_and_respects_k() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    topic_corpus(&e);
    for id in 0..7 {
        let got = e.similar(id, 3).unwrap();
        assert!(got.len() <= 3);
        assert!(got.iter().all(|h| h.doc_id != id));
    }
    assert_eq!(e.similar(0, 1).unwrap().len(), 1);
    assert!(e.similar(0, 0).unwrap().is_empty());
}

#[test]
fn unknown_and_deleted_documents_are_empty() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    topic_corpus(&e);
    assert!(e.similar(500, 5).unwrap().is_empty());
    e.delete(1).unwrap();
    assert!(e.similar(1, 5).unwrap().is_empty());
    // and the deleted one isn't offered as a neighbour
    assert!(e.similar(0, 5).unwrap().iter().all(|h| h.doc_id != 1));
}

#[test]
fn unique_terms_alone_still_gives_a_query() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    e.add_document("zzyzx qwertyuiop").unwrap();
    e.add_document("something else entirely").unwrap();
    e.commit().unwrap();
    // every term is unique to doc 0: no neighbours, but no panic either
    assert!(e.similar(0, 5).unwrap().is_empty());
}
