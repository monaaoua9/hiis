use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use hiis::index::merge::MergePolicy;
use hiis::{Config, Engine};

struct TmpDir(PathBuf);

impl TmpDir {
    fn new() -> TmpDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "hiis-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn small_cfg() -> Config {
    Config {
        flush_docs: 3,
        background_merge: false,
        merge: MergePolicy { factor: 2, max_at_once: 8, floor_docs: 2 },
        ..Config::default()
    }
}

const CORPUS: &[&str] = &[
    "The quick brown fox jumps over the lazy dog",
    "A fast brown dog outpaces a quick fox",
    "Inverted indexes map every term to the documents containing it",
    "Posting lists are compressed with frame of reference encoding",
    "BM25 ranks documents by term frequency and inverse document frequency",
    "Segment merging rewrites many small segments into one large segment",
    "The lazy cat sleeps all day while the dog barks",
    "Phrase queries need term positions stored in the posting lists",
];

fn ids(hits: &[hiis::Hit]) -> Vec<u64> {
    hits.iter().map(|h| h.doc_id).collect()
}

fn loaded(cfg: Config) -> (TmpDir, Engine) {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, cfg).unwrap();
    for d in CORPUS {
        e.add_document(d).unwrap();
    }
    e.commit().unwrap();
    (dir, e)
}

#[test]
fn ranks_by_bm25_and_returns_best_first() {
    let (_d, e) = loaded(Config::default());
    let hits = e.search("quick fox", 10).unwrap();
    let top2: std::collections::HashSet<u64> = ids(&hits)[..2].iter().copied().collect();
    assert_eq!(top2, [0, 1].into());
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    assert!(hits.iter().all(|h| h.score > 0.0));
}

#[test]
fn stemming_and_stopwords_apply_to_queries() {
    let (_d, e) = loaded(Config::default());
    assert_eq!(ids(&e.search("compress", 5).unwrap()), [3]);
    assert!(!e.search("document", 5).unwrap().is_empty());
    assert!(e.search("the of and", 5).unwrap().is_empty());
}

#[test]
fn phrase_queries_respect_adjacency() {
    let (_d, e) = loaded(Config::default());
    assert_eq!(ids(&e.search("\"brown fox\"", 10).unwrap()), [0]);
    assert_eq!(ids(&e.search("\"brown dog\"", 10).unwrap()), [1]);
    assert!(e.search("\"fox brown\"", 10).unwrap().is_empty());
    // stopword in the middle must keep its gap
    assert_eq!(ids(&e.search("\"frame of reference\"", 10).unwrap()), [3]);
    assert_eq!(e.search("\"posting lists\"", 10).unwrap().len(), 2);
}

#[test]
fn must_and_must_not() {
    let (_d, e) = loaded(Config::default());
    let mut got = ids(&e.search("+dog +lazy", 10).unwrap());
    got.sort();
    assert_eq!(got, [0, 6]);
    assert_eq!(ids(&e.search("dog -lazy", 10).unwrap()), [1]);
    assert!(e.search("-dog", 10).unwrap().is_empty());
}

#[test]
fn top_k_is_truncated_and_stable() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, Config::default()).unwrap();
    for i in 0..50 {
        e.add_document(&format!("common word number {i}")).unwrap();
    }
    e.commit().unwrap();
    let a = e.search("common", 7).unwrap();
    assert_eq!(a.len(), 7);
    // equal scores: ties go to the lowest id
    assert_eq!(ids(&a), (0..7).collect::<Vec<_>>());
    assert!(e.search("common", 0).unwrap().is_empty());
}

#[test]
fn merging_preserves_results() {
    let (_d, e) = loaded(small_cfg());
    assert!(e.stats().segments > 1);

    let queries = ["quick fox", "\"posting lists\"", "+dog +lazy", "segment merging", "frequency -inverse"];
    let before: Vec<_> = queries.iter().map(|q| e.search(q, 10).unwrap()).collect();

    e.force_merge().unwrap();
    assert_eq!(e.stats().segments, 1);
    assert_eq!(e.stats().live_docs, CORPUS.len() as u64);

    for (q, old) in queries.iter().zip(&before) {
        let new = e.search(q, 10).unwrap();
        assert_eq!(ids(&new), ids(old), "query {q}");
        for (a, b) in new.iter().zip(old) {
            assert!((a.score - b.score).abs() < 1e-4, "query {q}: {} vs {}", a.score, b.score);
        }
    }
}

