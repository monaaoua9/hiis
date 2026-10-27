//! Inverted index storage: in-memory builder, immutable segments, merging.

pub mod builder;
pub mod merge;
pub mod postings;
pub mod segment;

/// Engine-wide document identifier, assigned on insert and never reused.
pub type DocId = u64;
