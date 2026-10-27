//! Posting list encoding and the cursor that reads it back.
//!
//! One term's list is laid out as
//!
//! ```text
//! n_docs: vbyte | n_blocks: vbyte
//! skip table:  n_blocks x { last_doc: vbyte, block_len: vbyte, max_tf: vbyte }
//! blocks:      n_blocks x block
//!
//! block := df_len: vbyte
//!          doc deltas + term freqs   (df_len bytes; FoR if 128 docs, else vbyte)
//!          positions                 (per doc: freq vbyte-coded gaps)
//! ```
//!
//! Doc ids are segment-local and ascending; the first delta of a block is
//! relative to the previous block's last doc (0 for the first block). Keeping
//! the positions behind the doc/freq section means a pure ranking query never
//! touches them, and the skip table lets `advance` hop over whole blocks
//! without decoding. `max_tf` per block feeds the score upper bounds used by
//! WAND pruning in `search`.

use crate::codec::bitpack::{self, BLOCK_LEN};
use crate::codec::vbyte;
use crate::error::{Error, Result};

pub struct EncodedPostings {
    pub bytes: Vec<u8>,
    pub doc_freq: u32,
    pub total_tf: u64,
}

#[derive(Default)]
pub struct PostingsWriter {
    blocks: Vec<u8>,
    skips: Vec<(u32, u32, u32)>,
    docs: Vec<u32>,
    freqs: Vec<u32>,
    pos: Vec<u8>,
    prev_last: u32,
    n_docs: u32,
    total_tf: u64,
}

impl PostingsWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// `positions` must be ascending and non-empty; docs must arrive in ascending order.
    pub fn add(&mut self, doc: u32, positions: &[u32]) {
        debug_assert!(!positions.is_empty());
        debug_assert!(self.n_docs == 0 || doc > self.prev_last.max(*self.docs.last().unwrap_or(&0)));

        let mut prev = 0;
        for &p in positions {
            vbyte::write_u32(&mut self.pos, p - prev);
            prev = p;
        }
        self.docs.push(doc);
        self.freqs.push(positions.len() as u32);
        self.n_docs += 1;
        self.total_tf += positions.len() as u64;

        if self.docs.len() == BLOCK_LEN {
            self.flush_block();
        }
    }

    fn flush_block(&mut self) {
        let start = self.blocks.len();

        let mut prev = self.prev_last;
        let deltas: Vec<u32> = self
            .docs
            .iter()
            .map(|&d| {
                let delta = d - prev;
                prev = d;
                delta
            })
            .collect();

        let mut head = Vec::new();
        if self.docs.len() == BLOCK_LEN {
            bitpack::pack(&deltas, &mut head);
            bitpack::pack(&self.freqs, &mut head);
        } else {
            deltas.iter().for_each(|&d| vbyte::write_u32(&mut head, d));
            self.freqs.iter().for_each(|&f| vbyte::write_u32(&mut head, f));
        }

        vbyte::write_u32(&mut self.blocks, head.len() as u32);
        self.blocks.extend_from_slice(&head);
        self.blocks.extend_from_slice(&self.pos);

        let last = *self.docs.last().unwrap();
        let max_tf = self.freqs.iter().copied().max().unwrap_or(0);
        self.skips.push((last, (self.blocks.len() - start) as u32, max_tf));
        self.prev_last = last;
        self.docs.clear();
        self.freqs.clear();
        self.pos.clear();
    }

    pub fn finish(mut self) -> EncodedPostings {
        if !self.docs.is_empty() {
            self.flush_block();
        }
        let mut bytes = Vec::with_capacity(self.blocks.len() + self.skips.len() * 4 + 8);
        vbyte::write_u32(&mut bytes, self.n_docs);
        vbyte::write_u32(&mut bytes, self.skips.len() as u32);
        for &(last, len, max_tf) in &self.skips {
            vbyte::write_u32(&mut bytes, last);
            vbyte::write_u32(&mut bytes, len);
            vbyte::write_u32(&mut bytes, max_tf);
        }
        bytes.extend_from_slice(&self.blocks);
        EncodedPostings { bytes, doc_freq: self.n_docs, total_tf: self.total_tf }
    }
}

struct BlockInfo {
    last_doc: u32,
    max_tf: u32,
    start: usize,
    len: usize,
}

const NO_BLOCK: usize = usize::MAX;

pub struct PostingsCursor<'a> {
    data: &'a [u8],
    n_docs: u32,
    blocks: Vec<BlockInfo>,
    cur_blk: usize,
    done: bool,

    docs: Vec<u32>,
    freqs: Vec<u32>,
    idx: usize,

    // positions of the loaded block are decoded lazily: pos_off is the byte
    // offset where the doc at index pos_idx starts
    pos_bytes: &'a [u8],
    pos_off: usize,
    pos_idx: usize,
}