#[test]
fn deletes_hide_docs_and_merge_reclaims_them() {
    let (_d, e) = loaded(small_cfg());
    assert!(e.delete(0).unwrap());
    assert!(!e.delete(0).unwrap());
    assert!(!e.delete(999).unwrap());
    assert!(e.get(0).is_none());
    assert!(e.search("\"brown fox\"", 10).unwrap().is_empty());
    assert!(!ids(&e.search("fox", 10).unwrap()).contains(&0));

    e.force_merge().unwrap();
    let st = e.stats();
    assert_eq!(st.deleted_docs, 0);
    assert_eq!(st.live_docs, CORPUS.len() as u64 - 1);
    assert!(e.search("fox", 10).unwrap().iter().all(|h| h.doc_id != 0));
}

#[test]
fn delete_of_buffered_doc_works() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, Config::default()).unwrap();
    let a = e.add_document("alpha unique").unwrap();
    e.add_document("alpha other").unwrap();
    assert!(e.delete(a).unwrap());
    e.commit().unwrap();
    assert_eq!(e.search("alpha", 10).unwrap().len(), 1);
    assert!(e.search("unique", 10).unwrap().is_empty());
}

#[test]
fn reopen_restores_everything() {
    let dir = TmpDir::new();
    {
        let e = Engine::open(&dir.0, small_cfg()).unwrap();
        for d in CORPUS {
            e.add_document(d).unwrap();
        }
        e.delete(2).unwrap();
        e.commit().unwrap();
    }
    let e = Engine::open(&dir.0, small_cfg()).unwrap();
    assert_eq!(e.stats().live_docs, CORPUS.len() as u64 - 1);
    assert_eq!(ids(&e.search("\"brown fox\"", 10).unwrap()), [0]);
    assert!(e.get(2).is_none());
    assert_eq!(e.get(3).as_deref(), Some(CORPUS[3]));

    // ids keep counting instead of colliding with old ones
    assert_eq!(e.add_document("fresh document").unwrap(), CORPUS.len() as u64);
}

#[test]
fn uncommitted_buffer_is_not_visible() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, Config::default()).unwrap();
    e.add_document("hello world").unwrap();
    assert!(e.search("hello", 5).unwrap().is_empty());
    assert_eq!(e.stats().buffered_docs, 1);
    e.commit().unwrap();
    assert_eq!(e.search("hello", 5).unwrap().len(), 1);
}

#[test]
fn corrupt_segment_is_detected_on_open() {
    let dir = TmpDir::new();
    {
        let e = Engine::open(&dir.0, Config::default()).unwrap();
        e.add_document("some text to index").unwrap();
        e.commit().unwrap();
    }
    let seg = std::fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|f| f.path())
        .find(|p| p.extension().is_some_and(|x| x == "hiis"))
        .unwrap();
    let mut bytes = std::fs::read(&seg).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&seg, bytes).unwrap();

    match Engine::open(&dir.0, Config::default()) {
        Err(e) => assert!(e.to_string().contains("corrupt"), "{e}"),
        Ok(_) => panic!("opened a corrupted segment"),
    }
}

#[test]
fn background_merger_converges() {
    let dir = TmpDir::new();
    let cfg = Config { background_merge: true, ..small_cfg() };
    let e = Engine::open(&dir.0, cfg).unwrap();
    for i in 0..60 {
        e.add_document(&format!("document number {i} about rust and search")).unwrap();
    }
    e.commit().unwrap();
    e.wait_for_merges();

    assert!(e.last_merge_error().is_none());
    let st = e.stats();
    assert_eq!(st.live_docs, 60);
    assert!(st.segments < 20, "{} segments left", st.segments);
    assert_eq!(e.search("rust", 100).unwrap().len(), 60);
}

#[test]
fn concurrent_search_and_indexing() {
    let dir = TmpDir::new();
    let cfg = Config { background_merge: true, ..small_cfg() };
    let e = std::sync::Arc::new(Engine::open(&dir.0, cfg).unwrap());

    let writer = {
        let e = e.clone();
        std::thread::spawn(move || {
            for i in 0..300 {
                e.add_document(&format!("stream item {i} marker")).unwrap();
                if i % 25 == 0 {
                    e.commit().unwrap();
                }
            }
            e.commit().unwrap();
        })
    };
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let e = e.clone();
            std::thread::spawn(move || {
                let mut last = 0;
                for _ in 0..200 {
                    let n = e.search("marker", 1000).unwrap().len();
                    assert!(n >= last, "result count went backwards: {last} -> {n}");
                    last = n;
                }
            })
        })
        .collect();

    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
    e.wait_for_merges();
    assert_eq!(e.search("marker", 1000).unwrap().len(), 300);
}

