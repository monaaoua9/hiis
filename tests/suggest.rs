mod common;
use common::*;

fn words(v: &[hiis::Suggestion]) -> Vec<&str> {
    v.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn completes_with_real_words() {
    let (_d, e) = loaded(small_cfg());
    // the dictionary only has the stem "compress"
    assert_eq!(words(&e.complete("compre", 5)), ["compressed"]);
    assert_eq!(words(&e.complete("SEGM", 5)), ["segment"]);
    assert!(e.complete("zzz", 5).is_empty());
    assert!(e.complete("", 5).is_empty());
    assert!(e.complete("seg", 0).is_empty());
}

#[test]
fn ranks_by_document_frequency() {
    let dir = TmpDir::new();
    let e = hiis::Engine::open(&dir.0, small_cfg()).unwrap();
    for _ in 0..3 {
        e.add_document("database tuning").unwrap();
    }
    e.add_document("datagram basics").unwrap();
    e.add_document("dataset size").unwrap();
    e.add_document("dataset split").unwrap();
    e.commit().unwrap();

    let got = e.complete("data", 10);
    assert_eq!(words(&got), ["database", "dataset", "datagram"]);
    assert_eq!(got[0].doc_freq, 3);
    assert_eq!(words(&e.complete("data", 2)), ["database", "dataset"]);
}

#[test]
fn typing_past_the_stem_still_completes() {
    let dir = TmpDir::new();
    let e = hiis::Engine::open(&dir.0, small_cfg()).unwrap();
    e.add_document("lossless compression of text").unwrap();
    e.add_document("compressing streams").unwrap();
    e.commit().unwrap();
    // one word per stem, taken from the first document that has it
    assert_eq!(words(&e.complete("compressi", 5)), ["compression"]);
    assert_eq!(words(&e.complete("compressio", 5)), ["compression"]);
}

#[test]
fn deleted_docs_dont_supply_words() {
    let dir = TmpDir::new();
    let e = hiis::Engine::open(&dir.0, small_cfg()).unwrap();
    let a = e.add_document("quarantine notes").unwrap();
    e.add_document("quartz crystals").unwrap();
    e.commit().unwrap();
    e.delete(a).unwrap();
    assert_eq!(words(&e.complete("quar", 5)), ["quartz"]);
}

#[test]
fn did_you_mean_fixes_unknown_words_only() {
    let (_d, e) = loaded(small_cfg());
    assert_eq!(e.did_you_mean("quikc fox").as_deref(), Some("quick fox"));
    assert_eq!(e.did_you_mean("+quikc -lazyy \"brwn fox\"").as_deref(), Some("+quick -lazy \"brown fox\""));
    assert_eq!(e.did_you_mean("quick fox"), None);
    // operators and fuzzy/wildcard words are not touched
    assert_eq!(e.did_you_mean("quick AND fox"), None);
    assert_eq!(e.did_you_mean("quikc~"), None);
    assert_eq!(e.did_you_mean("quik*"), None);
    // nothing near it
    assert_eq!(e.did_you_mean("xylophone"), None);
    assert_eq!(e.did_you_mean(""), None);
}

#[test]
fn corrected_query_finds_something() {
    let (_d, e) = loaded(small_cfg());
    assert!(e.search("invertd", 5).unwrap().is_empty());
    let fixed = e.did_you_mean("invertd").unwrap();
    assert_eq!(fixed, "inverted");
    assert!(!e.search(&fixed, 5).unwrap().is_empty());
}
