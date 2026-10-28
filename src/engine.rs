//! The engine ties everything together.
//!
//! Write path:  `add_document` -> analyze -> RAM buffer -> (threshold) flush ->
//!              immutable segment file -> manifest -> wake merger thread.
//! Read path:   `search` takes a snapshot (`Vec<Arc<Segment>>`) and runs
//!              without holding any lock, so queries never block on a merge.
//!
//! Visibility/durability model: documents become searchable when their buffer
//! is flushed (commit, or automatically at the configured thresholds). Until
//! then they live in the RAM buffer and in the write-ahead log, which is replayed
//! on open, so an uncommitted document survives a crash (and a power loss too if
//! wal_fsync is on). Deletes are applied immediately in memory and persisted by
//! commit.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use crate::analysis::{Analyzer, AnalyzerConfig};
use crate::error::{Error, Result};
use crate::index::builder::SegmentBuilder;
use crate::index::merge::{merge_segments, MergePolicy};
use crate::index::segment::{bit_set, del_path, seg_path, write_atomic, Segment};
use crate::index::DocId;
use crate::search::bm25::Bm25;
use crate::search::query::Query;
use crate::search::{self, Hit};
use crate::wal::Wal;

const MANIFEST: &str = "MANIFEST";
const WAL: &str = "wal.log";

#[derive(Debug, Clone)]
pub struct Config {
    pub analyzer: AnalyzerConfig,
    pub bm25_k1: f32,
    pub bm25_b: f32,
    /// flush the RAM buffer once it holds this many docs...
    pub flush_docs: usize,
    /// ...or roughly this many bytes
    pub flush_bytes: usize,
    pub merge: MergePolicy,
    pub background_merge: bool,
    /// fsync the log on every add: survives power loss, costs an fsync per document.
    /// Off, the log still survives a process crash (data is in the OS cache).
    pub wal_fsync: bool,
    /// segment files up to this size are held in memory; bigger ones are read
    /// from disk per query, so index size is not capped by RAM
    pub resident_limit: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            analyzer: AnalyzerConfig::default(),
            bm25_k1: 1.2,
            bm25_b: 0.75,
            flush_docs: 50_000,
            flush_bytes: 64 << 20,
            merge: MergePolicy::default(),
            background_merge: true,
            wal_fsync: false,
            resident_limit: 512 << 20,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub segments: usize,
    pub live_docs: u64,
    pub deleted_docs: u64,
    pub buffered_docs: usize,
    pub terms: u64,
    pub disk_bytes: u64,
}

#[derive(Default)]
struct MergeCtl {
    wake: bool,
    shutdown: bool,
}

struct Shared {
    _lock: fs::File,
    dir: PathBuf,
    cfg: Config,
    analyzer: Analyzer,
    buffer: Mutex<SegmentBuilder>,
    // lock order: buffer, then wal
    wal: Mutex<Wal>,
    flushed_upto: AtomicU64,
    segments: RwLock<Vec<Arc<Segment>>>,
    next_doc: AtomicU64,
    next_seg: AtomicU64,
    manifest_mu: Mutex<()>,
    // serialises background and forced merges
    merge_mu: Mutex<()>,
    ctl: Mutex<MergeCtl>,
    cv: Condvar,
    last_merge_err: Mutex<Option<String>>,
}

