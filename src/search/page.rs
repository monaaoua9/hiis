//! Cursor pagination: "the next k hits after this one", no offsets.
//!
//! A cursor is the last hit of the previous page. Because ranking is a total
//! order (score, then lower doc id), everything strictly below that hit is the
//! next page, and each page costs a fresh top-k instead of re-scoring the
//! skipped ones. If documents are added or deleted between requests the idf
//! shifts and scores with it, so pages can overlap or skip a little; the
//! cursor still never goes backwards.

use std::fmt;
use std::str::FromStr;

use super::Hit;
use crate::error::Error;
use crate::index::DocId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    score_bits: u32,
    doc_id: DocId,
}

impl Cursor {
    pub fn after(hit: &Hit) -> Cursor {
        Cursor { score_bits: hit.score.to_bits(), doc_id: hit.doc_id }
    }

    pub(crate) fn as_hit(&self) -> Hit {
        Hit { doc_id: self.doc_id, score: f32::from_bits(self.score_bits) }
    }
}

// opaque to callers, but plain text so it survives a URL
impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}-{}", self.score_bits, self.doc_id)
    }
}

impl FromStr for Cursor {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let bad = || Error::Invalid(format!("not a cursor: {s:?}"));
        let (bits, id) = s.split_once('-').ok_or_else(bad)?;
        let score_bits = u32::from_str_radix(bits, 16).map_err(|_| bad())?;
        let doc_id = id.parse().map_err(|_| bad())?;
        // NaN never comes out of scoring, and total_cmp would rank it above everything
        if f32::from_bits(score_bits).is_nan() {
            return Err(bad());
        }
        Ok(Cursor { score_bits, doc_id })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub hits: Vec<Hit>,
    /// pass this back to get the following page; None when this one came up short
    pub next: Option<Cursor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let c = Cursor::after(&Hit { doc_id: 42, score: 3.25 });
        let s = c.to_string();
        assert_eq!(s.parse::<Cursor>().unwrap(), c);
        assert_eq!(c.as_hit(), Hit { doc_id: 42, score: 3.25 });
        let z = Cursor::after(&Hit { doc_id: 0, score: 0.0 });
        assert_eq!(z.to_string().parse::<Cursor>().unwrap(), z);
    }

    #[test]
    fn rejects_junk() {
        for s in ["", "-", "zz-1", "3fc00000", "3fc00000-x", "ffc00001-3", "3fc00000-1-2", "-5"] {
            assert!(s.parse::<Cursor>().is_err(), "{s:?}");
        }
    }
}
