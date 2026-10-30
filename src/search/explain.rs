//! "Why did this document score like that" (or "why is it missing").
//!
//! Every clause of the query is evaluated against the one document, so the
//! breakdown uses the same code and the same collection statistics as a real
//! search. The total comes from the normal combine step, which is what makes it
//! equal to the score `search` reports.

use std::fmt;
use std::sync::Arc;

use super::bm25::Bm25;
use super::query::{Clause, Occur, Query, QueryClause};
use super::{build_ctx, eval_clause, eval_segment, Ctx};
use crate::error::Result;
use crate::index::segment::Segment;
use crate::index::DocId;

#[derive(Debug, Clone, PartialEq)]
pub struct ClauseExplain {
    pub label: String,
    pub occur: Occur,
    pub boost: f32,
    /// score contributed (boost included); None if the clause does not match the document
    pub score: Option<f32>,
    pub note: String,
    /// the clauses of a group
    pub children: Vec<ClauseExplain>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    pub doc_id: DocId,
    /// whether the document is part of the result set at all
    pub matched: bool,
    pub score: f32,
    pub clauses: Vec<ClauseExplain>,
}

pub fn explain(segments: &[Arc<Segment>], q: &Query, bm25: Bm25, id: DocId) -> Result<Option<Explanation>> {
    let Some((seg, local)) = segments.iter().find_map(|s| s.local_of(id).map(|l| (s, l))) else {
        return Ok(None);
    };
    // nothing indexed at all can't happen here (the doc exists), but an empty query can
    let Some(ctx) = build_ctx(segments, q, bm25) else { return Ok(None) };
    if q.clauses.is_empty() {
        return Ok(Some(Explanation { doc_id: id, matched: false, score: 0.0, clauses: Vec::new() }));
    }

    let total = eval_segment(seg, q, &ctx)?;
    let found = total.binary_search_by_key(&local, |x| x.0).ok().map(|i| total[i].1);
    let dels = seg.deleted_snapshot();
    Ok(Some(Explanation {
        doc_id: id,
        matched: found.is_some(),
        score: found.unwrap_or(0.0),
        clauses: explain_clauses(seg, &dels, local, q, &ctx)?,
    }))
}

fn explain_clauses(
    seg: &Segment,
    dels: &[u64],
    local: u32,
    q: &Query,
    ctx: &Ctx,
) -> Result<Vec<ClauseExplain>> {
    let mut out = Vec::with_capacity(q.clauses.len());
    for qc in &q.clauses {
        let hits = eval_clause(seg, dels, qc, ctx)?;
        let score = hits.binary_search_by_key(&local, |x| x.0).ok().map(|i| hits[i].1);
        let children = match &qc.clause {
            Clause::Group(g) => explain_clauses(seg, dels, local, g, ctx)?,
            _ => Vec::new(),
        };
        out.push(ClauseExplain {
            label: label(&qc.clause),
            occur: qc.occur,
            boost: qc.boost,
            score,
            note: note(seg, local, qc, score, ctx)?,
            children,
        });
    }
    Ok(out)
}

fn label(c: &Clause) -> String {
    let phrase = |p: &[(String, u32)]| {
        let mut s = String::from("\"");
        let mut at = 0;
        for (i, (t, off)) in p.iter().enumerate() {
            if i > 0 {
                // stopword gaps show up as "_"
                for _ in at + 1..*off {
                    s.push_str(" _");
                }
                s.push(' ');
            }
            s.push_str(t);
            at = *off;
        }
        s.push('"');
        s
    };
    match c {
        Clause::Term(t) => t.clone(),
        Clause::Prefix(p) => format!("{p}*"),
        Clause::Phrase(p) => phrase(p),
        Clause::Near(p, n) => format!("{}~{n}", phrase(p)),
        Clause::Fuzzy(t, d) => format!("{t}~{d}"),
        Clause::Group(_) => "(group)".to_string(),
    }
}

fn note(seg: &Segment, local: u32, qc: &QueryClause, score: Option<f32>, ctx: &Ctx) -> Result<String> {
    if score.is_none() {
        return Ok(String::new());
    }
    let Clause::Term(t) = &qc.clause else { return Ok(String::new()) };
    let Some(entry) = seg.term(t) else { return Ok(String::new()) };
    let mut cur = seg.cursor(entry)?;
    if cur.advance(local)? != Some(local) {
        return Ok(String::new());
    }
    let (idf, tf, dl) = (ctx.idf[t.as_str()], cur.freq(), seg.doc(local).len);
    let raw = ctx.bm25.score(idf, tf as f32, dl as f32, ctx.avg_len);
    Ok(format!(
        "idf {idf:.3}, tf {tf}, doc len {dl} (avg {:.1}), k1 {} b {}; bm25 {raw:.4}",
        ctx.avg_len, ctx.bm25.k1, ctx.bm25.b
    ))
}

impl fmt::Display for Explanation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.matched {
            writeln!(f, "doc {} scores {:.4}", self.doc_id, self.score)?;
        } else {
            writeln!(f, "doc {} is not a result", self.doc_id)?;
        }
        write_clauses(f, &self.clauses, 1)
    }
}

fn write_clauses(f: &mut fmt::Formatter<'_>, cs: &[ClauseExplain], depth: usize) -> fmt::Result {
    for c in cs {
        let sign = match c.occur {
            Occur::Must => "+",
            Occur::Should => " ",
            Occur::MustNot => "-",
        };
        write!(f, "{:indent$}{sign}{}", "", c.label, indent = depth * 2)?;
        if c.boost != 1.0 {
            write!(f, "^{}", c.boost)?;
        }
        match c.score {
            Some(s) => write!(f, "  {s:.4}")?,
            None => write!(f, "  no match")?,
        }
        if !c.note.is_empty() {
            write!(f, "  [{}]", c.note)?;
        }
        writeln!(f)?;
        write_clauses(f, &c.children, depth + 1)?;
    }
    Ok(())
}
