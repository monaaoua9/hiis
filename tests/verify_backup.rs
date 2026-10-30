mod common;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use hiis::{Config, Engine};

fn bulk(e: &Engine, n: usize) {
    for i in 0..n {
        e.add_document(&format!("document number {i} about topic{} and shared words", i % 7)).unwrap();
    }
    e.commit().unwrap();
}

#[test]
fn healthy_index_verifies_clean() {
    let (_d, e) = loaded(small_cfg());
    let rep = e.verify(true);
    assert!(rep.is_ok(), "{rep}");
    assert_eq!(rep.docs, CORPUS.len() as u64);
    assert!(rep.terms > 20 && rep.postings > 30);

    e.delete(2).unwrap();
    e.force_merge().unwrap();
    bulk(&e, 50);
    assert!(e.verify(true).is_ok());
    assert!(e.verify(false).to_string().ends_with("ok"));
}

#[test]
fn verify_notices_damage_that_passes_the_checksum() {
    let dir = TmpDir::new();
    {
        let e = Engine::open(&dir.0, Config { background_merge: false, ..Config::default() }).unwrap();
        bulk(&e, 120);
    }
    let seg = std::fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|f| f.path())
        .find(|p| p.extension().is_some_and(|x| x == "hiis"))
        .unwrap();
    let orig = std::fs::read(&seg).unwrap();
    let foot = orig.len() - 48;

    let (mut caught, mut opened) = (0, 0);
    for off in (8..300).step_by(7) {
        let mut bad = orig.clone();
        bad[off] ^= 0x5a;
        let crc = hiis::codec::crc32::checksum(&bad[..foot + 40]);
        bad[foot + 40..foot + 44].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&seg, &bad).unwrap();

        // a refused open is also a detection; only count the ones that got through
        if let Ok(e) = Engine::open(&dir.0, Config { background_merge: false, ..Config::default() }) {
            opened += 1;
            if !e.verify(true).is_ok() {
                caught += 1;
            }
        }
    }
    assert!(opened > 0 && caught > opened / 2, "opened {opened}, verify flagged {caught}");
}

#[test]
fn backup_opens_as_an_independent_index() {
    let (_d, e) = loaded(small_cfg());
    e.delete(1).unwrap();
    e.add_document("buffered but not yet committed").unwrap();

    let dest = TmpDir::new();
    e.backup(&dest.0).unwrap();
    let want = e.search("fox dog", 10).unwrap();

    // the original carries on after the backup
    e.add_document("fox written after the backup").unwrap();
    e.commit().unwrap();

    let b = Engine::open(&dest.0, small_cfg()).unwrap();
    assert!(b.verify(true).is_ok());
    assert_eq!(b.search("fox dog", 10).unwrap(), want);
    assert_eq!(b.get(1), None, "deleted doc must stay deleted");
    assert!(b.get(0).is_some());
    // commit() ran before the copy, so the buffered doc is in
    assert_eq!(b.search("buffered", 5).unwrap().len(), 1);
    assert!(b.search("backup", 5).unwrap().is_empty());

    // ids keep counting from where the source was
    let id = b.add_document("new").unwrap();
    assert!(id > CORPUS.len() as u64);
}

#[test]
fn backup_refuses_bad_targets() {
    let (d, e) = loaded(small_cfg());
    assert!(e.backup(&d.0).is_err());
    let dest = TmpDir::new();
    e.backup(&dest.0).unwrap();
    assert!(e.backup(&dest.0).is_err());
}

#[test]
fn backup_while_writing_and_merging() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, Config { flush_docs: 20, ..small_cfg() }).unwrap();
    bulk(&e, 100);

    let stop = AtomicBool::new(false);
    let dest = TmpDir::new();
    std::thread::scope(|sc| {
        sc.spawn(|| {
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                e.add_document(&format!("live write {i} shared")).unwrap();
                i += 1;
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        let done = e.backup(&dest.0);
        // stop the writer before unwrapping, or a failure here would hang the scope
        stop.store(true, Ordering::Relaxed);
        done.unwrap();
    });

    let b = Engine::open(&dest.0, small_cfg()).unwrap();
    let rep = b.verify(true);
    assert!(rep.is_ok(), "{rep}");
    assert!(b.stats().live_docs >= 100);
    assert!(!b.search("shared", 5).unwrap().is_empty());
}
