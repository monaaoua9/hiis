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
pub mod explain;
pub mod fuzzy;
pub mod query;

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use crate::error::Result;
use crate::index::postings::PostingsCursor;
use crate::index::segment::{bit_set, Segment};
use crate::index::DocId;
use bm25::Bm25;
use query::{Clause, Occur, Query, QueryClause};

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
    // prefix -> the indexed terms it stands for (same list for every segment, so idf is consistent)
    expand: HashMap<String, Vec<String>>,
    // (term, edits) -> variants with a weight that drops as they get further from the typed word
    fuzzy: HashMap<(String, u32), Vec<(String, f32)>>,
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

    let Some(ctx) = build_ctx(segments, q, bm25) else { return Ok(Vec::new()) };

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

// None when there are no live documents at all
fn build_ctx(segments: &[Arc<Segment>], q: &Query, bm25: Bm25) -> Option<Ctx> {
    let n_docs: u64 = segments.iter().map(|s| s.live_docs() as u64).sum();
    if n_docs == 0 {
        return None;
    }
    let total_len: u64 = segments.iter().map(|s| s.live_token_len()).sum();

    let mut expand = HashMap::new();
    q.visit(&mut |c| {
        if let Clause::Prefix(p) = c {
            expand.entry(p.clone()).or_insert_with(|| expand_prefix(segments, p));
        }
    });

    let mut fuzzy_map: HashMap<(String, u32), Vec<(String, f32)>> = HashMap::new();
    q.visit(&mut |c| {
        if let Clause::Fuzzy(t, d) = c {
            fuzzy_map.entry((t.clone(), *d)).or_insert_with(|| expand_fuzzy(segments, t, *d));
        }
    });

    let mut idf = HashMap::new();
    let wanted = q
        .terms()
        .into_iter()
        .chain(expand.values().flatten().map(|s| s.as_str()))
        .chain(fuzzy_map.values().flatten().map(|v| v.0.as_str()));
    for t in wanted {
        if idf.contains_key(t) {
            continue;
        }
        let df: u64 = segments.iter().filter_map(|s| s.term(t)).map(|e| e.doc_freq as u64).sum();
        idf.insert(t.to_string(), Bm25::idf(n_docs, df));
    }
    Some(Ctx { bm25, avg_len: total_len as f32 / n_docs as f32, idf, expand, fuzzy: fuzzy_map })
}

// a prefix like "a*" would otherwise drag in half the dictionary; first terms in
// sorted order win, which is arbitrary but at least stable
const MAX_EXPANSION: usize = 256;

fn expand_prefix(segments: &[Arc<Segment>], prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in segments {
        let terms = seg.terms();
        let from = terms.partition_point(|e| e.term.as_str() < prefix);
        out.extend(
            terms[from..]
                .iter()
                .take_while(|e| e.term.starts_with(prefix))
                .take(MAX_EXPANSION)
                .map(|e| e.term.clone()),
        );
    }
    out.sort_unstable();
    out.dedup();
    out.truncate(MAX_EXPANSION);
    out
}

const MAX_FUZZY_TERMS: usize = 64;

fn expand_fuzzy(segments: &[Arc<Segment>], term: &str, dist: u32) -> Vec<(String, f32)> {
    let len = term.chars().count().max(1) as f32;
    fuzzy::expand(segments, term, dist, MAX_FUZZY_TERMS)
        .into_iter()
        .map(|c| {
            let w = 1.0 - c.dist as f32 / (len + 1.0);
            (c.term, w)
        })
        .collect()
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
fn pure_disjunction(q: &Query) -> Option<Vec<(&str, f32)>> {
    q.clauses
        .iter()
        .map(|c| match (&c.occur, &c.clause) {
            (Occur::Should, Clause::Term(t)) => Some((t.as_str(), c.boost)),
            _ => None,
        })
        .collect()
}

// bounds are computed with dl=0 and rounded floats, so give them a hair of slack
const SLACK: f32 = 1.0001;

struct Lane<'a> {
    cur: PostingsCursor<'a>,
    idf: f32,
    boost: f32,
    ub: f32,
    order: usize,
}

fn lane_doc(l: &Lane) -> u32 {
    l.cur.doc().unwrap_or(u32::MAX)
}

