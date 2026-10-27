use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::analysis::Token;
use crate::index::postings::PostingsWriter;
use crate::index::segment::SegmentWriter;
use crate::index::DocId;

// flat per-term buffers: positions of doc i live at the running sum of freqs
#[derive(Default)]
struct TermBuf {
    docs: Vec<u32>,
    freqs: Vec<u32>,
    positions: Vec<u32>,
}

// term strings are trusted input from our own analyzer, so a cheap multiplicative
// hash beats SipHash here (no HashDoS concern for an in-process buffer)
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// The mutable in-memory segment. Everything lands here first and is turned
/// into an immutable on-disk segment by `build`.
#[derive(Default)]
pub struct SegmentBuilder {
    terms: HashMap<String, TermBuf, BuildHasherDefault<Fx>>,
    docs: Vec<(DocId, u32, String)>,
    ram: usize,
}

impl SegmentBuilder {
    pub fn add(&mut self, id: DocId, text: &str, tokens: &[Token]) {
        let local = self.docs.len() as u32;

        // group by term without a per-doc map: sort token indices by (term, position)
        let mut order: Vec<usize> = (0..tokens.len()).collect();
        order.sort_unstable_by(|&a, &b| {
            tokens[a].term.cmp(&tokens[b].term).then(tokens[a].position.cmp(&tokens[b].position))
        });

        let mut i = 0;
        while i < order.len() {
            let term = tokens[order[i]].term.as_str();
            let mut j = i;
            while j < order.len() && tokens[order[j]].term == term {
                j += 1;
            }
            let buf = match self.terms.get_mut(term) {
                Some(b) => b,
                None => {
                    self.ram += term.len() + 64;
                    self.terms.entry(term.to_string()).or_default()
                }
            };
            buf.docs.push(local);
            buf.freqs.push((j - i) as u32);
            buf.positions.extend(order[i..j].iter().map(|&k| tokens[k].position));
            self.ram += 8 + (j - i) * 4;
            i = j;
        }

        self.ram += text.len() + 48;
        self.docs.push((id, tokens.len() as u32, text.to_string()));
    }
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    pub fn ram_bytes(&self) -> usize {
        self.ram
    }

    pub fn contains(&self, id: DocId) -> bool {
        self.docs.iter().any(|d| d.0 == id)
    }

    pub fn build(self) -> Vec<u8> {
        let mut w = SegmentWriter::new();
        for (id, len, text) in &self.docs {
            w.add_doc(*id, *len, text);
        }

        let mut terms: Vec<(String, TermBuf)> = self.terms.into_iter().collect();
        terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        for (term, buf) in terms {
            let mut pw = PostingsWriter::new();
            let mut off = 0usize;
            for (i, &doc) in buf.docs.iter().enumerate() {
                let n = buf.freqs[i] as usize;
                pw.add(doc, &buf.positions[off..off + n]);
                off += n;
            }
            w.add_term(&term, pw.finish());
        }
        w.finish()
    }
}