pub struct Engine {
    shared: Arc<Shared>,
    merger: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn open(dir: impl AsRef<Path>, cfg: Config) -> Result<Engine> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        // held for the life of the engine; the OS drops it if we crash
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("LOCK"))?;
        lock.try_lock().map_err(|e| match e {
            fs::TryLockError::WouldBlock => Error::Locked,
            fs::TryLockError::Error(e) => Error::Io(e),
        })?;

        let (mut next_doc, mut next_seg) = (0u64, 0u64);
        let mut flushed_upto: Option<u64> = None;
        let mut ids = Vec::new();
        match fs::read_to_string(dir.join(MANIFEST)) {
            Ok(text) => {
                for line in text.lines() {
                    let mut it = line.split_whitespace();
                    let (key, val) = (it.next(), it.next().and_then(|v| v.parse::<u64>().ok()));
                    match (key, val) {
                        (Some("next_doc"), Some(v)) => next_doc = v,
                        (Some("next_seg"), Some(v)) => next_seg = v,
                        (Some("flushed_upto"), Some(v)) => flushed_upto = Some(v),
                        (Some("segment"), Some(v)) => ids.push(v),
                        (None, _) => {}
                        _ => return Err(Error::Corrupt("unparseable manifest line")),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let mut segments = Vec::with_capacity(ids.len());
        for &id in &ids {
            segments.push(Arc::new(Segment::open_with(&dir, id, cfg.resident_limit)?));
        }
        remove_orphans(&dir, &ids);

        // docs in the log with id < flushed_upto already made it into a segment
        // (crash between manifest write and log reset)
        let flushed_upto = flushed_upto.unwrap_or(next_doc);
        let analyzer = Analyzer::new(cfg.analyzer.clone());
        let (wal, replayed) = Wal::open(&dir.join(WAL), cfg.wal_fsync)?;
        let mut builder = SegmentBuilder::default();
        for (id, text) in replayed.into_iter().filter(|r| r.0 >= flushed_upto) {
            builder.add(id, &text, SegmentBuilder::group(analyzer.analyze(&text)));
            next_doc = next_doc.max(id + 1);
        }

        let shared = Arc::new(Shared {
            _lock: lock,
            analyzer,
            dir,
            cfg,
            wal: Mutex::new(wal),
            flushed_upto: AtomicU64::new(flushed_upto),
            buffer: Mutex::new(builder),
            segments: RwLock::new(segments),
            next_doc: AtomicU64::new(next_doc),
            next_seg: AtomicU64::new(next_seg),
            manifest_mu: Mutex::new(()),
            merge_mu: Mutex::new(()),
            ctl: Mutex::new(MergeCtl::default()),
            cv: Condvar::new(),
            last_merge_err: Mutex::new(None),
        });

        let merger = if shared.cfg.background_merge {
            let s = Arc::clone(&shared);
            Some(thread::Builder::new().name("hiis-merger".into()).spawn(move || merger_loop(s))?)
        } else {
            None
        };
        // a previous run may have stopped with a merge pending
        shared.wake_merger();
        Ok(Engine { shared, merger })
    }

    pub fn add_document(&self, text: &str) -> Result<DocId> {
        let grouped = SegmentBuilder::group(self.shared.analyzer.analyze(text));

        let mut buf = self.shared.buffer.lock().unwrap();
        // id is taken under the buffer lock: flush relies on "every id below
        // next_doc is already in this buffer or in a segment"
        let id = self.shared.next_doc.fetch_add(1, Ordering::Relaxed);
        self.shared.wal.lock().unwrap().append(id, text)?;
        buf.add(id, text, grouped);
        let c = &self.shared.cfg;
        if buf.len() >= c.flush_docs || buf.ram_bytes() >= c.flush_bytes {
            self.shared.flush(&mut buf)?;
        }
        Ok(id)
    }

    /// Same as calling `add_document` for each text, but the WAL gets one
    /// write (and one fsync with `wal_fsync`) for the lot.
    pub fn add_documents<S: AsRef<str> + Sync>(&self, texts: &[S]) -> Result<Vec<DocId>> {
        let an = &self.shared.analyzer;
        let one = |t: &S| SegmentBuilder::group(an.analyze(t.as_ref()));
        let lanes = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
        let grouped: Vec<_> = if lanes < 2 || texts.len() < 32 {
            texts.iter().map(one).collect()
        } else {
            // analysis is the bulk of ingest cost and needs no shared state
            let per = texts.len().div_ceil(lanes);
            std::thread::scope(|sc| {
                let jobs: Vec<_> = texts
                    .chunks(per)
                    .map(|part| sc.spawn(|| part.iter().map(one).collect::<Vec<_>>()))
                    .collect();
                jobs.into_iter().flat_map(|j| j.join().unwrap()).collect()
            })
        };

        let mut buf = self.shared.buffer.lock().unwrap();
        let first = self.shared.next_doc.fetch_add(texts.len() as u64, Ordering::Relaxed);
        let ids: Vec<DocId> = (first..first + texts.len() as u64).collect();
        let batch: Vec<(DocId, &str)> = ids.iter().copied().zip(texts.iter().map(|t| t.as_ref())).collect();
        self.shared.wal.lock().unwrap().append_batch(&batch)?;
        for ((id, text), g) in batch.into_iter().zip(grouped) {
            buf.add(id, text, g);
        }
        let c = &self.shared.cfg;
        if buf.len() >= c.flush_docs || buf.ram_bytes() >= c.flush_bytes {
            self.shared.flush(&mut buf)?;
        }
        Ok(ids)
    }

    /// Flush the buffer and persist pending deletes.
    pub fn commit(&self) -> Result<()> {
        {
            let mut buf = self.shared.buffer.lock().unwrap();
            self.shared.flush(&mut buf)?;
        }
        let segs = self.shared.segments.read().unwrap().clone();
        for s in segs {
            s.persist_deletes(&self.shared.dir)?;
        }
        Ok(())
    }

    pub fn delete(&self, id: DocId) -> Result<bool> {
        {
            let mut buf = self.shared.buffer.lock().unwrap();
            if buf.contains(id) {
                self.shared.flush(&mut buf)?;
            }
        }
        let segs = self.shared.segments.read().unwrap();
        Ok(segs.iter().any(|s| s.delete_ext(id)))
    }

    pub fn search(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        let q = Query::parse(query, &self.shared.analyzer);
        self.search_query(&q, k)
    }

    pub fn search_query(&self, q: &Query, k: usize) -> Result<Vec<Hit>> {
        let snapshot = self.shared.segments.read().unwrap().clone();
        let params = Bm25 { k1: self.shared.cfg.bm25_k1, b: self.shared.cfg.bm25_b };
        search::execute(&snapshot, q, k, params)
    }

    /// Same as `search` but with pruning off. Results must match exactly; kept public for tests and benchmarks.
    #[doc(hidden)]
    pub fn search_unpruned(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        let q = Query::parse(query, &self.shared.analyzer);
        let snapshot = self.shared.segments.read().unwrap().clone();
        let params = Bm25 { k1: self.shared.cfg.bm25_k1, b: self.shared.cfg.bm25_b };
        search::execute_with(&snapshot, &q, k, params, false)
    }

    pub fn get(&self, id: DocId) -> Option<String> {
        let segs = self.shared.segments.read().unwrap();
        segs.iter().find_map(|s| s.text_of(id))
    }

    /// Merge everything into one segment, dropping deleted docs.
    pub fn force_merge(&self) -> Result<()> {
        self.commit()?;
        let _g = self.shared.merge_mu.lock().unwrap();
        let (src, snaps) = {
            let segs = self.shared.segments.read().unwrap();
            let needed = segs.len() > 1 || segs.iter().any(|s| s.deleted_count() > 0);
            if !needed {
                return Ok(());
            }
            (segs.clone(), segs.iter().map(|s| s.deleted_snapshot()).collect::<Vec<_>>())
        };
        self.shared.run_merge(src, snaps)?;
        drop(_g);
        self.commit()
    }

    pub fn stats(&self) -> Stats {
        let buffered = self.shared.buffer.lock().unwrap().len();
        let segs = self.shared.segments.read().unwrap();
        let mut st = Stats { segments: segs.len(), buffered_docs: buffered, ..Default::default() };
        for s in segs.iter() {
            st.live_docs += s.live_docs() as u64;
            st.deleted_docs += s.deleted_count() as u64;
            st.terms += s.terms().len() as u64;
            st.disk_bytes += s.byte_size() as u64;
        }
        st
    }

    pub fn last_merge_error(&self) -> Option<String> {
        self.shared.last_merge_err.lock().unwrap().clone()
    }

    /// Block until the background merger has nothing left to do (mostly for tests/benchmarks).
    pub fn wait_for_merges(&self) {
        loop {
            let armed = self.shared.ctl.lock().unwrap().wake;
            if !armed {
                let _g = self.shared.merge_mu.lock().unwrap();
                let live: Vec<u32> =
                    self.shared.segments.read().unwrap().iter().map(|s| s.live_docs()).collect();
                if self.shared.cfg.merge.select(&live).is_none() {
                    return;
                }
            }
            thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.ctl.lock().unwrap().shutdown = true;
        self.shared.cv.notify_all();
        if let Some(h) = self.merger.take() {
            let _ = h.join();
        }
    }
}

impl Shared {
    fn flush(&self, buf: &mut SegmentBuilder) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let b = std::mem::take(buf);
        let id = self.next_seg.fetch_add(1, Ordering::Relaxed);
        let bytes = b.build();
        write_atomic(&seg_path(&self.dir, id), &bytes)?;
        let seg = if bytes.len() as u64 > self.cfg.resident_limit {
            drop(bytes);
            Arc::new(Segment::open_with(&self.dir, id, self.cfg.resident_limit)?)
        } else {
            Arc::new(Segment::from_bytes(id, bytes)?)
        };

        self.segments.write().unwrap().push(seg);
        self.flushed_upto.store(self.next_doc.load(Ordering::Relaxed), Ordering::Relaxed);
        self.write_manifest()?;
        self.wal.lock().unwrap().reset()?;
        self.wake_merger();
        Ok(())
    }

    fn write_manifest(&self) -> Result<()> {
        let _g = self.manifest_mu.lock().unwrap();
        let mut text = format!(
            "next_doc {}\nnext_seg {}\nflushed_upto {}\n",
            self.next_doc.load(Ordering::Relaxed),
            self.next_seg.load(Ordering::Relaxed),
            self.flushed_upto.load(Ordering::Relaxed)
        );
        for s in self.segments.read().unwrap().iter() {
            text.push_str(&format!("segment {}\n", s.id()));
        }
        write_atomic(&self.dir.join(MANIFEST), text.as_bytes())
    }

    fn wake_merger(&self) {
        self.ctl.lock().unwrap().wake = true;
        self.cv.notify_one();
    }

    /// One policy-driven merge. Ok(false) means nothing to do.
    fn merge_once(&self) -> Result<bool> {
        let _g = self.merge_mu.lock().unwrap();
        let (src, snaps) = {
            let segs = self.segments.read().unwrap();
            let live: Vec<u32> = segs.iter().map(|s| s.live_docs()).collect();
            let Some(pick) = self.cfg.merge.select(&live) else { return Ok(false) };
            let src: Vec<_> = pick.iter().map(|&i| Arc::clone(&segs[i])).collect();
            let snaps = src.iter().map(|s| s.deleted_snapshot()).collect();
            (src, snaps)
        };
        self.run_merge(src, snaps)?;
        Ok(true)
    }

    // caller holds merge_mu
    fn run_merge(&self, src: Vec<Arc<Segment>>, snaps: Vec<Vec<u64>>) -> Result<()> {
        // heavy part: no engine locks held, readers and writers carry on
        let id = self.next_seg.fetch_add(1, Ordering::Relaxed);
        merge_segments(&src, &snaps, &seg_path(&self.dir, id))?;
        let merged = Arc::new(Segment::open_with(&self.dir, id, self.cfg.resident_limit)?);

        {
            // deletes take the read lock, so under the write lock none can slip in
            let mut segs = self.segments.write().unwrap();
            for (s, snap) in src.iter().zip(&snaps) {
                let now = s.deleted_snapshot();
                for local in 0..s.n_docs() {
                    if bit_set(&now, local) && !bit_set(snap, local) {
                        merged.delete_ext(s.doc(local).ext_id);
                    }
                }
            }
            let gone: HashSet<u64> = src.iter().map(|s| s.id()).collect();
            segs.retain(|s| !gone.contains(&s.id()));
            if merged.n_docs() > 0 {
                segs.push(Arc::clone(&merged));
            }
        }
        self.write_manifest()?;

        // readers holding the old Arcs are unaffected: they keep their bytes or file handle
        for s in &src {
            let _ = fs::remove_file(seg_path(&self.dir, s.id()));
            let _ = fs::remove_file(del_path(&self.dir, s.id()));
        }
        if merged.n_docs() == 0 {
            let _ = fs::remove_file(seg_path(&self.dir, id));
        }
        Ok(())
    }
}

fn merger_loop(s: Arc<Shared>) {
    loop {
        {
            let mut c = s.ctl.lock().unwrap();
            while !c.wake && !c.shutdown {
                c = s.cv.wait(c).unwrap();
            }
            if c.shutdown {
                return;
            }
            // cleared before working, so a flush that lands mid-merge re-arms it
            c.wake = false;
        }
        loop {
            if s.ctl.lock().unwrap().shutdown {
                return;
            }
            match s.merge_once() {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    *s.last_merge_err.lock().unwrap() = Some(e.to_string());
                    break;
                }
            }
        }
    }
}

// files a crashed flush/merge left behind: tmp files and segments the manifest doesn't know
fn remove_orphans(dir: &Path, live: &[u64]) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let orphan = if name.ends_with(".tmp") {
            true
        } else if let Some(stem) = name.strip_prefix("seg_") {
            stem.split('.').next().and_then(|n| n.parse::<u64>().ok()).is_some_and(|id| !live.contains(&id))
        } else {
            false
        };
        if orphan {
            let _ = fs::remove_file(e.path());
        }
    }
}
