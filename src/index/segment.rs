//! Immutable on-disk segment.
//!
//! ```text
//! header    8B   "HIIS" | version u32
//! postings       concatenated per-term posting lists
//! terms          sorted: len, utf8, doc_freq, total_tf, off, len   (vbyte fields)
//! store          document text, cut into 8 KiB blocks and LZ-compressed
//! docs           per doc: ext_id, token_len, text_off, text_len    (vbyte fields;
//!                offsets are into the uncompressed text stream), then the block
//!                table: total_len, n_blocks, compressed length of each block,
//!                then (v5) field lengths: n_names, names, per doc n pairs of (name, tokens)
//! footer   48B   n_docs u32 | n_terms u32 | 4 x u64 section offsets | crc32 of everything before | "SIIH"
//! ```
//!
//! The counts live in the footer so a segment can be written in one streaming
//! pass (merges do that) without going back to patch the header.
//!
//! Segments never change after they are written, apart from the side-car
//! `.del` bitmap that records deleted docs. Deleted docs are physically
//! dropped the next time their segment takes part in a merge.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use crate::codec::{crc32, lz, vbyte};
use crate::error::{Error, Result};
use crate::index::postings::{EncodedPostings, PostingsCursor};
use crate::index::DocId;

const MAGIC: &[u8; 4] = b"HIIS";
const END_MAGIC: &[u8; 4] = b"SIIH";
const VERSION: u32 = 5; // 5: per-field token counts (v4 still opens); 4: compressed text store; 3: counts in the footer; 2: per-block max_tf
const HEADER_LEN: usize = 8;
const FOOTER_LEN: usize = 48;
const BLOCK: usize = 8 << 10;

pub fn seg_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("seg_{id:08}.hiis"))
}

pub fn del_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("seg_{id:08}.del"))
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let f = fs::File::create(&tmp)?;
        use std::io::Write;
        let mut w = std::io::BufWriter::new(&f);
        w.write_all(bytes)?;
        w.flush()?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct DocEntry {
    pub ext_id: DocId,
    pub len: u32,
    pub(crate) text_off: u64,
    pub(crate) text_len: u32,
    /// tokens per named field, empty for plain documents
    pub flens: Vec<(String, u32)>,
}

#[derive(Debug, Clone)]
pub struct TermEntry {
    pub term: String,
    pub doc_freq: u32,
    pub total_tf: u64,
    off: usize,
    len: usize,
}

#[derive(Default)]
pub struct SegmentWriter {
    docs: Vec<DocEntry>,
    store: Vec<u8>,
    terms: Vec<TermEntry>,
    postings: Vec<u8>,
}

impl SegmentWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_doc(&mut self, ext_id: DocId, len: u32, text: &str) {
        self.add_doc_fields(ext_id, len, Vec::new(), text);
    }

    pub fn add_doc_fields(&mut self, ext_id: DocId, len: u32, flens: Vec<(String, u32)>, text: &str) {
        self.docs.push(DocEntry {
            flens,
            ext_id,
            len,
            text_off: self.store.len() as u64,
            text_len: text.len() as u32,
        });
        self.store.extend_from_slice(text.as_bytes());
    }

    /// Terms have to come in ascending byte order, the reader binary-searches them.
    pub fn add_term(&mut self, term: &str, p: EncodedPostings) {
        debug_assert!(self.terms.last().is_none_or(|t| t.term.as_str() < term));
        self.terms.push(TermEntry {
            term: term.to_string(),
            doc_freq: p.doc_freq,
            total_tf: p.total_tf,
            off: self.postings.len(),
            len: p.bytes.len(),
        });
        self.postings.extend_from_slice(&p.bytes);
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(HEADER_LEN + self.postings.len() + self.store.len() + FOOTER_LEN + 1024);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());

        let post_off = out.len() as u64;
        out.extend_from_slice(&self.postings);

        let terms_off = out.len() as u64;
        for t in &self.terms {
            vbyte::write_u32(&mut out, t.term.len() as u32);
            out.extend_from_slice(t.term.as_bytes());
            vbyte::write_u32(&mut out, t.doc_freq);
            vbyte::write_u64(&mut out, t.total_tf);
            vbyte::write_u64(&mut out, t.off as u64);
            vbyte::write_u64(&mut out, t.len as u64);
        }

        let store_off = out.len() as u64;
        let mut clens = Vec::new();
        for chunk in self.store.chunks(BLOCK) {
            let before = out.len();
            lz::compress(chunk, &mut out);
            clens.push((out.len() - before) as u32);
        }

        let docs_off = out.len() as u64;
        let table = self.docs.iter().map(|d| (d.ext_id, d.len, d.text_off, d.text_len));
        write_doc_table(&mut out, table, self.store.len() as u64, &clens);
        write_field_section(&mut out, self.docs.iter().map(|d| d.flens.as_slice()));

        out.extend_from_slice(&(self.docs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.terms.len() as u32).to_le_bytes());
        for off in [post_off, terms_off, store_off, docs_off] {
            out.extend_from_slice(&off.to_le_bytes());
        }
        let crc = crc32::checksum(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(END_MAGIC);
        out
    }
}

