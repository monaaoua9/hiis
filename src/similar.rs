//! More-like-this: turn a document into a weighted OR query of its most
//! characteristic terms (term frequency x idf), so the usual BM25 search finds
//! its neighbours.

use std::collections::HashMap;
use std::sync::Arc;

use crate::analysis::Analyzer;
use crate::index::segment::Segment;
use crate::search::bm25::Bm25;
use crate::search::query::{Clause, Occur, Query, QueryClause};

pub const MAX_TERMS: usize = 25;

pub fn query_for(segs: &[Arc<Segment>], an: &Analyzer, text: &str, max_terms: usize) -> Query {
    let n_docs: u64 = segs.iter().map(|s| s.live_docs() as u64).sum();
    let mut tf: HashMap<String, u32> = HashMap::new();
    for t in an.analyze(text) {
        *tf.entry(t.term).or_default() += 1;
    }

    let mut weighted: Vec<(String, f32, u64)> = tf
        .into_iter()
        .map(|(term, f)| {
            let df: u64 = segs.iter().filter_map(|s| s.term(&term)).map(|e| e.doc_freq as u64).sum();
            let w = f as f32 * Bm25::idf(n_docs, df);
            (term, w, df)
        })
        .collect();

    // a term only this document has can't point at any other document
    let shared = weighted.iter().filter(|x| x.2 >= 2).count();
    if shared > 0 {
        weighted.retain(|x| x.2 >= 2);
    }
    weighted.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    weighted.truncate(max_terms);

    let top = weighted.first().map_or(1.0, |x| x.1.max(f32::MIN_POSITIVE));
    let clauses = weighted
        .into_iter()
        .map(|(term, w, _)| QueryClause { occur: Occur::Should, clause: Clause::Term(term), boost: w / top })
        .collect();
    Query { clauses }
}
