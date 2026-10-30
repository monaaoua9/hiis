mod common;
use common::*;
use hiis::Engine;

const DOCS: &[&str] = &[
    "ذهب الطلاب إلى المدرسة صباحا",        // 0
    "المعلمون في المدارس الكبيرة يعملون", // 1
    "كتبت المعلمة درسا عن الحاسوب",       // 2
    "هذا كتاب جديد عن تاريخ المدينة",     // 3
    "The school opened a new library",    // 4
];

fn engine() -> (TmpDir, Engine) {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    for d in DOCS {
        e.add_document(d).unwrap();
    }
    e.commit().unwrap();
    (dir, e)
}

#[test]
fn article_and_ending_variants_match() {
    let (_d, e) = engine();
    // with and without the article, with and without the teh marbuta
    for q in ["مدرسة", "المدرسة", "مدرسه", "بالمدرسة", "والمدرسة"] {
        assert!(ids(&e.search(q, 5).unwrap()).contains(&0), "{q}");
    }
    // plural / singular teachers
    for q in ["معلمون", "المعلمين", "معلمات"] {
        assert!(ids(&e.search(q, 5).unwrap()).contains(&1), "{q}");
    }
}

#[test]
fn diacritics_and_hamza_dont_matter() {
    let (_d, e) = engine();
    assert!(ids(&e.search("اِلى", 5).unwrap()).is_empty(), "a stopword on its own matches nothing");
    assert!(ids(&e.search("المَدْرَسَةِ", 5).unwrap()).contains(&0));
    assert!(ids(&e.search("الحاسوب", 5).unwrap()).contains(&2));
}

#[test]
fn stopwords_do_not_break_phrases() {
    let (_d, e) = engine();
    // "في" sits between the words and leaves a gap, same on both sides
    assert_eq!(ids(&e.search("\"المعلمون في المدارس\"", 5).unwrap()), [1]);
    assert_eq!(ids(&e.search("\"المعلمون المدارس\"", 5).unwrap()), [] as [u64; 0]);
    assert_eq!(ids(&e.search("\"المعلمون المدارس\"~2", 5).unwrap()), [1]);
}

#[test]
fn mixed_script_documents() {
    let (_d, e) = engine();
    assert_eq!(ids(&e.search("school", 5).unwrap()), [4]);
    assert_eq!(sorted_ids(&e.search("مدرسة school", 5).unwrap()), [0, 4]);
}

#[test]
fn highlight_and_suggest_work_on_arabic() {
    let (_d, e) = engine();
    let s = e.highlight(0, "مدرسه", &hiis::HighlightOpts::default()).unwrap();
    assert!(s.text.contains("<em>المدرسة</em>"), "{}", s.text);
    let c = e.complete("مدر", 5);
    assert_eq!(c.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), ["مدرسة"], "{c:?}");
}
