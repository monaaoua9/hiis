//! Query execution and ranking.
//!
//! Each clause is evaluated per segment into a doc-sorted `(doc, score)` list
//! (term clauses stream one cursor, phrase clauses leapfrog the term cursors
//! and then verify positions). Clause lists are combined with MUST / SHOULD /
//! MUST_NOT semantics, and a bounded min-heap keeps the global top-k.
//!
//! IDF and average length are computed across *all* segments of the snapshot,
//! so scores don't depend on how documents happen to be split into segments.

pub mod bm25;
pub mod query;

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use crate::error::Result;
use crate::index::postings::PostingsCursor;
use crate::index::segment::{bit_set, Segment};
use crate::index::DocId;
use bm25::Bm25;
use query::{Clause, Occur, Query};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub doc_id: DocId,
    pub score: f32,
}

// "greater" means "ranks higher": bigger score, then smaller doc id
impl Eq for Hit {}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.total_cmp(&other.score).then_with(|| other.doc_id.cmp(&self.doc_id))
    }
}

struct Ctx {
    bm25: Bm25,
    avg_len: f32,
    idf: HashMap<String, f32>,
}

type Scored = Vec<(u32, f32)>;

pub fn execute(segments: &[Arc<Segment>], q: &Query, k: usize, bm25: Bm25) -> Result<Vec<Hit>> {
    execute_with(segments, q, k, bm25, true)
}

/// `prune = false` forces exhaustive scoring; results are identical either
/// way, it exists so tests and benchmarks can compare the two paths.
pub fn execute_with(
    segments: &[Arc<Segment>],
    q: &Query,
    k: usize,
    bm25: Bm25,
    prune: bool,
) -> Result<Vec<Hit>> {
    if k == 0 || q.clauses.is_empty() {
        return Ok(Vec::new());
    }

    let n_docs: u64 = segments.iter().map(|s| s.live_docs() as u64).sum();
    if n_docs == 0 {
        return Ok(Vec::new());
    }
    let total_len: u64 = segments.iter().map(|s| s.live_token_len()).sum();

    let mut idf = HashMap::new();
    for t in q.terms() {
        let df: u64 = segments.iter().filter_map(|s| s.term(t)).map(|e| e.doc_freq as u64).sum();
        idf.insert(t.to_string(), Bm25::idf(n_docs, df));
    }
    let ctx = Ctx { bm25, avg_len: total_len as f32 / n_docs as f32, idf };

    let mut heap: BinaryHeap<Reverse<Hit>> = BinaryHeap::with_capacity(k + 1);
    let disjunction = if prune { pure_disjunction(q) } else { None };

    for seg in segments {
        if let Some(terms) = &disjunction {
            wand_segment(seg, terms, &ctx, k, &mut heap)?;
            continue;
        }
        for (doc, score) in eval_segment(seg, q, &ctx)? {
            offer(&mut heap, k, Hit { doc_id: seg.doc(doc).ext_id, score });
        }
    }
    Ok(heap.into_sorted_vec().into_iter().map(|r| r.0).collect())
}

fn offer(heap: &mut BinaryHeap<Reverse<Hit>>, k: usize, hit: Hit) {
    if heap.len() < k {
        heap.push(Reverse(hit));
    } else if hit > heap.peek().unwrap().0 {
        heap.pop();
        heap.push(Reverse(hit));
    }
}

