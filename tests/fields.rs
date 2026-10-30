mod common;
use common::*;
use hiis::{Config, Engine, Error};

fn books(e: &Engine) {
    let docs: [&[(&str, &str)]; 5] = [
        &[("title", "Rust in Action"), ("body", "systems programming with ownership and borrowing")],
        &[("title", "The Go Programming Language"), ("body", "a book about rust and go concurrency")],
        &[("title", "Baking Bread"), ("body", "flour water salt and patience")],
        &[("title", "Rust Cookbook"), ("body", "recipes for programming tasks")],
        &[("title", "Gardening"), ("body", "soil and sunlight")],
    ];
    for d in docs {
        e.add_fields(d).unwrap();
    }
    e.commit().unwrap();
}

#[test]
fn field_queries_see_only_that_field() {
    let dir = TmpDir::new();
    let e2 = Engine::open(&dir.0, small_cfg()).unwrap();
    books(&e2);

    // "rust" is in the title of 0 and 3, in the body of 1 only
    assert_eq!(sorted_ids(&e2.search("title:rust", 10).unwrap()), [0, 3]);
    assert_eq!(sorted_ids(&e2.search("body:rust", 10).unwrap()), [1]);
    // no field: all of them
    assert_eq!(sorted_ids(&e2.search("rust", 10).unwrap()), [0, 1, 3]);
    assert_eq!(sorted_ids(&e2.search("title:rust body:go", 10).unwrap()), [0, 1, 3]);
    assert_eq!(sorted_ids(&e2.search("+title:rust +body:programming", 10).unwrap()), [0, 3]);
    assert_eq!(sorted_ids(&e2.search("+title:rust -body:ownership", 10).unwrap()), [3]);
}

#[test]
fn phrases_prefixes_fuzzy_and_groups_take_a_field() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    books(&e);
    assert_eq!(sorted_ids(&e.search("title:\"rust in action\"", 10).unwrap()), [0]);
    assert_eq!(sorted_ids(&e.search("title:\"programming language\"", 10).unwrap()), [1]);
    assert_eq!(sorted_ids(&e.search("title:progr*", 10).unwrap()), [1]);
    assert_eq!(sorted_ids(&e.search("body:progr*", 10).unwrap()), [0, 3]);
    assert_eq!(sorted_ids(&e.search("title:bakng~2", 10).unwrap()), [2]);
    assert_eq!(sorted_ids(&e.search("title:(bread OR garden)", 10).unwrap()), [2, 4]);
    // plain prefix and fuzzy queries don't pick up the field postings
    assert_eq!(sorted_ids(&e.search("rus*", 10).unwrap()), [0, 1, 3]);
    assert_eq!(e.count("title:rust").unwrap(), 2);
}

#[test]
fn phrases_do_not_cross_a_field_boundary() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    // "action" ends the title, "systems" starts the body
    e.add_fields(&[("title", "Rust in Action"), ("body", "systems programming")]).unwrap();
    e.commit().unwrap();
    assert!(e.search("\"action systems\"", 5).unwrap().is_empty());
    assert!(e.search("\"action systems\"~10", 5).unwrap().is_empty());
    assert_eq!(e.search("\"systems programming\"", 5).unwrap().len(), 1);
}

#[test]
fn field_boost_changes_the_order() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    e.add_fields(&[("title", "garden"), ("body", "tools for the shed")]).unwrap(); // 0
    e.add_fields(&[("title", "tools"), ("body", "a garden journal")]).unwrap(); // 1
    e.commit().unwrap();
    assert_eq!(e.search("title:garden^10 title:tools", 2).unwrap()[0].doc_id, 0);
    assert_eq!(e.search("title:garden title:tools^10", 2).unwrap()[0].doc_id, 1);
}

#[test]
fn readers_get_plain_text_and_pairs() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    let a = e.add_fields(&[("title", "Hello"), ("body", "line one\nline two")]).unwrap();
    let b = e.add_document("plain text").unwrap();
    e.commit().unwrap();

    assert_eq!(e.get(a).as_deref(), Some("Hello\nline one\nline two"));
    assert_eq!(
        e.get_fields(a).unwrap(),
        [("title".to_string(), "Hello".to_string()), ("body".to_string(), "line one\nline two".to_string())]
    );
    assert!(e.get_fields(b).is_none());
    assert!(e.get_fields(999).is_none());

    let s = e.highlight(a, "title:hello", &hiis::HighlightOpts::default()).unwrap();
    assert!(s.text.starts_with("<em>Hello</em>"), "{}", s.text);
    // autocomplete and top terms never offer field postings
    assert!(e.complete("tit", 5).is_empty());
    assert!(e.top_terms(50).iter().all(|(t, _)| !t.contains(':')));
}

#[test]
fn bad_input_is_refused() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    for bad in [&[][..], &[("Title", "x")], &[("a b", "x")], &[("t", "x\u{1f}y")], &[("t", "x\u{1e}y")]] {
        assert!(matches!(e.add_fields(bad), Err(Error::Invalid(_))), "{bad:?}");
    }
    assert!(matches!(e.add_document("sneaky \u{1e}title\u{1f}x"), Err(Error::Invalid(_))));
    assert!(matches!(e.add_documents(&["ok", "\u{1e}no"]), Err(Error::Invalid(_))));
    assert_eq!(e.stats().buffered_docs, 0);
}

#[test]
fn survives_merge_reopen_and_wal_replay() {
    let dir = TmpDir::new();
    let want;
    {
        let e = Engine::open(&dir.0, small_cfg()).unwrap();
        books(&e);
        e.delete(4).unwrap();
        want = e.search("title:rust body:go", 10).unwrap();
        e.force_merge().unwrap();
        assert_eq!(e.search("title:rust body:go", 10).unwrap(), want);
        // one in the log only: fielded replay has to rebuild the field postings
        e.add_fields(&[("title", "Rust Embedded"), ("body", "microcontrollers")]).unwrap();
        std::mem::forget(e);
    }
    let copy = TmpDir::new();
    std::fs::create_dir_all(&copy.0).unwrap();
    for f in std::fs::read_dir(&dir.0).unwrap().flatten() {
        if f.file_name() != "LOCK" {
            std::fs::copy(f.path(), copy.0.join(f.file_name())).unwrap();
        }
    }
    let e = Engine::open(&copy.0, small_cfg()).unwrap();
    e.commit().unwrap();
    assert_eq!(sorted_ids(&e.search("title:rust", 10).unwrap()), [0, 3, 5]);
    assert!(e.verify(true).is_ok(), "{}", e.verify(true));
}

#[test]
fn verify_and_pruning_hold_with_fields() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, Config { flush_docs: 7, ..small_cfg() }).unwrap();
    for i in 0..60 {
        let t = format!("alpha{} beta", i % 3);
        let b = format!("gamma{} beta delta", i % 5);
        e.add_fields(&[("title", &t), ("body", &b)]).unwrap();
    }
    e.commit().unwrap();
    assert!(e.verify(true).is_ok());
    for q in ["beta", "title:alpha1 body:gamma2", "title:beta^3 delta", "+body:delta title:alpha0"] {
        assert_eq!(e.search(q, 10).unwrap(), e.search_unpruned(q, 10).unwrap(), "{q}");
    }
    e.force_merge().unwrap();
    assert!(e.verify(true).is_ok());
}
