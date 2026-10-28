//! Okapi BM25.
//!
//! score(d, q) = sum over terms t of  idf(t) * tf * (k1 + 1) / (tf + k1 * (1 - b + b * |d| / avgdl))
//! idf(t)      = ln(1 + (N - n_t + 0.5) / (n_t + 0.5))
//!
//! The `1 +` inside the log is the Lucene/Robertson variant: plain BM25 idf
//! goes negative for terms present in more than half the corpus, which would
//! make matching a common word *lower* a document's score.

#[derive(Debug, Clone, Copy)]
pub struct Bm25 {
    pub k1: f32,
    pub b: f32,
}

impl Default for Bm25 {
    fn default() -> Self {
        Bm25 { k1: 1.2, b: 0.75 }
    }
}

impl Bm25 {
    pub fn idf(n_docs: u64, doc_freq: u64) -> f32 {
        let n = n_docs as f64;
        // doc_freq counts tombstoned docs too, so it can briefly exceed N
        let df = (doc_freq as f64).min(n);
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln() as f32
    }

    /// Largest score any doc can get from a term whose tf is at most `max_tf`:
    /// score rises with tf and falls with length, so take dl = 0.
    pub fn upper_bound(&self, idf: f32, max_tf: u32) -> f32 {
        let floor = 1.0 - self.b;
        if floor <= 0.0 {
            return f32::INFINITY; // b >= 1: short docs make the bound unbounded, so no pruning
        }
        let tf = max_tf as f32;
        idf * tf * (self.k1 + 1.0) / (tf + self.k1 * floor)
    }

    #[inline]
    pub fn score(&self, idf: f32, tf: f32, doc_len: f32, avg_len: f32) -> f32 {
        let norm = 1.0 - self.b + self.b * doc_len / avg_len.max(1.0);
        idf * tf * (self.k1 + 1.0) / (tf + self.k1 * norm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rarer_terms_weigh_more() {
        assert!(Bm25::idf(1000, 2) > Bm25::idf(1000, 500));
    }

    #[test]
    fn idf_stays_positive() {
        assert!(Bm25::idf(100, 100) > 0.0);
        assert!(Bm25::idf(100, 150) > 0.0);
    }

    #[test]
    fn tf_saturates() {
        let m = Bm25::default();
        let s1 = m.score(1.0, 1.0, 10.0, 10.0);
        let s10 = m.score(1.0, 10.0, 10.0, 10.0);
        let s100 = m.score(1.0, 100.0, 10.0, 10.0);
        assert!(s1 < s10 && s10 < s100);
        assert!(s100 < m.k1 + 1.0 + 1e-3); // asymptote is idf * (k1 + 1)
        assert!(s100 - s10 < s10 - s1);
    }

    #[test]
    fn longer_docs_score_lower() {
        let m = Bm25::default();
        assert!(m.score(1.0, 2.0, 5.0, 10.0) > m.score(1.0, 2.0, 50.0, 10.0));
    }

    #[test]
    fn matches_hand_computation() {
        // N=10, n=1, tf=1, |d|=avgdl  ->  idf*(k1+1)/(1+k1)  = idf
        let idf = Bm25::idf(10, 1);
        let want = (1.0f64 + 9.5 / 1.5).ln() as f32;
        assert!((idf - want).abs() < 1e-6);
        assert!((Bm25::default().score(idf, 1.0, 8.0, 8.0) - idf).abs() < 1e-5);
    }
}