// only plain "a b c" queries are pruned; MUST/NOT/phrase go through the exhaustive path
fn pure_disjunction(q: &Query) -> Option<Vec<&str>> {
    q.clauses
        .iter()
        .map(|c| match (&c.occur, &c.clause) {
            (Occur::Should, Clause::Term(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

// bounds are computed with dl=0 and rounded floats, so give them a hair of slack
const SLACK: f32 = 1.0001;

struct Lane<'a> {
    cur: PostingsCursor<'a>,
    idf: f32,
    ub: f32,
    order: usize,
}

fn lane_doc(l: &Lane) -> u32 {
    l.cur.doc().unwrap_or(u32::MAX)
}

/// WAND with block-max refinement. Same hits and bit-identical scores as the
/// exhaustive path: pruning only drops docs whose upper bound is *below* the
/// current k-th score (ties are kept), and survivors are scored with the
/// same formula, summed in query-clause order.
fn wand_segment(
    seg: &Segment,
    terms: &[&str],
    ctx: &Ctx,
    k: usize,
    heap: &mut BinaryHeap<Reverse<Hit>>,
) -> Result<()> {
    let dels = seg.deleted_snapshot();

    let mut lanes: Vec<Lane> = Vec::with_capacity(terms.len());
    for (order, t) in terms.iter().enumerate() {
        let Some(entry) = seg.term(t) else { continue };
        let mut cur = seg.cursor(entry)?;
        if cur.next()?.is_none() {
            continue;
        }
        let idf = ctx.idf[*t];
        let ub = ctx.bm25.upper_bound(idf, cur.max_tf());
        lanes.push(Lane { cur, idf, ub, order });
    }

    // one term: no pivoting needed, just skip blocks that can't reach the top-k
    if lanes.len() == 1 {
        let lane = &mut lanes[0];
        let mut cur_doc = lane.cur.doc();
        while let Some(d) = cur_doc {
            if heap.len() >= k {
                let block_ub = ctx.bm25.upper_bound(lane.idf, lane.cur.block_max_tf());
                if block_ub * SLACK < heap.peek().unwrap().0.score {
                    cur_doc = lane.cur.advance(lane.cur.block_last_doc().saturating_add(1))?;
                    continue;
                }
            }
            if !bit_set(&dels, d) {
                let dl = seg.doc(d).len as f32;
                let score = ctx.bm25.score(lane.idf, lane.cur.freq() as f32, dl, ctx.avg_len);
                offer(heap, k, Hit { doc_id: seg.doc(d).ext_id, score });
            }
            cur_doc = lane.cur.next()?;
        }
        return Ok(());
    }

    let mut parts: Vec<(usize, f32)> = Vec::with_capacity(lanes.len());
    while !lanes.is_empty() {
        lanes.sort_by_key(lane_doc);
        let theta = if heap.len() < k { f32::NEG_INFINITY } else { heap.peek().unwrap().0.score };

        // pivot: first lane where the cumulative term bound can reach theta
        let mut acc = 0.0f32;
        let Some(p) = lanes.iter().position(|l| {
            acc += l.ub;
            acc * SLACK >= theta
        }) else {
            break; // nothing left can beat the current top-k
        };

        let pdoc = lane_doc(&lanes[p]);
        if lane_doc(&lanes[0]) != pdoc {
            // lagging lanes can't produce a candidate before pdoc
            if lanes[0].cur.advance(pdoc)?.is_none() {
                lanes.remove(0);
            }
            continue;
        }

        let aligned = lanes.iter().take_while(|l| lane_doc(l) == pdoc).count();

        // block-level bound for the lanes sitting on pdoc
        let block_ub: f32 =
            lanes[..aligned].iter().map(|l| ctx.bm25.upper_bound(l.idf, l.cur.block_max_tf())).sum();
        if block_ub * SLACK < theta {
            // nothing in [pdoc, limit] can qualify: only these lanes' current
            // blocks (or lanes already past limit) cover that range
            let mut limit = lanes[..aligned].iter().map(|l| l.cur.block_last_doc()).min().unwrap();
            if let Some(next) = lanes.get(aligned) {
                limit = limit.min(lane_doc(next) - 1);
            }
            let mut i = 0;
            let mut n = aligned;
            while i < n {
                if lanes[i].cur.advance(limit.saturating_add(1))?.is_none() {
                    lanes.remove(i);
                    n -= 1;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        if !bit_set(&dels, pdoc) {
            let dl = seg.doc(pdoc).len as f32;
            parts.clear();
            parts.extend(
                lanes[..aligned]
                    .iter()
                    .map(|l| (l.order, ctx.bm25.score(l.idf, l.cur.freq() as f32, dl, ctx.avg_len))),
            );
            parts.sort_by_key(|x| x.0);
            let mut score = parts[0].1;
            for p in &parts[1..] {
                score += p.1;
            }
            offer(heap, k, Hit { doc_id: seg.doc(pdoc).ext_id, score });
        }

        let mut i = 0;
        let mut n = aligned;
        while i < n {
            if lanes[i].cur.next()?.is_none() {
                lanes.remove(i);
                n -= 1;
            } else {
                i += 1;
            }
        }
    }
    Ok(())
}

fn eval_segment(seg: &Segment, q: &Query, ctx: &Ctx) -> Result<Scored> {
    // one lock round-trip per segment instead of one per posting
    let dels = seg.deleted_snapshot();
    let mut must = Vec::new();
    let mut should = Vec::new();
    let mut not = Vec::new();

    for qc in &q.clauses {
        let hits = match &qc.clause {
            Clause::Term(t) => eval_term(seg, &dels, t, ctx)?,
            Clause::Phrase(p) => eval_phrase(seg, &dels, p, ctx)?,
        };
        match qc.occur {
            Occur::Must => must.push(hits),
            Occur::Should => should.push(hits),
            Occur::MustNot => not.push(hits),
        }
    }

    // a bare negation matches nothing rather than "everything else"
    if must.is_empty() && should.is_empty() {
        return Ok(Vec::new());
    }

    let mut acc = if must.is_empty() {
        union(should)
    } else {
        // smallest list first keeps the intermediate results small
        must.sort_by_key(|l| l.len());
        let mut it = must.into_iter();
        let mut acc = it.next().unwrap();
        for l in it {
            acc = intersect(&acc, &l);
        }
        for l in &should {
            add_matching(&mut acc, l);
        }
        acc
    };

    for l in &not {
        acc = difference(&acc, l);
    }
    Ok(acc)
}

fn eval_term(seg: &Segment, dels: &[u64], term: &str, ctx: &Ctx) -> Result<Scored> {
    let Some(entry) = seg.term(term) else { return Ok(Vec::new()) };
    let idf = ctx.idf[term];
    let mut cur = seg.cursor(entry)?;
    let mut out = Vec::with_capacity(entry.doc_freq as usize);

    while let Some(doc) = cur.next()? {
        if bit_set(dels, doc) {
            continue;
        }
        let s = ctx.bm25.score(idf, cur.freq() as f32, seg.doc(doc).len as f32, ctx.avg_len);
        out.push((doc, s));
    }
    Ok(out)
}

fn eval_phrase(seg: &Segment, dels: &[u64], parts: &[(String, u32)], ctx: &Ctx) -> Result<Scored> {
    let mut curs = Vec::with_capacity(parts.len());
    for (t, _) in parts {
        match seg.term(t) {
            Some(e) => curs.push(seg.cursor(e)?),
            None => return Ok(Vec::new()),
        }
    }
    // phrase idf is the sum of its terms', same idea as Lucene's PhraseWeight
    let idf: f32 = parts.iter().map(|(t, _)| ctx.idf[t.as_str()]).sum();

    let mut out = Vec::new();
    let mut pos: Vec<Vec<u32>> = vec![Vec::new(); parts.len()];

    let Some(mut d) = curs[0].advance(0)? else { return Ok(out) };
    'outer: loop {
        for i in 1..curs.len() {
            match curs[i].advance(d)? {
                None => break 'outer,
                Some(x) if x > d => match curs[0].advance(x)? {
                    Some(nd) => {
                        d = nd;
                        continue 'outer;
                    }
                    None => break 'outer,
                },
                Some(_) => {}
            }
        }

        // every cursor sits on d
        if !bit_set(dels, d) {
            for (i, c) in curs.iter_mut().enumerate() {
                c.positions(&mut pos[i])?;
            }
            let n = phrase_freq(&pos, parts);
            if n > 0 {
                let s = ctx.bm25.score(idf, n as f32, seg.doc(d).len as f32, ctx.avg_len);
                out.push((d, s));
            }
        }
        match curs[0].advance(d + 1)? {
            Some(nd) => d = nd,
            None => break,
        }
    }
    Ok(out)
}

fn phrase_freq(pos: &[Vec<u32>], parts: &[(String, u32)]) -> u32 {
    let mut n = 0;
    'start: for &p in &pos[0] {
        for i in 1..pos.len() {
            // parts[i].1 >= parts[0].1 by construction
            let want = p + (parts[i].1 - parts[0].1);
            if pos[i].binary_search(&want).is_err() {
                continue 'start;
            }
        }
        n += 1;
    }
    n
}

fn intersect(a: &[(u32, f32)], b: &[(u32, f32)]) -> Scored {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                out.push((a[i].0, a[i].1 + b[j].1));
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn union(lists: Vec<Scored>) -> Scored {
    let mut all: Scored = lists.into_iter().flatten().collect();
    all.sort_by_key(|x| x.0);

    let mut out: Scored = Vec::with_capacity(all.len());
    for (d, s) in all {
        match out.last_mut() {
            Some(last) if last.0 == d => last.1 += s,
            _ => out.push((d, s)),
        }
    }
    out
}

// adds optional-clause scores to docs already in acc, never adds docs
fn add_matching(acc: &mut Scored, extra: &[(u32, f32)]) {
    let mut j = 0;
    for entry in acc.iter_mut() {
        while j < extra.len() && extra[j].0 < entry.0 {
            j += 1;
        }
        if j < extra.len() && extra[j].0 == entry.0 {
            entry.1 += extra[j].1;
        }
    }
}

fn difference(a: &[(u32, f32)], b: &[(u32, f32)]) -> Scored {
    let mut j = 0;
    a.iter()
        .filter(|(d, _)| {
            while j < b.len() && b[j].0 < *d {
                j += 1;
            }
            !(j < b.len() && b[j].0 == *d)
        })
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_ops() {
        let a = vec![(1, 1.0), (3, 1.0), (5, 1.0)];
        let b = vec![(3, 2.0), (4, 2.0), (5, 2.0)];
        assert_eq!(intersect(&a, &b), [(3, 3.0), (5, 3.0)]);
        assert_eq!(union(vec![a.clone(), b.clone()]), [(1, 1.0), (3, 3.0), (4, 2.0), (5, 3.0)]);
        assert_eq!(difference(&a, &b), [(1, 1.0)]);
        let mut c = a.clone();
        add_matching(&mut c, &b);
        assert_eq!(c, [(1, 1.0), (3, 3.0), (5, 3.0)]);
    }

    #[test]
    fn hit_ordering_prefers_score_then_low_id() {
        let a = Hit { doc_id: 5, score: 2.0 };
        let b = Hit { doc_id: 1, score: 2.0 };
        let c = Hit { doc_id: 9, score: 3.0 };
        assert!(c > b && b > a);
    }

    #[test]
    fn phrase_matching_on_positions() {
        let parts = vec![("a".to_string(), 0), ("b".to_string(), 1), ("c".to_string(), 3)];
        let pos = vec![vec![0, 10, 20], vec![1, 11, 30], vec![3, 14, 23]];
        // start 0: a@0 b@1 c@3 ok; start 10: b@11 ok, c@13 missing; start 20: b@21 missing
        assert_eq!(phrase_freq(&pos, &parts), 1);
    }
}
