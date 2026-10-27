//! Immutable on-disk segment.
//!
//! ```text
//! header   16B   "HIIS" | version u32 | n_docs u32 | n_terms u32
//! postings       concatenated per-term posting lists
//! terms          sorted: len, utf8, doc_freq, total_tf, off, len   (vbyte fields)
//! docs           per doc: ext_id, token_len, text_off, text_len    (vbyte fields)
//! store          raw document text
//! footer   40B   4 x u64 section offsets | crc32 of everything before | "SIIH"
//! ```
//!
//! Segments never change after they are written, apart from the side-car
//! `.del` bitmap that records deleted docs. Deleted docs are physically
//! dropped the next time their segment takes part in a merge.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;

use crate::codec::{crc32, vbyte};
use crate::error::{Error, Result};
use crate::index::postings::{EncodedPostings, PostingsCursor};
use crate::index::DocId;

const MAGIC: &[u8; 4] = b"HIIS";
const END_MAGIC: &[u8; 4] = b"SIIH";
const VERSION: u32 = 2; // 2: per-block max_tf in posting skip tables
const HEADER_LEN: usize = 16;
const FOOTER_LEN: usize = 40;

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
    text_off: u64,
    text_len: u32,
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
        self.docs.push(DocEntry {
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
        out.extend_from_slice(&(self.docs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.terms.len() as u32).to_le_bytes());

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

        let docs_off = out.len() as u64;
        for d in &self.docs {
            vbyte::write_u64(&mut out, d.ext_id);
            vbyte::write_u32(&mut out, d.len);
            vbyte::write_u64(&mut out, d.text_off);
            vbyte::write_u32(&mut out, d.text_len);
        }

        let store_off = out.len() as u64;
        out.extend_from_slice(&self.store);

        for off in [post_off, terms_off, docs_off, store_off] {
            out.extend_from_slice(&off.to_le_bytes());
        }
        let crc = crc32::checksum(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(END_MAGIC);
        out
    }
}

struct Deleted {
    bits: Vec<u64>,
    count: u32,
}

pub struct Segment {
    id: u64,
    data: Vec<u8>,
    post_range: (usize, usize),
    store_range: (usize, usize),
    terms: Vec<TermEntry>,
    docs: Vec<DocEntry>,
    by_ext: HashMap<DocId, u32>,
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
        if data.len() < HEADER_LEN + FOOTER_LEN {
            return Err(Error::Corrupt("segment too short"));
        }
        if &data[..4] != MAGIC || &data[data.len() - 4..] != END_MAGIC {
            return Err(Error::Corrupt("bad segment magic"));
        }
        if rd_u32(&data, 4) != VERSION {
            return Err(Error::Corrupt("unsupported segment version"));
        }
        let foot = data.len() - FOOTER_LEN;
        if crc32::checksum(&data[..foot + 32]) != rd_u32(&data, foot + 32) {
            return Err(Error::Corrupt("segment checksum mismatch"));
        }

        let n_docs = rd_u32(&data, 8) as usize;
        let n_terms = rd_u32(&data, 12) as usize;
        let post_off = rd_u64(&data, foot) as usize;
        let terms_off = rd_u64(&data, foot + 8) as usize;
        let docs_off = rd_u64(&data, foot + 16) as usize;
        let store_off = rd_u64(&data, foot + 24) as usize;
        if !(HEADER_LEN <= post_off
            && post_off <= terms_off
            && terms_off <= docs_off
            && docs_off <= store_off
            && store_off <= foot)
        {
            return Err(Error::Corrupt("segment section offsets"));
        }

        let post_len = terms_off - post_off;
        let store_len = foot - store_off;

        let mut terms = Vec::with_capacity(n_terms);
        let tsec = &data[..docs_off];
        let mut p = terms_off;
        for _ in 0..n_terms {
            let tl = vbyte::read_u32(tsec, &mut p)? as usize;
            let raw = tsec.get(p..p + tl).ok_or(Error::Corrupt("term bytes"))?;
            let term = std::str::from_utf8(raw).map_err(|_| Error::Corrupt("term not utf8"))?.to_string();
            p += tl;
            let doc_freq = vbyte::read_u32(tsec, &mut p)?;
            let total_tf = vbyte::read_u64(tsec, &mut p)?;
            let off = vbyte::read_u64(tsec, &mut p)? as usize;
            let len = vbyte::read_u64(tsec, &mut p)? as usize;
            if off.checked_add(len).is_none_or(|e| e > post_len) {
                return Err(Error::Corrupt("term postings out of range"));
            }
            terms.push(TermEntry { term, doc_freq, total_tf, off, len });
        }
        if terms.windows(2).any(|w| w[0].term >= w[1].term) {
            return Err(Error::Corrupt("terms not sorted"));
        }

        let mut docs = Vec::with_capacity(n_docs);
        let mut by_ext = HashMap::with_capacity(n_docs);
        let dsec = &data[..store_off];
        let mut p = docs_off;
        let mut total_len = 0u64;
        for i in 0..n_docs {
            let ext_id = vbyte::read_u64(dsec, &mut p)?;
            let len = vbyte::read_u32(dsec, &mut p)?;
            let text_off = vbyte::read_u64(dsec, &mut p)?;
            let text_len = vbyte::read_u32(dsec, &mut p)?;
            if text_off.checked_add(text_len as u64).is_none_or(|e| e > store_len as u64) {
                return Err(Error::Corrupt("doc text out of range"));
            }
            total_len += len as u64;
            by_ext.insert(ext_id, i as u32);
            docs.push(DocEntry { ext_id, len, text_off, text_len });
        }

        Ok(Segment {
            id,
            data,
            post_range: (post_off, terms_off),
            store_range: (store_off, foot),
            terms,
            docs,
            by_ext,
            deleted: RwLock::new(Deleted { bits: vec![0; n_docs.div_ceil(64)], count: 0 }),
            live_len: AtomicU64::new(total_len),
            del_dirty: AtomicBool::new(false),
        })
    }

    pub fn open(dir: &Path, id: u64) -> Result<Segment> {
        let seg = Segment::from_bytes(id, fs::read(seg_path(dir, id))?)?;
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
        self.data.len()
    }

    pub fn is_deleted(&self, local: u32) -> bool {
        let d = self.deleted.read().unwrap();
        d.bits[local as usize / 64] >> (local % 64) & 1 == 1
    }

    pub fn deleted_snapshot(&self) -> Vec<u64> {
        self.deleted.read().unwrap().bits.clone()
    }

    pub fn doc(&self, local: u32) -> &DocEntry {
        &self.docs[local as usize]
    }

    pub fn contains(&self, id: DocId) -> bool {
        self.by_ext.get(&id).is_some_and(|&l| !self.is_deleted(l))
    }

    pub fn text_of(&self, id: DocId) -> Option<&str> {
        let local = *self.by_ext.get(&id)?;
        if self.is_deleted(local) {
            return None;
        }
        self.text(local)
    }

    pub fn text(&self, local: u32) -> Option<&str> {
        let d = &self.docs[local as usize];
        let base = self.store_range.0;
        let bytes =
            self.data.get(base + d.text_off as usize..base + d.text_off as usize + d.text_len as usize)?;
        std::str::from_utf8(bytes).ok()
    }

    pub fn terms(&self) -> &[TermEntry] {
        &self.terms
    }

    pub fn term(&self, t: &str) -> Option<&TermEntry> {
        self.terms.binary_search_by(|e| e.term.as_str().cmp(t)).ok().map(|i| &self.terms[i])
    }

    pub fn cursor(&self, e: &TermEntry) -> Result<PostingsCursor<'_>> {
        let blob = &self.data[self.post_range.0..self.post_range.1];
        let cur = PostingsCursor::new(&blob[e.off..e.off + e.len])?;
        if cur.max_doc().is_some_and(|d| d as usize >= self.docs.len()) {
            return Err(Error::Corrupt("posting refers to a doc beyond the segment"));
        }
        Ok(cur)
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
        assert_eq!(seg.text_of(11), Some("beta"));

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
