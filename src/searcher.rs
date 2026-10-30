//! A frozen view of the index for consistent reads.
//!
//! Takes the segment list once, so documents flushed or merged afterwards don't change what
//! this view returns (pagination across requests stays put). Deletes are the exception: they
//! are flags on the shared segments, so a document deleted later disappears from the view too.

use std::sync::Arc;

use crate::analysis::Analyzer;
use crate::error::Result;
use crate::index::segment::Segment;
use crate::index::DocId;
use crate::search::bm25::Bm25;
use crate::search::page::{Cursor, Page};
use crate::search::query::Query;
use crate::search::{self, Hit};

pub struct Searcher {
    pub(crate) segments: Vec<Arc<Segment>>,
    pub(crate) analyzer: Analyzer,
    pub(crate) bm25: Bm25,
}

impl Searcher {
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        let q = Query::parse(query, &self.analyzer);
        search::execute(&self.segments, &q, k, self.bm25)
    }

    pub fn search_page(&self, query: &str, k: usize, after: Option<Cursor>) -> Result<Page> {
        let q = Query::parse(query, &self.analyzer);
        let hits = search::execute_after(&self.segments, &q, k, self.bm25, true, after.map(|c| c.as_hit()))?;
        let next = if k > 0 && hits.len() == k { hits.last().map(Cursor::after) } else { None };
        Ok(Page { hits, next })
    }

    pub fn count(&self, query: &str) -> Result<u64> {
        let q = Query::parse(query, &self.analyzer);
        search::count(&self.segments, &q, self.bm25)
    }

    pub fn get(&self, id: DocId) -> Option<String> {
        self.segments.iter().find_map(|s| s.text_of(id))
    }
}