fn write_doc_table(
    out: &mut Vec<u8>,
    docs: impl Iterator<Item = (DocId, u32, u64, u32)>,
    total_len: u64,
    clens: &[u32],
) {
    for (ext_id, len, text_off, text_len) in docs {
        vbyte::write_u64(out, ext_id);
        vbyte::write_u32(out, len);
        vbyte::write_u64(out, text_off);
        vbyte::write_u32(out, text_len);
    }
    vbyte::write_u64(out, total_len);
    vbyte::write_u32(out, clens.len() as u32);
    for &c in clens {
        vbyte::write_u32(out, c);
    }
}

fn write_field_section<'a>(out: &mut Vec<u8>, docs: impl Iterator<Item = &'a [(String, u32)]> + Clone) {
    let mut names: Vec<&str> = docs.clone().flatten().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    vbyte::write_u32(out, names.len() as u32);
    for n in &names {
        vbyte::write_u32(out, n.len() as u32);
        out.extend_from_slice(n.as_bytes());
    }
    for d in docs {
        vbyte::write_u32(out, d.len() as u32);
        for (n, l) in d {
            vbyte::write_u32(out, names.binary_search(&n.as_str()).unwrap() as u32);
            vbyte::write_u32(out, *l);
        }
    }
}

// remembers the last decompressed block; merges read docs in order, so
// neighbours share it
pub(crate) struct BlockCache {
    idx: usize,
    data: Vec<u8>,
}

impl Default for BlockCache {
    fn default() -> Self {
        BlockCache { idx: usize::MAX, data: Vec::new() }
    }
}

struct Deleted {
    bits: Vec<u64>,
    count: u32,
}

// resident segments are one Vec; big ones are read on demand with positional
// reads (mmap would need unsafe), and the OS page cache does the caching
enum Backing {
    Mem(Vec<u8>),
    File(fs::File),
}

#[cfg(unix)]
fn read_exact_at(f: &fs::File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
}

#[cfg(windows)]
fn read_exact_at(f: &fs::File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = std::os::windows::fs::FileExt::seek_read(f, &mut buf[done..], off + done as u64)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

impl Backing {
    fn read(&self, off: usize, len: usize) -> Result<Cow<'_, [u8]>> {
        match self {
            Backing::Mem(v) => off
                .checked_add(len)
                .and_then(|end| v.get(off..end))
                .map(Cow::Borrowed)
                .ok_or(Error::Corrupt("read past end of segment")),
            Backing::File(f) => {
                let mut buf = vec![0u8; len];
                read_exact_at(f, &mut buf, off as u64)?;
                Ok(Cow::Owned(buf))
            }
        }
    }

    // crc of the first `n` bytes
    fn crc_prefix(&self, n: usize) -> Result<u32> {
        match self {
            Backing::Mem(v) => Ok(crc32::checksum(&v[..n])),
            Backing::File(f) => {
                let mut crc = crc32::Crc::new();
                let mut buf = vec![0u8; 1 << 20];
                let mut at = 0;
                while at < n {
                    let take = buf.len().min(n - at);
                    read_exact_at(f, &mut buf[..take], at as u64)?;
                    crc.update(&buf[..take]);
                    at += take;
                }
                Ok(crc.finish())
            }
        }
    }
}

