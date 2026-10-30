mod common;
use common::*;
use hiis::{Config, Engine, Error};

fn ro() -> Config {
    Config { read_only: true, ..small_cfg() }
}

#[test]
fn many_readers_share_one_directory() {
    let (d, e) = loaded(small_cfg());
    let want = e.search("fox", 10).unwrap();
    drop(e);

    let a = Engine::open(&d.0, ro()).unwrap();
    let b = Engine::open(&d.0, ro()).unwrap();
    assert_eq!(a.search("fox", 10).unwrap(), want);
    assert_eq!(b.search("fox", 10).unwrap(), want);
    assert_eq!(a.get(0).as_deref(), Some(CORPUS[0]));
    assert!(a.verify(true).is_ok());
}

#[test]
fn writers_and_readers_exclude_each_other() {
    let (d, e) = loaded(small_cfg());
    assert!(matches!(Engine::open(&d.0, ro()), Err(Error::Locked)));
    drop(e);

    let r = Engine::open(&d.0, ro()).unwrap();
    assert!(matches!(Engine::open(&d.0, small_cfg()), Err(Error::Locked)));
    drop(r);
    assert!(Engine::open(&d.0, small_cfg()).is_ok());
}

#[test]
fn every_write_is_refused() {
    let (d, e) = loaded(small_cfg());
    drop(e);
    let r = Engine::open(&d.0, ro()).unwrap();
    assert!(matches!(r.add_document("x"), Err(Error::Invalid(_))));
    assert!(matches!(r.add_documents(&["x", "y"]), Err(Error::Invalid(_))));
    assert!(matches!(r.delete(0), Err(Error::Invalid(_))));
    assert!(matches!(r.force_merge(), Err(Error::Invalid(_))));
    // nothing to flush, so this one is a harmless no-op
    r.commit().unwrap();
    assert_eq!(r.stats().live_docs as usize, CORPUS.len());
}

#[test]
fn opening_changes_nothing_on_disk() {
    let dir = TmpDir::new();
    {
        let e = Engine::open(&dir.0, small_cfg()).unwrap();
        for d in CORPUS {
            e.add_document(d).unwrap();
        }
        e.commit().unwrap();
        // left in the log, uncommitted
        e.add_document("only in the wal").unwrap();
        std::mem::forget(e);
    }
    // forgetting skipped the Drop but the lock dies with the file handle in the OS only at exit,
    // so work on a copy
    let copy = TmpDir::new();
    std::fs::create_dir_all(&copy.0).unwrap();
    for f in std::fs::read_dir(&dir.0).unwrap().flatten() {
        if f.file_name() != "LOCK" {
            std::fs::copy(f.path(), copy.0.join(f.file_name())).unwrap();
        }
    }
    std::fs::write(copy.0.join("stray.tmp"), b"x").unwrap();
    let snapshot = |p: &std::path::Path| {
        let mut v: Vec<_> = std::fs::read_dir(p)
            .unwrap()
            .flatten()
            .map(|f| (f.file_name(), f.metadata().unwrap().len()))
            .collect();
        v.sort();
        v
    };
    std::fs::write(copy.0.join("LOCK"), b"").unwrap();
    let before = snapshot(&copy.0);

    let r = Engine::open(&copy.0, ro()).unwrap();
    // the log is ignored: only committed documents are visible
    assert!(r.search("wal", 5).unwrap().is_empty());
    assert_eq!(r.stats().live_docs as usize, CORPUS.len());
    drop(r);
    assert_eq!(snapshot(&copy.0), before, "stray.tmp must survive, nothing may be created");
}

#[test]
fn missing_directory_is_an_error_not_a_new_index() {
    let d = TmpDir::new();
    assert!(Engine::open(&d.0, ro()).is_err());
    assert!(!d.0.exists());
}

#[test]
fn read_only_engine_can_back_up() {
    let (d, e) = loaded(small_cfg());
    drop(e);
    let r = Engine::open(&d.0, ro()).unwrap();
    let dest = TmpDir::new();
    r.backup(&dest.0).unwrap();
    drop(r);
    let b = Engine::open(&dest.0, small_cfg()).unwrap();
    assert_eq!(b.stats().live_docs as usize, CORPUS.len());
}
