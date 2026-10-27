//! Randomised and fault-injection tests. No external crates: a small seeded
//! generator keeps every failure reproducible from the printed seed.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use hiis::codec::{bitpack, crc32, vbyte};
use hiis::index::postings::{PostingsCursor, PostingsWriter};
use hiis::index::segment::Segment;
use hiis::{Config, Engine};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "hiis-rb-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn vbyte_roundtrips_random_values() {
    let mut r = Rng(1);
    for seed in 0..200 {
        let vals: Vec<u64> = (0..r.below(300)).map(|_| r.next() >> r.below(64)).collect();
        let mut buf = Vec::new();
        vals.iter().for_each(|&v| vbyte::write_u64(&mut buf, v));
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(vbyte::read_u64(&buf, &mut pos).unwrap(), v, "seed {seed}");
        }
        assert_eq!(pos, buf.len());
    }
}

#[test]
fn bitpack_roundtrips_random_blocks() {
    let mut r = Rng(2);
    for seed in 0..500 {
        let base = (r.next() >> r.below(33)) as u32;
        let spread = 1u64 << r.below(33);
        let n = 1 + r.below(bitpack::BLOCK_LEN as u64) as usize;
        let vals: Vec<u32> =
            (0..n).map(|_| base.saturating_add((r.below(spread)).min(u32::MAX as u64) as u32)).collect();
        let mut buf = Vec::new();
        bitpack::pack(&vals, &mut buf);
        let mut out = Vec::new();
        let mut pos = 0;
        bitpack::unpack(&buf, &mut pos, n, &mut out).unwrap();
        assert_eq!(out, vals, "seed {seed}");
        assert_eq!(pos, buf.len());
    }
}

type Entry = (u32, Vec<u32>);

fn random_postings(r: &mut Rng) -> Vec<Entry> {
    let n = r.below(700) as usize;
    let mut doc = r.below(5) as u32;
    (0..n)
        .map(|_| {
            let span = if r.below(4) == 0 { 5000 } else { 3 };
            doc += 1 + r.below(span) as u32;
            let mut p = r.below(4) as u32;
            let pos: Vec<u32> = (0..1 + r.below(6))
                .map(|_| {
                    p += 1 + r.below(30) as u32;
                    p
                })
                .collect();
            (doc, pos)
        })
        .collect()
}

fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut w = PostingsWriter::new();
    entries.iter().for_each(|(d, p)| w.add(*d, p));
    w.finish().bytes
}

#[test]
fn postings_roundtrip_and_advance_match_a_naive_model() {
    let mut r = Rng(3);
    for seed in 0..150 {
        let entries = random_postings(&mut r);
        let bytes = encode(&entries);

        // full scan with positions
        let mut c = PostingsCursor::new(&bytes).unwrap();
        let mut pos = Vec::new();
        for (d, p) in &entries {
            assert_eq!(c.next().unwrap(), Some(*d), "seed {seed}");
            assert_eq!(c.freq() as usize, p.len());
            c.positions(&mut pos).unwrap();
            assert_eq!(&pos, p, "seed {seed}");
        }
        assert_eq!(c.next().unwrap(), None);

        // random monotone advance() calls against a linear search
        let mut c = PostingsCursor::new(&bytes).unwrap();
        let max = entries.last().map_or(10, |e| e.0 + 10);
        let mut target = 0u32;
        for _ in 0..40 {
            target += r.below(max as u64 / 8 + 2) as u32;
            let want = entries.iter().map(|e| e.0).find(|&d| d >= target);
            assert_eq!(c.advance(target).unwrap(), want, "seed {seed} target {target}");
            if want.is_none() {
                break;
            }
        }
    }
}

#[test]
fn block_max_tf_is_a_true_upper_bound() {
    let mut r = Rng(4);
    for _ in 0..60 {
        let entries = random_postings(&mut r);
        if entries.is_empty() {
            continue;
        }
        let bytes = encode(&entries);
        let mut c = PostingsCursor::new(&bytes).unwrap();
        assert_eq!(c.max_tf() as usize, entries.iter().map(|e| e.1.len()).max().unwrap());
        while c.next().unwrap().is_some() {
            assert!(c.freq() <= c.block_max_tf());
        }
    }
}

#[test]
fn garbage_never_panics_the_decoders() {
    let mut r = Rng(5);
    for _ in 0..3000 {
        let junk: Vec<u8> = (0..r.below(200)).map(|_| r.next() as u8).collect();
        let _ = Segment::from_bytes(0, junk.clone());
        if let Ok(mut c) = PostingsCursor::new(&junk) {
            let mut pos = Vec::new();
            for _ in 0..500 {
                match c.next() {
                    Ok(Some(_)) => {
                        let _ = c.positions(&mut pos);
                    }
                    _ => break,
                }
            }
        }
        let _ = bitpack::unpack(&junk, &mut 0, 128, &mut Vec::new());
    }
}

