//! Offline-style integrity check, runnable on a live engine.
//!
//! Opening a segment only proves its checksum; this walks everything the
//! query path trusts: every posting list (order, counts, positions), the
//! per-document token counts against what the postings add up to, and with
//! `deep` also decompresses every stored text and re-analyses it.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use crate::analysis::Analyzer;
use crate::index::segment::Segment;

// a corrupt index can produce a problem per posting; nobody wants a million lines
const MAX_PROBLEMS: usize = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub segments: usize,
    pub docs: u64,
    pub terms: u64,
    pub postings: u64,
    pub problems: Vec<String>,
    /// more problems were found than listed
    pub truncated: bool,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }

    fn problem(&mut self, msg: String) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(msg);
        } else {
            self.truncated = true;
        }
    }
}

impl fmt::Display for VerifyReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} segments, {} docs, {} terms, {} postings checked",
            self.segments, self.docs, self.terms, self.postings
        )?;
        if self.is_ok() {
            return write!(f, "ok");
        }
        for p in &self.problems {
            writeln!(f, "PROBLEM {p}")?;
        }
        if self.truncated {
            writeln!(f, "... and more")?;
        }
        write!(f, "{} problem(s)", self.problems.len())
    }
}

pub fn verify(segs: &[Arc<Segment>], an: &Analyzer, deep: bool) -> VerifyReport {
    let mut rep = VerifyReport { segments: segs.len(), ..Default::default() };
    let mut seen_ids: HashSet<u64> = HashSet::new();

    for seg in segs {
        let sid = seg.id();
        rep.docs += seg.n_docs() as u64;
        rep.terms += seg.terms().len() as u64;

        for local in 0..seg.n_docs() {
            let d = seg.doc(local);
            // a doc that was deleted and re-merged can't come back, so only live ones must be unique
            if !seg.is_deleted(local) && !seen_ids.insert(d.ext_id) {
                rep.problem(format!("segment {sid}: doc id {} is live in more than one segment", d.ext_id));
            }
        }

        check_postings(seg, &mut rep);
        if deep {
            check_texts(seg, an, &mut rep);
        }
    }
    rep
}

fn check_postings(seg: &Segment, rep: &mut VerifyReport) {
    let sid = seg.id();
    let n_docs = seg.n_docs();
    let mut per_doc = vec![0u64; n_docs as usize];
    let mut pos = Vec::new();
    let mut clean = true; // per-doc sums are meaningless once a list failed to decode

    for (i, e) in seg.terms().iter().enumerate() {
        if i > 0 && seg.terms()[i - 1].term >= e.term {
            rep.problem(format!("segment {sid}: terms out of order at {:?}", e.term));
        }
        let mut cur = match seg.cursor(e) {
            Ok(c) => c,
            Err(err) => {
                rep.problem(format!("segment {sid}: term {:?}: {err}", e.term));
                clean = false;
                continue;
            }
        };

        let (mut docs, mut tf_sum, mut prev) = (0u32, 0u64, None::<u32>);
        loop {
            let doc = match cur.next() {
                Ok(Some(d)) => d,
                Ok(None) => break,
                Err(err) => {
                    rep.problem(format!("segment {sid}: term {:?}: {err}", e.term));
                    clean = false;
                    break;
                }
            };
            docs += 1;
            rep.postings += 1;
            if prev.is_some_and(|p| p >= doc) {
                rep.problem(format!("segment {sid}: term {:?}: doc {doc} not ascending", e.term));
            }
            prev = Some(doc);
            if doc >= n_docs {
                rep.problem(format!("segment {sid}: term {:?}: doc {doc} out of range", e.term));
                clean = false;
                continue;
            }

            let freq = cur.freq();
            tf_sum += freq as u64;
            per_doc[doc as usize] += freq as u64;
            if let Err(err) = cur.positions(&mut pos) {
                rep.problem(format!("segment {sid}: term {:?} doc {doc}: {err}", e.term));
                clean = false;
                continue;
            }
            if pos.len() != freq as usize {
                rep.problem(format!(
                    "segment {sid}: term {:?} doc {doc}: freq {freq} but {} positions",
                    e.term,
                    pos.len()
                ));
            }
            if pos.windows(2).any(|w| w[0] >= w[1]) {
                rep.problem(format!("segment {sid}: term {:?} doc {doc}: positions not ascending", e.term));
            }
        }

        if docs != e.doc_freq {
            rep.problem(format!(
                "segment {sid}: term {:?}: doc_freq {} but {docs} postings",
                e.term, e.doc_freq
            ));
        }
        if tf_sum != e.total_tf {
            rep.problem(format!(
                "segment {sid}: term {:?}: total_tf {} but {tf_sum} counted",
                e.term, e.total_tf
            ));
        }
    }

    if clean {
        for (local, &sum) in per_doc.iter().enumerate() {
            let want = seg.doc(local as u32).len as u64;
            if sum != want {
                let ext = seg.doc(local as u32).ext_id;
                rep.problem(format!(
                    "segment {sid}: doc {ext}: {want} tokens recorded, postings add up to {sum}"
                ));
            }
        }
    }
}

fn check_texts(seg: &Segment, an: &Analyzer, rep: &mut VerifyReport) {
    let sid = seg.id();
    for local in 0..seg.n_docs() {
        let d = seg.doc(local);
        match seg.text(local) {
            None => rep.problem(format!("segment {sid}: doc {}: stored text unreadable", d.ext_id)),
            Some(text) => {
                let n = an.analyze(&text).len() as u32;
                if n != d.len {
                    rep.problem(format!(
                        "segment {sid}: doc {}: text analyses to {n} tokens, index says {}",
                        d.ext_id, d.len
                    ));
                }
            }
        }
    }
}