impl<'a> PostingsCursor<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        let mut p = 0;
        let n_docs = vbyte::read_u32(data, &mut p)?;
        let n_blocks = vbyte::read_u32(data, &mut p)? as usize;
        if n_blocks != (n_docs as usize).div_ceil(BLOCK_LEN) {
            return Err(Error::Corrupt("posting list block count mismatch"));
        }

        let mut raw = Vec::with_capacity(n_blocks);
        for _ in 0..n_blocks {
            let last = vbyte::read_u32(data, &mut p)?;
            let len = vbyte::read_u32(data, &mut p)? as usize;
            let max_tf = vbyte::read_u32(data, &mut p)?;
            raw.push((last, len, max_tf));
        }

        let mut blocks = Vec::with_capacity(n_blocks);
        let mut start = p;
        for (last_doc, len, max_tf) in raw {
            let end = start.checked_add(len).filter(|&e| e <= data.len());
            if end.is_none() {
                return Err(Error::Corrupt("posting block out of bounds"));
            }
            blocks.push(BlockInfo { last_doc, max_tf, start, len });
            start += len;
        }

        Ok(PostingsCursor {
            data,
            n_docs,
            blocks,
            cur_blk: NO_BLOCK,
            done: false,
            docs: Vec::with_capacity(BLOCK_LEN),
            freqs: Vec::with_capacity(BLOCK_LEN),
            idx: 0,
            pos_bytes: &[],
            pos_off: 0,
            pos_idx: 0,
        })
    }

    pub fn doc_freq(&self) -> u32 {
        self.n_docs
    }

    /// Largest doc id in the list (from the skip table), None if empty.
    pub fn max_doc(&self) -> Option<u32> {
        self.blocks.last().map(|b| b.last_doc)
    }

    /// Highest tf anywhere in the list.
    pub fn max_tf(&self) -> u32 {
        self.blocks.iter().map(|b| b.max_tf).max().unwrap_or(0)
    }

    /// Highest tf in the block the cursor currently sits in (0 before the first `next`).
    pub fn block_max_tf(&self) -> u32 {
        self.blocks.get(self.cur_blk).map_or(0, |b| b.max_tf)
    }

    /// Last doc id covered by the current block.
    pub fn block_last_doc(&self) -> u32 {
        self.blocks.get(self.cur_blk).map_or(0, |b| b.last_doc)
    }

    pub fn doc(&self) -> Option<u32> {
        if self.done || self.cur_blk == NO_BLOCK {
            return None;
        }
        self.docs.get(self.idx).copied()
    }

    pub fn freq(&self) -> u32 {
        self.freqs[self.idx]
    }

    fn load_block(&mut self, b: usize) -> Result<()> {
        let info = &self.blocks[b];
        let base = if b == 0 { 0 } else { self.blocks[b - 1].last_doc };
        let count = if b + 1 < self.blocks.len() { BLOCK_LEN } else { self.n_docs as usize - b * BLOCK_LEN };
        let blk = &self.data[info.start..info.start + info.len];

        let mut p = 0;
        let head_len = vbyte::read_u32(blk, &mut p)? as usize;
        let head = blk.get(p..p + head_len).ok_or(Error::Corrupt("posting block header out of bounds"))?;

        self.docs.clear();
        self.freqs.clear();
        let mut q = 0;
        if count == BLOCK_LEN {
            bitpack::unpack(head, &mut q, count, &mut self.docs)?;
            bitpack::unpack(head, &mut q, count, &mut self.freqs)?;
        } else {
            for _ in 0..count {
                self.docs.push(vbyte::read_u32(head, &mut q)?);
            }
            for _ in 0..count {
                self.freqs.push(vbyte::read_u32(head, &mut q)?);
            }
        }

        let mut acc = base;
        for d in self.docs.iter_mut() {
            acc = acc.checked_add(*d).ok_or(Error::Corrupt("doc id overflow in posting block"))?;
            *d = acc;
        }
        // the skip table is trusted for pruning and range checks, so it has to agree with the data
        if self.docs.last() != Some(&info.last_doc) {
            return Err(Error::Corrupt("posting block disagrees with its skip entry"));
        }

        self.pos_bytes = &blk[p + head_len..];
        self.pos_off = 0;
        self.pos_idx = 0;
        self.cur_blk = b;
        self.idx = 0;
        Ok(())
    }

    #[allow(clippy::should_implement_trait)] // fallible, so it cannot be an Iterator
    pub fn next(&mut self) -> Result<Option<u32>> {
        if self.done {
            return Ok(None);
        }
        if self.cur_blk == NO_BLOCK {
            if self.blocks.is_empty() {
                self.done = true;
                return Ok(None);
            }
            self.load_block(0)?;
            return Ok(self.doc());
        }
        self.idx += 1;
        if self.idx < self.docs.len() {
            return Ok(self.doc());
        }
        let nb = self.cur_blk + 1;
        if nb >= self.blocks.len() {
            self.done = true;
            return Ok(None);
        }
        self.load_block(nb)?;
        Ok(self.doc())
    }

    /// First doc >= target (staying put if already there). Hops whole blocks via the skip table.
    pub fn advance(&mut self, target: u32) -> Result<Option<u32>> {
        if self.done {
            return Ok(None);
        }
        if let Some(d) = self.doc() {
            if d >= target {
                return Ok(Some(d));
            }
        }
        let from = if self.cur_blk == NO_BLOCK { 0 } else { self.cur_blk };
        let b = from + self.blocks[from..].partition_point(|b| b.last_doc < target);
        if b >= self.blocks.len() {
            self.done = true;
            return Ok(None);
        }
        if b != self.cur_blk {
            self.load_block(b)?;
        }
        self.idx += self.docs[self.idx..].partition_point(|&d| d < target);
        Ok(self.doc())
    }

    /// Positions of the current doc. Calling it twice for one doc rescans the block.
    pub fn positions(&mut self, out: &mut Vec<u32>) -> Result<()> {
        out.clear();
        if self.idx < self.pos_idx {
            self.pos_off = 0;
            self.pos_idx = 0;
        }
        while self.pos_idx < self.idx {
            for _ in 0..self.freqs[self.pos_idx] {
                vbyte::read_u32(self.pos_bytes, &mut self.pos_off)?;
            }
            self.pos_idx += 1;
        }
        let mut p = 0u32;
        for _ in 0..self.freqs[self.idx] {
            p = p.wrapping_add(vbyte::read_u32(self.pos_bytes, &mut self.pos_off)?);
            out.push(p);
        }
        self.pos_idx += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(n: u32, step: u32) -> (Vec<u8>, Vec<(u32, Vec<u32>)>) {
        let mut w = PostingsWriter::new();
        let mut expect = Vec::new();
        for i in 0..n {
            let doc = i * step + 3;
            let positions: Vec<u32> = (0..(i % 4 + 1)).map(|k| k * 5 + i % 7).collect();
            w.add(doc, &positions);
            expect.push((doc, positions));
        }
        (w.finish().bytes, expect)
    }

    #[test]
    fn roundtrip_across_block_boundaries() {
        for n in [0, 1, 127, 128, 129, 256, 300, 1000] {
            let (bytes, expect) = build(n, 3);
            let mut c = PostingsCursor::new(&bytes).unwrap();
            let mut buf = Vec::new();
            for (doc, pos) in &expect {
                assert_eq!(c.next().unwrap(), Some(*doc));
                assert_eq!(c.freq() as usize, pos.len());
                c.positions(&mut buf).unwrap();
                assert_eq!(&buf, pos);
            }
            assert_eq!(c.next().unwrap(), None, "n={n}");
        }
    }

    #[test]
    fn advance_skips_blocks() {
        let (bytes, expect) = build(1000, 2);
        let docs: Vec<u32> = expect.iter().map(|e| e.0).collect();
        let mut c = PostingsCursor::new(&bytes).unwrap();
        for target in [0, 3, 4, 500, 501, 1999, 2000, 1_000_000] {
            let want = docs.iter().copied().find(|&d| d >= target);
            let mut c2 = PostingsCursor::new(&bytes).unwrap();
            assert_eq!(c2.advance(target).unwrap(), want, "target {target}");
        }
        // monotone use on a single cursor
        assert_eq!(c.advance(100).unwrap(), Some(101));
        assert_eq!(c.advance(100).unwrap(), Some(101));
        assert_eq!(c.advance(102).unwrap(), Some(103));
        assert_eq!(c.advance(1500).unwrap(), Some(1501));
        assert_eq!(c.advance(5000).unwrap(), None);
        assert_eq!(c.next().unwrap(), None);
    }

    #[test]
    fn positions_after_skipping_docs() {
        let (bytes, expect) = build(400, 1);
        let mut c = PostingsCursor::new(&bytes).unwrap();
        let mut buf = Vec::new();
        c.advance(200).unwrap();
        c.positions(&mut buf).unwrap();
        assert_eq!(buf, expect.iter().find(|e| e.0 == 200).unwrap().1);
        // asking again for the same doc must give the same answer
        c.positions(&mut buf).unwrap();
        assert_eq!(buf, expect.iter().find(|e| e.0 == 200).unwrap().1);
    }

    #[test]
    fn stats() {
        let (bytes, expect) = build(10, 1);
        let mut w = PostingsWriter::new();
        for (d, p) in &expect {
            w.add(*d, p);
        }
        let enc = w.finish();
        assert_eq!(enc.doc_freq, 10);
        assert_eq!(enc.total_tf, expect.iter().map(|e| e.1.len() as u64).sum::<u64>());
        assert_eq!(enc.bytes, bytes);
    }

    #[test]
    fn compresses_dense_lists() {
        let mut w = PostingsWriter::new();
        for d in 0..10_000u32 {
            w.add(d, &[0]);
        }
        let enc = w.finish();
        // 1 doc-delta + 1 tf + 1 position byte-ish per doc would be ~30KB uncompressed
        assert!(enc.bytes.len() < 12_000, "got {}", enc.bytes.len());
    }

    #[test]
    fn garbage_is_rejected_not_panicking() {
        let (bytes, _) = build(300, 1);
        for cut in [0, 1, 5, bytes.len() / 2] {
            let r = PostingsCursor::new(&bytes[..cut]).and_then(|mut c| {
                while c.next()?.is_some() {}
                Ok(())
            });
            assert!(r.is_err(), "cut {cut}");
        }
    }
}