pub struct Segment {
    id: u64,
    backing: Backing,
    size: usize,
    post_off: usize,
    store_off: usize,
    // (start relative to store_off, compressed length) per text block
    blocks: Vec<(usize, usize)>,
    store_len: usize,
    terms: Vec<TermEntry>,
    docs: Vec<DocEntry>,
    by_ext: HashMap<DocId, u32>,
    // field -> (tokens, docs that have it), over the live docs
    field_totals: Mutex<HashMap<String, (u64, u64)>>,
    deleted: RwLock<Deleted>,
    live_len: AtomicU64,
    del_dirty: AtomicBool,
}

fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn rd_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

impl Segment {
    pub fn from_bytes(id: u64, data: Vec<u8>) -> Result<Segment> {
        let size = data.len();
        Segment::load(id, Backing::Mem(data), size)
    }

    fn load(id: u64, backing: Backing, size: usize) -> Result<Segment> {
        if size < HEADER_LEN + FOOTER_LEN {
            return Err(Error::Corrupt("segment too short"));
        }
        let head = backing.read(0, HEADER_LEN)?;
        if &head[..4] != MAGIC {
            return Err(Error::Corrupt("bad segment magic"));
        }
        let version = rd_u32(&head, 4);
        if version != VERSION && version != 4 {
            return Err(Error::Corrupt("unsupported segment version"));
        }
        let foot_at = size - FOOTER_LEN;
        let foot = backing.read(foot_at, FOOTER_LEN)?;
        if &foot[FOOTER_LEN - 4..] != END_MAGIC {
            return Err(Error::Corrupt("bad segment magic"));
        }
        if backing.crc_prefix(foot_at + 40)? != rd_u32(&foot, 40) {
            return Err(Error::Corrupt("segment checksum mismatch"));
        }

        let n_docs = rd_u32(&foot, 0) as usize;
        let n_terms = rd_u32(&foot, 4) as usize;
        let post_off = rd_u64(&foot, 8) as usize;
        let terms_off = rd_u64(&foot, 16) as usize;
        let store_off = rd_u64(&foot, 24) as usize;
        let docs_off = rd_u64(&foot, 32) as usize;
        if !(HEADER_LEN <= post_off
            && post_off <= terms_off
            && terms_off <= store_off
            && store_off <= docs_off
            && docs_off <= foot_at)
        {
            return Err(Error::Corrupt("segment section offsets"));
        }

        let post_len = terms_off - post_off;

        // the term dictionary and doc table are always resident; only
        // postings and stored text can stay on disk
        let tsec = backing.read(terms_off, store_off - terms_off)?;
        let mut terms = Vec::with_capacity(n_terms.min(tsec.len()));
        let mut p = 0;
        for _ in 0..n_terms {
            let tl = vbyte::read_u32(&tsec, &mut p)? as usize;
            let raw = tsec.get(p..p.saturating_add(tl)).ok_or(Error::Corrupt("term bytes"))?;
            let term = std::str::from_utf8(raw).map_err(|_| Error::Corrupt("term not utf8"))?.to_string();
            p += tl;
            let doc_freq = vbyte::read_u32(&tsec, &mut p)?;
            let total_tf = vbyte::read_u64(&tsec, &mut p)?;
            let off = vbyte::read_u64(&tsec, &mut p)? as usize;
            let len = vbyte::read_u64(&tsec, &mut p)? as usize;
            if off.checked_add(len).is_none_or(|e| e > post_len) {
                return Err(Error::Corrupt("term postings out of range"));
            }
            terms.push(TermEntry { term, doc_freq, total_tf, off, len });
        }
        if terms.windows(2).any(|w| w[0].term >= w[1].term) {
            return Err(Error::Corrupt("terms not sorted"));
        }
        drop(tsec);

        let dsec = backing.read(docs_off, foot_at - docs_off)?;
        let mut raw_docs = Vec::with_capacity(n_docs.min(dsec.len()));
        let mut p = 0;
        for _ in 0..n_docs {
            let ext_id = vbyte::read_u64(&dsec, &mut p)?;
            let len = vbyte::read_u32(&dsec, &mut p)?;
            let text_off = vbyte::read_u64(&dsec, &mut p)?;
            let text_len = vbyte::read_u32(&dsec, &mut p)?;
            raw_docs.push((ext_id, len, text_off, text_len));
        }
        let store_len = vbyte::read_u64(&dsec, &mut p)? as usize;
        let n_blocks = vbyte::read_u32(&dsec, &mut p)? as usize;
        if n_blocks != store_len.div_ceil(BLOCK) {
            return Err(Error::Corrupt("text block count mismatch"));
        }
        let mut blocks = Vec::with_capacity(n_blocks.min(dsec.len()));
        let mut at = 0usize;
        for _ in 0..n_blocks {
            let clen = vbyte::read_u32(&dsec, &mut p)? as usize;
            blocks.push((at, clen));
            at = at.checked_add(clen).ok_or(Error::Corrupt("text block table overflow"))?;
        }
        if at != docs_off - store_off {
            return Err(Error::Corrupt("text block table disagrees with store size"));
        }

        let mut flens: Vec<Vec<(String, u32)>> = vec![Vec::new(); n_docs];
        if version >= 5 {
            let n_names = vbyte::read_u32(&dsec, &mut p)? as usize;
            if n_names > dsec.len() {
                return Err(Error::Corrupt("field name count"));
            }
            let mut names = Vec::with_capacity(n_names);
            for _ in 0..n_names {
                let l = vbyte::read_u32(&dsec, &mut p)? as usize;
                let b = p.checked_add(l).and_then(|e| dsec.get(p..e)).ok_or(Error::Corrupt("field name"))?;
                names.push(String::from_utf8(b.to_vec()).map_err(|_| Error::Corrupt("field name utf8"))?);
                p += l;
            }
            for slot in flens.iter_mut() {
                let n = vbyte::read_u32(&dsec, &mut p)? as usize;
                if n > n_names {
                    return Err(Error::Corrupt("field pair count"));
                }
                for _ in 0..n {
                    let i = vbyte::read_u32(&dsec, &mut p)? as usize;
                    let l = vbyte::read_u32(&dsec, &mut p)?;
                    let name = names.get(i).ok_or(Error::Corrupt("field index"))?;
                    slot.push((name.clone(), l));
                }
            }
        }
        drop(dsec);

        let mut field_totals: HashMap<String, (u64, u64)> = HashMap::new();
        let mut docs = Vec::with_capacity(raw_docs.len());
        let mut by_ext = HashMap::with_capacity(raw_docs.len());
        let mut total_len = 0u64;
        for (i, ((ext_id, len, text_off, text_len), flens)) in raw_docs.into_iter().zip(flens).enumerate() {
            if text_off.checked_add(text_len as u64).is_none_or(|e| e > store_len as u64) {
                return Err(Error::Corrupt("doc text out of range"));
            }
            total_len += len as u64;
            by_ext.insert(ext_id, i as u32);
            for (n, l) in &flens {
                let t = field_totals.entry(n.clone()).or_default();
                t.0 += *l as u64;
                t.1 += 1;
            }
            docs.push(DocEntry { ext_id, len, text_off, text_len, flens });
        }

        Ok(Segment {
            id,
            backing,
            size,
            post_off,
            store_off,
            blocks,
            store_len,
            terms,
            docs,
            by_ext,
            field_totals: Mutex::new(field_totals),
            deleted: RwLock::new(Deleted { bits: vec![0; n_docs.div_ceil(64)], count: 0 }),
            live_len: AtomicU64::new(total_len),
            del_dirty: AtomicBool::new(false),
        })
    }

