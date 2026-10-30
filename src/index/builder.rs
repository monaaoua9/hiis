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

pub struct Grouped {
    n_tokens: u32,
    // sorted by term; positions are flat, `freq` of them per term, ascending
    terms: Vec<(String, u32)>,
    positions: Vec<u32>,
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
    // the sort-and-group half of indexing a doc; needs no builder state, so
    // callers do it before taking the buffer lock
    pub fn group(tokens: Vec<Token>) -> Grouped {
        let n = tokens.len() as u32;
        Self::group_counted(tokens, n)
    }

    // `n_tokens` is the document length BM25 sees; field postings ride along without counting
    pub fn group_counted(tokens: Vec<Token>, n_tokens: u32) -> Grouped {
        let mut order: Vec<usize> = (0..tokens.len()).collect();
        order.sort_unstable_by(|&a, &b| {
            tokens[a].term.cmp(&tokens[b].term).then(tokens[a].position.cmp(&tokens[b].position))
        });

        let mut terms: Vec<(String, u32)> = Vec::new();
        let mut positions = Vec::with_capacity(tokens.len());
        let mut tokens: Vec<Option<Token>> = tokens.into_iter().map(Some).collect();
        for k in order {
            let t = tokens[k].take().unwrap();
            match terms.last_mut() {
                Some((last, freq)) if *last == t.term => *freq += 1,
                _ => terms.push((t.term, 1)),
            }
            positions.push(t.position);
        }
        Grouped { n_tokens, terms, positions }
    }

    pub fn add(&mut self, id: DocId, text: &str, g: Grouped) {
        let local = self.docs.len() as u32;

        let mut off = 0usize;
        for (term, freq) in g.terms {
            let n = freq as usize;
            let buf = match self.terms.get_mut(term.as_str()) {
                Some(b) => b,
                None => {
                    self.ram += term.len() + 64;
                    self.terms.entry(term).or_default()
                }
            };
            buf.docs.push(local);
            buf.freqs.push(freq);
            buf.positions.extend_from_slice(&g.positions[off..off + n]);
            self.ram += 8 + n * 4;
            off += n;
        }

        self.ram += text.len() + 48;
        self.docs.push((id, g.n_tokens, text.to_string()));
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