#[test]
fn uncommitted_docs_survive_a_crash_via_the_wal() {
    let dir = TmpDir::new();
    {
        let e = Engine::open(&dir.0, Config::default()).unwrap();
        e.add_document("durable before commit").unwrap();
        e.add_document("second pending doc").unwrap();
        // dropped without commit(): the buffer is gone, the log is not
    }
    let e = Engine::open(&dir.0, Config::default()).unwrap();
    assert_eq!(e.stats().buffered_docs, 2);
    e.commit().unwrap();
    assert_eq!(ids(&e.search("durable", 5).unwrap()), [0]);
    assert_eq!(e.add_document("next").unwrap(), 2);
}

#[test]
fn torn_wal_tail_does_not_block_recovery() {
    let dir = TmpDir::new();
    {
        let e = Engine::open(&dir.0, Config::default()).unwrap();
        e.add_document("kept document").unwrap();
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(dir.0.join("wal.log")).unwrap();
    f.write_all(&[99, 0, 0, 0, 1, 2]).unwrap();
    drop(f);

    let e = Engine::open(&dir.0, Config::default()).unwrap();
    e.commit().unwrap();
    assert_eq!(e.search("kept", 5).unwrap().len(), 1);
}

#[test]
fn flushed_docs_are_not_replayed_twice() {
    let dir = TmpDir::new();
    {
        let e = Engine::open(&dir.0, Config::default()).unwrap();
        e.add_document("only once").unwrap();
        e.commit().unwrap();
    }
    // simulate a crash between manifest write and log reset: log still has the doc
    {
        let (mut w, _) = (std::fs::OpenOptions::new().append(true).open(dir.0.join("wal.log")).unwrap(), ());
        use std::io::Write;
        let text = "only once";
        let mut payload = 0u64.to_le_bytes().to_vec();
        payload.extend_from_slice(text.as_bytes());
        let mut rec = (payload.len() as u32).to_le_bytes().to_vec();
        rec.extend_from_slice(&hiis::codec::crc32::checksum(&payload).to_le_bytes());
        rec.extend_from_slice(&payload);
        w.write_all(&rec).unwrap();
    }
    let e = Engine::open(&dir.0, Config::default()).unwrap();
    assert_eq!(e.stats().buffered_docs, 0);
    assert_eq!(e.search("once", 5).unwrap().len(), 1);
}

fn lcg(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *state >> 33
}

#[test]
fn wand_pruning_matches_exhaustive_scoring_exactly() {
    let dir = TmpDir::new();
    let cfg = Config {
        flush_docs: 700,
        merge: MergePolicy { factor: 3, max_at_once: 8, floor_docs: 50 },
        ..small_cfg()
    };
    let e = Engine::open(&dir.0, cfg).unwrap();

    let vocab: Vec<String> = (0..400).map(|i| format!("term{i}")).collect();
    let mut st = 12345u64;
    for _ in 0..3000 {
        let len = 5 + lcg(&mut st) % 60;
        let words: Vec<&str> = (0..len)
            .map(|_| {
                // skew toward low ranks so some terms are very common
                let u = (lcg(&mut st) % 10_000) as f64 / 10_000.0;
                vocab[(u * u * u * vocab.len() as f64) as usize].as_str()
            })
            .collect();
        e.add_document(&words.join(" ")).unwrap();
    }
    e.commit().unwrap();
    for id in (0..3000).step_by(11) {
        e.delete(id).unwrap();
    }
    assert!(e.stats().segments > 1);

    let mut checked = 0;
    for round in 0..60 {
        let nt = 1 + (round % 4) as u64;
        let q: Vec<&str> = (0..nt)
            .map(|_| {
                let u = (lcg(&mut st) % 10_000) as f64 / 10_000.0;
                vocab[(u * u * vocab.len() as f64) as usize].as_str()
            })
            .collect();
        let q = q.join(" ");
        for k in [1, 5, 25] {
            let fast = e.search(&q, k).unwrap();
            let slow = e.search_unpruned(&q, k).unwrap();
            assert_eq!(fast.len(), slow.len(), "q={q} k={k}");
            for (a, b) in fast.iter().zip(&slow) {
                assert_eq!(a.doc_id, b.doc_id, "q={q} k={k}");
                assert_eq!(a.score.to_bits(), b.score.to_bits(), "q={q} k={k}");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 180);
}

#[test]
fn second_engine_on_same_dir_is_refused() {
    let dir = TmpDir::new();
    let first = Engine::open(&dir.0, Config::default()).unwrap();
    match Engine::open(&dir.0, Config::default()) {
        Err(hiis::Error::Locked) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("two engines opened the same directory"),
    }
    // the refused attempt must not have disturbed the first one
    first.add_document("still works").unwrap();
    first.commit().unwrap();
    drop(first);
    let again = Engine::open(&dir.0, Config::default()).unwrap();
    assert_eq!(again.search("works", 5).unwrap().len(), 1);
}
