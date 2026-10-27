//! Segment merging.
//!
//! A merge never re-analyses text: it walks the sorted term dictionaries of
//! the inputs in lock step, streams each term's postings through a cursor,
//! drops tombstoned docs, renumbers the survivors, and re-encodes. Inputs are
//! concatenated in order, so renumbered doc ids stay ascending per term and
//! postings can be appended without sorting.

use std::sync::Arc;

use crate::error::Result;
use crate::index::postings::PostingsWriter;
use crate::index::segment::{bit_set, Segment, SegmentWriter};

/// Tiered policy: bucket segments by size (log base `factor`), and when a
/// bucket gets `factor` members, merge them. Small segments get merged often
/// and cheaply, big ones rarely, so each doc is rewritten O(log n) times.
#[derive(Debug, Clone)]
pub struct MergePolicy {
    pub factor: usize,
    pub max_at_once: usize,
    // everything below this many live docs counts as the same tier
    pub floor_docs: u32,
}

impl Default for MergePolicy {
    fn default() -> Self {
        MergePolicy { factor: 4, max_at_once: 10, floor_docs: 1000 }
    }
}

impl MergePolicy {
    /// Takes live-doc counts, returns indices of the segments to merge.
    pub fn select(&self, live: &[u32]) -> Option<Vec<usize>> {
        let factor = self.factor.max(2);
        let mut tiers: Vec<(u32, Vec<usize>)> = Vec::new();

        for (i, &n) in live.iter().enumerate() {
            let mut size = self.floor_docs.max(1) as u64;
            let mut tier = 0u32;
            while (n as u64) >= size * factor as u64 {
                size *= factor as u64;
                tier += 1;
            }
            match tiers.iter_mut().find(|t| t.0 == tier) {
                Some(t) => t.1.push(i),
                None => tiers.push((tier, vec![i])),
            }
        }

        tiers.sort_by_key(|t| t.0);
        for (_, mut members) in tiers {
            if members.len() >= factor {
                members.sort_by_key(|&i| live[i]);
                members.truncate(self.max_at_once.max(factor));
                members.sort_unstable();
                return Some(members);
            }
        }
        None
    }
}

/// `deleted[i]` is the tombstone bitmap of `sources[i]` as of the moment the
/// merge was planned; later deletes are reconciled by the caller.
pub fn merge_segments(sources: &[Arc<Segment>], deleted: &[Vec<u64>]) -> Result<Vec<u8>> {
    const GONE: u32 = u32::MAX;

    let mut w = SegmentWriter::new();
    let mut remap: Vec<Vec<u32>> = Vec::with_capacity(sources.len());
    let mut next = 0u32;

    for (seg, bits) in sources.iter().zip(deleted) {
        let mut map = vec![GONE; seg.n_docs() as usize];
        for local in 0..seg.n_docs() {
            if bit_set(bits, local) {
                continue;
            }
            let d = seg.doc(local);
            w.add_doc(d.ext_id, d.len, seg.text(local).unwrap_or(""));
            map[local as usize] = next;
            next += 1;
        }
        remap.push(map);
    }

    let mut at = vec![0usize; sources.len()];
    let mut pos = Vec::new();
    loop {
        let smallest =
            sources.iter().zip(&at).filter_map(|(s, &i)| s.terms().get(i).map(|t| t.term.as_str())).min();
        let Some(term) = smallest else { break };
        let term = term.to_string();

        let mut pw = PostingsWriter::new();
        for (si, seg) in sources.iter().enumerate() {
            let Some(entry) = seg.terms().get(at[si]).filter(|e| e.term == term) else { continue };
            at[si] += 1;

            let mut cur = seg.cursor(entry)?;
            while let Some(doc) = cur.next()? {
                let new = remap[si][doc as usize];
                if new == GONE {
                    continue;
                }
                cur.positions(&mut pos)?;
                pw.add(new, &pos);
            }
        }

        let enc = pw.finish();
        if enc.doc_freq > 0 {
            w.add_term(&term, enc);
        }
    }

    Ok(w.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(factor: usize) -> MergePolicy {
        MergePolicy { factor, max_at_once: 8, floor_docs: 10 }
    }

    #[test]
    fn waits_for_enough_peers() {
        assert_eq!(policy(4).select(&[5, 5, 5]), None);
        assert_eq!(policy(4).select(&[5, 5, 5, 5]), Some(vec![0, 1, 2, 3]));
    }

    #[test]
    fn tiers_are_independent() {
        // three tiny ones and one large: nothing to do
        assert_eq!(policy(4).select(&[5, 6, 7, 5_000]), None);
        // four large ones merge even with tiny ones around
        assert_eq!(policy(4).select(&[5, 5_000, 5_100, 5_200, 5_300]), Some(vec![1, 2, 3, 4]));
    }

    #[test]
    fn prefers_the_smallest_tier() {
        let live = [5, 5, 5, 5, 900, 900, 900, 900];
        assert_eq!(policy(4).select(&live), Some(vec![0, 1, 2, 3]));
    }

    #[test]
    fn caps_merge_width() {
        let p = MergePolicy { factor: 2, max_at_once: 3, floor_docs: 10 };
        let picked = p.select(&[1; 7]).unwrap();
        assert_eq!(picked.len(), 3);
    }
}