fn build_segment_bytes(dir: &TmpDir) -> Vec<u8> {
    let e = Engine::open(&dir.0, Config { background_merge: false, ..Config::default() }).unwrap();
    let mut r = Rng(9);
    for i in 0..300 {
        let words: Vec<String> = (0..4 + r.below(20)).map(|_| format!("w{}", r.below(40))).collect();
        e.add_document(&format!("{} doc{i}", words.join(" "))).unwrap();
    }
    e.commit().unwrap();
    drop(e);
    let seg = std::fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|f| f.path())
        .find(|p| p.extension().is_some_and(|x| x == "hiis"))
        .unwrap();
    std::fs::read(seg).unwrap()
}

fn fix_crc(bytes: &mut [u8]) {
    let foot = bytes.len() - 40;
    let crc = crc32::checksum(&bytes[..foot + 32]);
    bytes[foot + 32..foot + 36].copy_from_slice(&crc.to_le_bytes());
}

// A corrupted file whose checksum still validates (bug or malicious input)
// must produce errors, never a panic or an out-of-range document id.
#[test]
fn checksum_valid_but_mutated_segments_are_survivable() {
    let dir = TmpDir::new("mut");
    let good = build_segment_bytes(&dir);
    let mut r = Rng(11);
    let mut opened = 0;

    for round in 0..1500 {
        let mut bad = good.clone();
        for _ in 0..1 + r.below(3) {
            let at = r.below(bad.len() as u64 - 40) as usize;
            bad[at] = r.next() as u8;
        }
        fix_crc(&mut bad);

        let Ok(seg) = Segment::from_bytes(1, bad) else { continue };
        opened += 1;
        // touch everything a query could touch
        let n = seg.n_docs();
        for e in seg.terms().to_vec() {
            let Ok(mut c) = seg.cursor(&e) else { continue };
            let mut pos = Vec::new();
            while let Ok(Some(d)) = c.next() {
                assert!(d < n, "round {round}: doc id {d} out of range ({n} docs)");
                let _ = c.positions(&mut pos);
                let _ = seg.doc(d);
            }
        }
        for local in 0..n {
            let _ = seg.text(local);
        }
    }
    assert!(opened > 100, "mutations were too destructive to test anything ({opened} opened)");
}

#[test]
fn leftovers_from_a_crashed_flush_or_merge_are_cleaned_on_open() {
    let dir = TmpDir::new("orph");
    {
        let e = Engine::open(&dir.0, Config::default()).unwrap();
        e.add_document("survivor document").unwrap();
        e.commit().unwrap();
    }
    // segment written but manifest never updated, a half-written tmp, and a
    // merged segment whose predecessors were never removed
    std::fs::write(dir.0.join("seg_00000099.hiis"), b"half a segment").unwrap();
    std::fs::write(dir.0.join("seg_00000098.del"), b"junk").unwrap();
    std::fs::write(dir.0.join("seg_00000077.tmp"), b"tmp").unwrap();
    std::fs::write(dir.0.join("MANIFEST.tmp"), b"tmp").unwrap();

    let e = Engine::open(&dir.0, Config::default()).unwrap();
    assert_eq!(e.search("survivor", 5).unwrap().len(), 1);
    drop(e);

    let mut names: Vec<String> = std::fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(
        names
            .iter()
            .all(|n| !n.contains("99") && !n.contains("98") && !n.contains("77") && !n.ends_with(".tmp")),
        "{names:?}"
    );
}

// Real crash: kill the CLI mid-indexing, over and over, and check that what
// survives is a gap-free prefix of the input with intact text.
#[test]
fn killing_the_indexer_leaves_a_consistent_prefix() {
    let exe = env!("CARGO_BIN_EXE_hiis");
    let work = TmpDir::new("kill");
    let input = work.0.join("docs.txt");
    let total = 150_000;
    let text: String = (0..total).map(|i| format!("document number {i} shared marker words\n")).collect();
    std::fs::write(&input, text).unwrap();

    let idx = work.0.join("idx");
    let mut base = 0u64; // first id handed out to the current run
    for (round, ms) in [150u64, 400, 800, 250, 1200].into_iter().enumerate() {
        let mut child = Command::new(exe)
            .args(["index", idx.to_str().unwrap(), input.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(ms));
        let _ = child.kill();
        let _ = child.wait();

        // recover (WAL replay), then audit
        let e = Engine::open(&idx, Config { background_merge: false, ..Config::default() })
            .unwrap_or_else(|err| panic!("round {round}: reopen failed: {err}"));
        e.commit().unwrap();
        let n = e.stats().live_docs;
        println!(
            "round {round}: killed after {ms} ms, recovered {} docs, {} segments",
            n - base,
            e.stats().segments
        );
        assert!(n >= base && n - base <= total as u64, "round {round}: base {base} n {n}");

        // this run indexed lines 0.. in order, so id base+i must hold line i with no gaps
        for id in (base..n).step_by(997).chain(n.checked_sub(1).filter(|&l| l >= base)) {
            let want = format!("document number {} shared marker words", id - base);
            let got = e.get(id).unwrap_or_else(|| panic!("round {round}: id {id} missing out of {n}"));
            assert_eq!(got, want, "round {round}: id {id}");
        }
        assert!(e.get(n).is_none(), "round {round}: unexpected doc past the end");
        base = n;
    }
}
