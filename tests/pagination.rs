mod common;
use common::*;
use hiis::{Config, Engine};

fn corpus(e: &Engine, n: usize) {
    let words = ["red", "green", "blue", "cyan", "pink", "gold", "gray"];
    let mut x = 11u32;
    for _ in 0..n {
        let mut doc = String::new();
        for _ in 0..(2 + x % 9) {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            doc.push_str(words[(x >> 16) as usize % words.len()]);
            doc.push(' ');
        }
        e.add_document(&doc).unwrap();
    }
    e.commit().unwrap();
}

fn walk(e: &Engine, q: &str, k: usize) -> Vec<hiis::Hit> {
    let mut all = Vec::new();
    let mut cur = None;
    for _ in 0..1000 {
        let page = e.search_page(q, k, cur).unwrap();
        all.extend(page.hits);
        match page.next {
            Some(c) => cur = Some(c.to_string().parse().unwrap()),
            None => return all,
        }
    }
    panic!("pagination never ended");
}

#[test]
fn pages_concatenate_to_the_full_ranking() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, Config { flush_docs: 40, ..small_cfg() }).unwrap();
    corpus(&e, 400);

    // dumb scores tie a lot here, which is the interesting part: ties break on doc id
    for q in ["red", "red green", "red^2 blue -gold", "\"red green\"", "(red OR blue) AND pink", "gre*"] {
        let full = e.search(q, 10_000).unwrap();
        assert!(full.len() > 20, "{q}");
        for k in [1, 7, 50, 10_000] {
            assert_eq!(walk(&e, q, k), full, "{q} k={k}");
        }
    }
}

#[test]
fn pagination_survives_a_merge() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, Config { flush_docs: 40, ..small_cfg() }).unwrap();
    corpus(&e, 300);
    let first = e.search_page("red blue", 10, None).unwrap();
    e.force_merge().unwrap();
    let second = e.search_page("red blue", 10, first.next).unwrap();

    let full = e.search("red blue", 20).unwrap();
    let mut joined = first.hits.clone();
    joined.extend(second.hits);
    assert_eq!(joined, full);
}

#[test]
fn last_page_has_no_cursor() {
    let (_d, e) = loaded(small_cfg());
    let all = e.search("dog", 100).unwrap().len();
    assert_eq!(all, 3);
    let p = e.search_page("dog", 5, None).unwrap();
    assert_eq!(p.hits.len(), 3);
    assert!(p.next.is_none());
    let p = e.search_page("dog", 3, None).unwrap();
    assert!(p.next.is_some());
    assert!(e.search_page("dog", 3, p.next).unwrap().hits.is_empty());
    assert!(e.search_page("dog", 0, None).unwrap().next.is_none());
}