/// MaxScore (single terms also skip by block bound). Same hits and bit-identical scores as the
/// exhaustive path: pruning only drops docs whose upper bound is *below* the
/// current k-th score (ties are kept), and survivors are scored with the
/// same formula, summed in query-clause order.
fn wand_segment(
    seg: &Segment,
    terms: &[(&str, f32)],
    ctx: &Ctx,
    k: usize,
    heap: &mut BinaryHeap<Reverse<Hit>>,
) -> Result<()> {
    let dels = seg.deleted_snapshot();

    let mut lanes: Vec<Lane> = Vec::with_capacity(terms.len());
    for (order, (t, boost)) in terms.iter().enumerate() {
        let Some(entry) = seg.term(t) else { continue };
        let mut cur = seg.cursor(entry)?;
        if cur.next()?.is_none() {
            continue;
        }
        let idf = ctx.idf[*t];
        let ub = ctx.bm25.upper_bound(idf, cur.max_tf()) * boost;
        lanes.push(Lane { cur, idf, boost: *boost, ub, order });
    }

    // one term: no pivoting needed, just skip blocks that can't reach the top-k
    if lanes.len() == 1 {
        let lane = &mut lanes[0];
        let mut cur_doc = lane.cur.doc();
        while let Some(d) = cur_doc {
            if heap.len() >= k {
                let block_ub = ctx.bm25.upper_bound(lane.idf, lane.cur.block_max_tf()) * lane.boost;
                if block_ub * SLACK < heap.peek().unwrap().0.score {
                    cur_doc = lane.cur.advance(lane.cur.block_last_doc().saturating_add(1))?;
                    continue;
                }
            }
            if !bit_set(&dels, d) {
                let dl = seg.doc(d).len as f32;
                let score = ctx.bm25.score(lane.idf, lane.cur.freq() as f32, dl, ctx.avg_len) * lane.boost;
                offer(heap, k, Hit { doc_id: seg.doc(d).ext_id, score });
            }
            cur_doc = lane.cur.next()?;
        }
        return Ok(());
    }

    // MaxScore: lanes sorted by upper bound; the cheap prefix whose bounds add up
    // to less than the k-th score can't produce a top-k doc on its own, so only
    // the remaining lanes drive the iteration and the prefix is probed with
    // advance() for the docs they find
    lanes.sort_by(|a, b| a.ub.total_cmp(&b.ub).then(a.order.cmp(&b.order)));
    let mut prefix = vec![0.0f32; lanes.len() + 1];
    for (i, l) in lanes.iter().enumerate() {
        prefix[i + 1] = prefix[i] + l.ub;
    }

    let mut ne = 0; // lanes[..ne] are non-essential
    let mut parts: Vec<(usize, f32)> = Vec::with_capacity(lanes.len());
    loop {
        let theta = if heap.len() < k { f32::NEG_INFINITY } else { heap.peek().unwrap().0.score };
        while ne < lanes.len() && prefix[ne + 1] * SLACK < theta {
            ne += 1;
        }
        if ne == lanes.len() {
            break;
        }

        let d = lanes[ne..].iter().map(lane_doc).min().unwrap();
        if d == u32::MAX {
            break;
        }

        parts.clear();
        let mut sum = 0.0f32;
        if !bit_set(&dels, d) {
            let dl = seg.doc(d).len as f32;
            for l in &lanes[ne..] {
                if lane_doc(l) == d {
                    let sc = ctx.bm25.score(l.idf, l.cur.freq() as f32, dl, ctx.avg_len) * l.boost;
                    parts.push((l.order, sc));
                    sum += sc;
                }
            }

            let mut rest = prefix[ne];
            if (sum + rest) * SLACK >= theta {
                for l in lanes[..ne].iter_mut().rev() {
                    if l.cur.advance(d)? == Some(d) {
                        let sc = ctx.bm25.score(l.idf, l.cur.freq() as f32, dl, ctx.avg_len) * l.boost;
                        parts.push((l.order, sc));
                        sum += sc;
                    }
                    rest -= l.ub;
                    if (sum + rest) * SLACK < theta {
                        parts.clear();
                        break;
                    }
                }
                if !parts.is_empty() {
                    parts.sort_by_key(|x| x.0);
                    let mut score = parts[0].1;
                    for p in &parts[1..] {
                        score += p.1;
                    }
                    offer(heap, k, Hit { doc_id: seg.doc(d).ext_id, score });
                }
            }
        }

        for l in lanes[ne..].iter_mut() {
            if lane_doc(l) == d {
                l.cur.next()?;
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
        let hits = eval_clause(seg, &dels, qc, ctx)?;
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

fn eval_clause(seg: &Segment, dels: &[u64], qc: &QueryClause, ctx: &Ctx) -> Result<Scored> {
    let mut hits = match &qc.clause {
        Clause::Term(t) => eval_term(seg, dels, t, ctx)?,
        Clause::Prefix(p) => {
            let mut lists = Vec::new();
            for t in &ctx.expand[p] {
                lists.push(eval_term(seg, dels, t, ctx)?);
            }
            union(lists)
        }
        Clause::Phrase(p) => eval_phrase(seg, dels, p, None, ctx)?,
        Clause::Near(p, slop) => eval_phrase(seg, dels, p, Some(*slop), ctx)?,
        Clause::Fuzzy(t, d) => {
            let mut lists = Vec::new();
            for (v, w) in &ctx.fuzzy[&(t.clone(), *d)] {
                let mut l = eval_term(seg, dels, v, ctx)?;
                l.iter_mut().for_each(|x| x.1 *= w);
                lists.push(l);
            }
            union(lists)
        }
        Clause::Group(g) => eval_segment(seg, g, ctx)?,
    };
    if qc.boost != 1.0 {
        hits.iter_mut().for_each(|x| x.1 *= qc.boost);
    }
    Ok(hits)
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

fn eval_phrase(
    seg: &Segment,
    dels: &[u64],
    parts: &[(String, u32)],
    slop: Option<u32>,
    ctx: &Ctx,
) -> Result<Scored> {
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
            let n = match slop {
                None => phrase_freq(&pos, parts),
                Some(s) => near_freq(&pos, parts, s),
            };
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

// Slide a window over every word's positions shifted back by its offset in the phrase.
// A phrase hit is a window of width 0; swapping two neighbours costs 2, like in Lucene.
// Each hit uses up the positions it covered, so overlapping windows aren't double counted.
fn near_freq(pos: &[Vec<u32>], parts: &[(String, u32)], slop: u32) -> u32 {
    let mut ev: Vec<(i64, usize)> = Vec::new();
    for (i, list) in pos.iter().enumerate() {
        let off = parts[i].1 as i64;
        ev.extend(list.iter().map(|&p| (p as i64 - off, i)));
    }
    ev.sort_unstable();

    let mut have = vec![0u32; parts.len()];
    let mut covered = 0;
    let (mut lo, mut n) = (0, 0);
    for hi in 0..ev.len() {
        if have[ev[hi].1] == 0 {
            covered += 1;
        }
        have[ev[hi].1] += 1;

        while ev[hi].0 - ev[lo].0 > slop as i64 {
            have[ev[lo].1] -= 1;
            if have[ev[lo].1] == 0 {
                covered -= 1;
            }
            lo += 1;
        }
        if covered == parts.len() {
            n += 1;
            have.iter_mut().for_each(|h| *h = 0);
            covered = 0;
            lo = hi + 1;
        }
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
    fn near_windows() {
        let parts = vec![("a".to_string(), 0), ("b".to_string(), 1)];
        // a@3 b@4 is exact; a@10 b@12 needs slop 1; b@20 a@21 is swapped (cost 2)
        let pos = vec![vec![3, 10, 21], vec![4, 12, 20]];
        assert_eq!(near_freq(&pos, &parts, 0), 1);
        assert_eq!(near_freq(&pos, &parts, 1), 2);
        assert_eq!(near_freq(&pos, &parts, 2), 3);
        assert_eq!(near_freq(&pos, &parts, 100), 3);
        assert_eq!(near_freq(&[vec![1], vec![]], &parts, 100), 0);
    }

    #[test]
    fn near_with_stopword_gap() {
        // "state of the art": art sits 3 after state
        let parts = vec![("state".to_string(), 0), ("art".to_string(), 3)];
        assert_eq!(near_freq(&[vec![5], vec![8]], &parts, 0), 1);
        assert_eq!(near_freq(&[vec![5], vec![9]], &parts, 0), 0);
        assert_eq!(near_freq(&[vec![5], vec![9]], &parts, 1), 1);
    }

    #[test]
    fn phrase_matching_on_positions() {
        let parts = vec![("a".to_string(), 0), ("b".to_string(), 1), ("c".to_string(), 3)];
        let pos = vec![vec![0, 10, 20], vec![1, 11, 30], vec![3, 14, 23]];
        // start 0: a@0 b@1 c@3 ok; start 10: b@11 ok, c@13 missing; start 20: b@21 missing
        assert_eq!(phrase_freq(&pos, &parts), 1);
    }
}
