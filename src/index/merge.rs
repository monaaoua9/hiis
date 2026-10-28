//! Segment merging.
//!
//! A merge never re-analyses text: it walks the sorted term dictionaries of
//! the inputs in lock step, streams each term's postings through a cursor,
//! drops tombstoned docs, renumbers the survivors, and re-encodes. Inputs are
//! concatenated in order, so renumbered doc ids stay ascending per term and
//! postings can be appended without sorting.

use std::path::Path;
use std::sync::Arc;

use crate::error::Result;
use crate::index::postings::PostingsWriter;
use crate::index::segment::{bit_set, BlockCache, Segment, StreamWriter};

/// Tiered policy: bucket segments by size (log base `factor`), and when a
/// bucket gets `factor` members, merge them. Small segments get merged often
/// and cheaply, big ones rarely, so each doc is rewritten O(log n) times.
#[derive(Debug, Clone)]
pub struct MergePolicy {
    pub factor: usize,
    pub max_at_once: usize,
    // everything below this many live docs counts as the same tier
    pub floor_docs: u32,
    // a merge reads all inputs into memory and builds the output next to them,
    // so this is what bounds the peak: bigger segments are left alone and a
    // merge never produces more live docs than this
    pub max_merged_docs: u32,
}

impl Default for MergePolicy {
    fn default() -> Self {
        MergePolicy { factor: 4, max_at_once: 10, floor_docs: 1000, max_merged_docs: 5_000_000 }
    }
}

impl MergePolicy {
    /// Takes live-doc counts, returns indices of the segments to merge.
    pub fn select(&self, live: &[u32]) -> Option<Vec<usize>> {
        let factor = self.factor.max(2);
        let mut tiers: Vec<(u32, Vec<usize>)> = Vec::new();

        for (i, &n) in live.iter().enumerate() {
            if n >= self.max_merged_docs {
                continue;
            }
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
                // smallest first, so trimming drops the biggest
                let mut total = 0u64;
                members.retain(|&i| {
                    total += live[i] as u64;
                    total <= self.max_merged_docs as u64
                });
                if members.len() < 2 {
                    continue;
                }
                members.sort_unstable();
                return Some(members);
            }
        }
        None
    }
}

/// `deleted[i]` is the tombstone bitmap of `sources[i]` as of the moment the
/// merge was planned; later deletes are reconciled by the caller. The merged
/// segment is streamed to `out`, so peak memory is the term/doc tables plus
/// one posting list, not the whole output.
pub fn merge_segments(sources: &[Arc<Segment>], deleted: &[Vec<u64>], out: &Path) -> Result<()> {
    const GONE: u32 = u32::MAX;

    let mut docs: Vec<(u64, u32, u32)> = Vec::new();
    let mut survivors: Vec<(usize, u32)> = Vec::new();
    let mut remap: Vec<Vec<u32>> = Vec::with_capacity(sources.len());

    for (si, (seg, bits)) in sources.iter().zip(deleted).enumerate() {
        let mut map = vec![GONE; seg.n_docs() as usize];
        for local in 0..seg.n_docs() {
            if bit_set(bits, local) {
                continue;
            }
            let d = seg.doc(local);
            map[local as usize] = docs.len() as u32;
            docs.push((d.ext_id, d.len, d.text_len));
            survivors.push((si, local));
        }
        remap.push(map);
    }

    let mut w = StreamWriter::create(out)?;
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
            w.add_term(&term, enc)?;
        }
    }

    let mut caches: Vec<BlockCache> = sources.iter().map(|_| BlockCache::default()).collect();
    w.finish(&docs, |i| {
        let (si, local) = survivors[i];
        sources[si].text_bytes_with(local, &mut caches[si])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(factor: usize) -> MergePolicy {
        MergePolicy { factor, max_at_once: 8, floor_docs: 10, ..MergePolicy::default() }
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
    fn merged_size_is_capped() {
        let p = MergePolicy { max_merged_docs: 100, ..policy(2) };
        // 60 is too big to be a candidate, the rest fit
        assert_eq!(p.select(&[60, 20, 20, 30]), Some(vec![1, 2, 3]));
        // 50+50 would hit the cap exactly, a third doesn't fit
        assert_eq!(p.select(&[50, 50, 50]), Some(vec![0, 1]));
        // already at the cap: never touched again
        assert_eq!(p.select(&[100, 100, 100]), None);
    }

    #[test]
    fn caps_merge_width() {
        let p = MergePolicy { factor: 2, max_at_once: 3, floor_docs: 10, ..MergePolicy::default() };
        let picked = p.select(&[1; 7]).unwrap();
        assert_eq!(picked.len(), 3);
    }
}