    pub fn open(dir: &Path, id: u64) -> Result<Segment> {
        Segment::open_with(dir, id, u64::MAX)
    }

    /// Files up to `resident_limit` bytes are read into memory, bigger ones
    /// stay on disk and are read per query.
    pub fn open_with(dir: &Path, id: u64, resident_limit: u64) -> Result<Segment> {
        let path = seg_path(dir, id);
        let size = fs::metadata(&path)?.len();
        let seg = if size <= resident_limit {
            Segment::from_bytes(id, fs::read(&path)?)?
        } else {
            Segment::load(id, Backing::File(fs::File::open(&path)?), size as usize)?
        };
        seg.load_deletes(&del_path(dir, id))?;
        Ok(seg)
    }

    fn load_deletes(&self, path: &Path) -> Result<()> {
        let raw = match fs::read(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let words = self.docs.len().div_ceil(64);
        if raw.len() != 4 + words * 8 + 4 || rd_u32(&raw, 0) as usize != self.docs.len() {
            return Err(Error::Corrupt("delete bitmap shape"));
        }
        if crc32::checksum(&raw[..raw.len() - 4]) != rd_u32(&raw, raw.len() - 4) {
            return Err(Error::Corrupt("delete bitmap checksum"));
        }
        for i in 0..words {
            let w = rd_u64(&raw, 4 + i * 8);
            for bit in 0..64 {
                let local = i * 64 + bit;
                if w >> bit & 1 == 1 && local < self.docs.len() {
                    self.mark_deleted(local as u32);
                }
            }
        }
        self.del_dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn persist_deletes(&self, dir: &Path) -> Result<()> {
        if !self.del_dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let d = self.deleted.read().unwrap();
        let mut raw = Vec::with_capacity(8 + d.bits.len() * 8);
        raw.extend_from_slice(&(self.docs.len() as u32).to_le_bytes());
        for w in &d.bits {
            raw.extend_from_slice(&w.to_le_bytes());
        }
        let crc = crc32::checksum(&raw);
        raw.extend_from_slice(&crc.to_le_bytes());
        write_atomic(&del_path(dir, self.id), &raw)
    }

    fn mark_deleted(&self, local: u32) -> bool {
        let mut d = self.deleted.write().unwrap();
        let (w, b) = (local as usize / 64, local % 64);
        if d.bits[w] >> b & 1 == 1 {
            return false;
        }
        d.bits[w] |= 1 << b;
        d.count += 1;
        self.live_len.fetch_sub(self.docs[local as usize].len as u64, Ordering::Relaxed);
        let flens = &self.docs[local as usize].flens;
        if !flens.is_empty() {
            let mut t = self.field_totals.lock().unwrap();
            for (n, l) in flens {
                if let Some(e) = t.get_mut(n) {
                    e.0 -= *l as u64;
                    e.1 -= 1;
                }
            }
        }
        self.del_dirty.store(true, Ordering::Release);
        true
    }

    pub fn delete_ext(&self, id: DocId) -> bool {
        match self.by_ext.get(&id) {
            Some(&local) => self.mark_deleted(local),
            None => false,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn n_docs(&self) -> u32 {
        self.docs.len() as u32
    }

    pub fn deleted_count(&self) -> u32 {
        self.deleted.read().unwrap().count
    }

    pub fn live_docs(&self) -> u32 {
        self.n_docs() - self.deleted_count()
    }

    pub fn live_token_len(&self) -> u64 {
        self.live_len.load(Ordering::Relaxed)
    }

    pub fn byte_size(&self) -> usize {
        self.size
    }

    pub fn is_deleted(&self, local: u32) -> bool {
        let d = self.deleted.read().unwrap();
        d.bits[local as usize / 64] >> (local % 64) & 1 == 1
    }

    pub fn deleted_snapshot(&self) -> Vec<u64> {
        self.deleted.read().unwrap().bits.clone()
    }

    /// Tokens `local` has in a named field (None when it has none, or the segment predates field lengths).
    pub fn field_len(&self, local: u32, field: &str) -> Option<u32> {
        self.docs[local as usize].flens.iter().find(|(n, _)| n == field).map(|x| x.1)
    }

    pub fn field_totals(&self) -> HashMap<String, (u64, u64)> {
        self.field_totals.lock().unwrap().clone()
    }

    pub fn doc(&self, local: u32) -> &DocEntry {
        &self.docs[local as usize]
    }

    /// Local number of a live document.
    pub fn local_of(&self, id: DocId) -> Option<u32> {
        self.by_ext.get(&id).copied().filter(|&l| !self.is_deleted(l))
    }

    pub fn contains(&self, id: DocId) -> bool {
        self.by_ext.get(&id).is_some_and(|&l| !self.is_deleted(l))
    }

    pub fn text_of(&self, id: DocId) -> Option<String> {
        let local = *self.by_ext.get(&id)?;
        if self.is_deleted(local) {
            return None;
        }
        self.text(local)
    }

    /// The text as readers see it: fielded documents come back as their values, one per line.
    pub fn text(&self, local: u32) -> Option<String> {
        self.raw_text(local).map(|t| crate::fields::plain(&t))
    }

    /// The stored form, field markers included.
    pub fn raw_text(&self, local: u32) -> Option<String> {
        String::from_utf8(self.text_bytes(local).ok()?).ok()
    }

    pub fn raw_text_of(&self, id: DocId) -> Option<String> {
        self.raw_text(self.local_of(id)?)
    }

    pub(crate) fn text_bytes(&self, local: u32) -> Result<Vec<u8>> {
        self.text_bytes_with(local, &mut BlockCache::default())
    }

    pub(crate) fn text_bytes_with(&self, local: u32, cache: &mut BlockCache) -> Result<Vec<u8>> {
        let d = &self.docs[local as usize];
        let (end, mut at) = ((d.text_off + d.text_len as u64) as usize, d.text_off as usize);
        let mut out = Vec::with_capacity((d.text_len as usize).min(1 << 20));
        while at < end {
            let b = at / BLOCK;
            if cache.idx != b {
                let (off, clen) = self.blocks[b];
                let raw = self.backing.read(self.store_off + off, clen)?;
                cache.data = lz::decompress(&raw, BLOCK.min(self.store_len - b * BLOCK))?;
                cache.idx = b;
            }
            let from = at - b * BLOCK;
            let take = (cache.data.len() - from).min(end - at);
            out.extend_from_slice(&cache.data[from..from + take]);
            at += take;
        }
        Ok(out)
    }

    pub fn terms(&self) -> &[TermEntry] {
        &self.terms
    }

    pub fn term(&self, t: &str) -> Option<&TermEntry> {
        self.terms.binary_search_by(|e| e.term.as_str().cmp(t)).ok().map(|i| &self.terms[i])
    }

    pub fn cursor(&self, e: &TermEntry) -> Result<PostingsCursor<'_>> {
        let cur = PostingsCursor::from_cow(self.backing.read(self.post_off + e.off, e.len)?)?;
        if cur.max_doc().is_some_and(|d| d as usize >= self.docs.len()) {
            return Err(Error::Corrupt("posting refers to a doc beyond the segment"));
        }
        Ok(cur)
    }
}

/// Writes a segment front to back without holding it in memory: postings go
/// out as each term is finished, the term and doc tables (small) at the end.
pub struct StreamWriter {
    out: BufWriter<fs::File>,
    crc: crc32::Crc,
    pos: u64,
    tmp: PathBuf,
    path: PathBuf,
    post_off: u64,
    terms: Vec<TermEntry>,
}

impl StreamWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let tmp = path.with_extension("tmp");
        let file = fs::File::create(&tmp)?;
        let mut w = StreamWriter {
            out: BufWriter::with_capacity(1 << 20, file),
            crc: crc32::Crc::new(),
            pos: 0,
            tmp,
            path: path.to_path_buf(),
            post_off: 0,
            terms: Vec::new(),
        };
        w.put(MAGIC)?;
        w.put(&VERSION.to_le_bytes())?;
        w.post_off = w.pos;
        Ok(w)
    }

    fn put(&mut self, b: &[u8]) -> Result<()> {
        self.out.write_all(b)?;
        self.crc.update(b);
        self.pos += b.len() as u64;
        Ok(())
    }

    fn put_block(&mut self, chunk: &[u8], clens: &mut Vec<u32>) -> Result<()> {
        let mut c = Vec::with_capacity(chunk.len() / 2);
        lz::compress(chunk, &mut c);
        clens.push(c.len() as u32);
        self.put(&c)
    }

    /// Same ordering rule as `SegmentWriter::add_term`.
    pub fn add_term(&mut self, term: &str, p: EncodedPostings) -> Result<()> {
        self.terms.push(TermEntry {
            term: term.to_string(),
            doc_freq: p.doc_freq,
            total_tf: p.total_tf,
            off: (self.pos - self.post_off) as usize,
            len: p.bytes.len(),
        });
        self.put(&p.bytes)
    }

    /// `docs` is (ext_id, token_len, text_len, field lengths) per doc; `text(i)` returns the
    /// raw stored bytes of doc i, which must be exactly `text_len` long.
    pub fn finish(
        mut self,
        docs: &[(DocId, u32, u32, Vec<(String, u32)>)],
        mut text: impl FnMut(usize) -> Result<Vec<u8>>,
    ) -> Result<()> {
        let terms_off = self.pos;
        let terms = std::mem::take(&mut self.terms);
        let n_terms = terms.len() as u32;
        let mut sec = Vec::new();
        for t in terms {
            vbyte::write_u32(&mut sec, t.term.len() as u32);
            sec.extend_from_slice(t.term.as_bytes());
            vbyte::write_u32(&mut sec, t.doc_freq);
            vbyte::write_u64(&mut sec, t.total_tf);
            vbyte::write_u64(&mut sec, t.off as u64);
            vbyte::write_u64(&mut sec, t.len as u64);
        }
        self.put(&sec)?;

        let store_off = self.pos;
        let mut clens = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut total = 0u64;
        for (i, &(_, _, text_len, _)) in docs.iter().enumerate() {
            let bytes = text(i)?;
            if bytes.len() != text_len as usize {
                return Err(Error::Corrupt("stored text length changed during merge"));
            }
            total += text_len as u64;
            pending.extend_from_slice(&bytes);
            while pending.len() >= BLOCK {
                self.put_block(&pending[..BLOCK], &mut clens)?;
                pending.drain(..BLOCK);
            }
        }
        if !pending.is_empty() {
            self.put_block(&pending, &mut clens)?;
        }

        let docs_off = self.pos;
        let mut sec = Vec::new();
        let mut text_off = 0u64;
        let table = docs.iter().map(|&(ext_id, len, text_len, _)| {
            let at = text_off;
            text_off += text_len as u64;
            (ext_id, len, at, text_len)
        });
        write_doc_table(&mut sec, table, total, &clens);
        write_field_section(&mut sec, docs.iter().map(|d| d.3.as_slice()));
        self.put(&sec)?;

        let mut foot = Vec::with_capacity(FOOTER_LEN);
        foot.extend_from_slice(&(docs.len() as u32).to_le_bytes());
        foot.extend_from_slice(&n_terms.to_le_bytes());
        for off in [self.post_off, terms_off, store_off, docs_off] {
            foot.extend_from_slice(&off.to_le_bytes());
        }
        self.put(&foot)?;
        let crc = self.crc.finish();
        self.out.write_all(&crc.to_le_bytes())?;
        self.out.write_all(END_MAGIC)?;
        self.out.flush()?;
        self.out.get_ref().sync_all()?;
        fs::rename(&self.tmp, &self.path)?;
        Ok(())
    }
}

pub fn bit_set(bits: &[u64], local: u32) -> bool {
    bits.get(local as usize / 64).is_some_and(|w| w >> (local % 64) & 1 == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::postings::PostingsWriter;

    fn tiny() -> Vec<u8> {
        let mut w = SegmentWriter::new();
        w.add_doc(10, 3, "alpha beta");
        w.add_doc(11, 1, "beta");
        let mut a = PostingsWriter::new();
        a.add(0, &[0]);
        w.add_term("alpha", a.finish());
        let mut b = PostingsWriter::new();
        b.add(0, &[1]);
        b.add(1, &[0]);
        w.add_term("beta", b.finish());
        w.finish()
    }

    #[test]
    fn write_then_read() {
        let seg = Segment::from_bytes(1, tiny()).unwrap();
        assert_eq!(seg.n_docs(), 2);
        assert_eq!(seg.live_token_len(), 4);
        assert_eq!(seg.term("beta").unwrap().doc_freq, 2);
        assert!(seg.term("gamma").is_none());
        assert_eq!(seg.text_of(11).as_deref(), Some("beta"));

        let mut c = seg.cursor(seg.term("beta").unwrap()).unwrap();
        assert_eq!(c.next().unwrap(), Some(0));
        assert_eq!(c.next().unwrap(), Some(1));
        assert_eq!(c.next().unwrap(), None);
    }

    #[test]
    fn delete_updates_counts() {
        let seg = Segment::from_bytes(1, tiny()).unwrap();
        assert!(seg.delete_ext(10));
        assert!(!seg.delete_ext(10));
        assert!(!seg.delete_ext(99));
        assert_eq!(seg.live_docs(), 1);
        assert_eq!(seg.live_token_len(), 1);
        assert!(seg.text_of(10).is_none());
    }

    #[test]
    fn any_flipped_byte_is_caught() {
        let good = tiny();
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x40;
            assert!(Segment::from_bytes(1, bad).is_err(), "byte {i}");
        }
    }

    #[test]
    fn truncation_is_caught() {
        let good = tiny();
        for cut in [0, 10, good.len() - 1] {
            assert!(Segment::from_bytes(1, good[..cut].to_vec()).is_err());
        }
    }
}
