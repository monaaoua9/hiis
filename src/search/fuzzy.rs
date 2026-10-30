//! Edit-distance matching against the term dictionary.
//!
//! Distance is optimal-string-alignment (Levenshtein plus adjacent swaps, so
//! "quikc" is 1 away from "quick"). Candidates must share the first letter with
//! the query term: that turns a full dictionary scan into one prefix range, and
//! typos on the very first letter are rare enough to give up.

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::segment::Segment;

pub const MAX_DIST: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub term: String,
    pub dist: u32,
    pub doc_freq: u64,
}

/// Distance if it is `<= max`, otherwise None. Bails out as soon as a whole row is over budget.
pub fn distance(a: &[char], b: &[char], max: u32) -> Option<u32> {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) as u32 > max {
        return None;
    }
    if n == 0 || m == 0 {
        return Some(n.max(m) as u32);
    }

    let mut prev2 = vec![0u32; m + 1];
    let mut prev: Vec<u32> = (0..=m as u32).collect();
    let mut cur = vec![0u32; m + 1];

    for i in 1..=n {
        cur[0] = i as u32;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = (a[i - 1] != b[j - 1]) as u32;
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[m];
    (d <= max).then_some(d)
}

/// AUTO fuzziness: short words get no slack, long ones get two edits.
pub fn auto_dist(term: &str) -> u32 {
    match term.chars().count() {
        0..=2 => 0,
        3..=5 => 1,
        _ => 2,
    }
}

/// Dictionary terms within `dist` edits of `term`, the term itself included (distance 0)
/// when it is indexed. Best first: closest, then most common, then alphabetical.
pub fn expand(segments: &[Arc<Segment>], term: &str, dist: u32, limit: usize) -> Vec<Candidate> {
    let dist = dist.min(MAX_DIST);
    let q: Vec<char> = term.chars().collect();
    let Some(&first) = q.first() else { return Vec::new() };
    let head = first.to_string();

    let mut found: HashMap<&str, (u32, u64)> = HashMap::new();
    let mut buf: Vec<char> = Vec::new();
    for seg in segments {
        let terms = seg.terms();
        let from = terms.partition_point(|e| e.term.as_str() < head.as_str());
        for e in terms[from..].iter().take_while(|e| e.term.starts_with(&head)) {
            buf.clear();
            buf.extend(e.term.chars());
            if let Some(d) = distance(&q, &buf, dist) {
                let slot = found.entry(e.term.as_str()).or_insert((d, 0));
                slot.1 += e.doc_freq as u64;
            }
        }
    }

    let mut out: Vec<Candidate> = found
        .into_iter()
        .map(|(t, (d, df))| Candidate { term: t.to_string(), dist: d, doc_freq: df })
        .collect();
    out.sort_by(|a, b| a.dist.cmp(&b.dist).then(b.doc_freq.cmp(&a.doc_freq)).then_with(|| a.term.cmp(&b.term)));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(a: &str, b: &str, max: u32) -> Option<u32> {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        distance(&a, &b, max)
    }

    #[test]
    fn basic_edits() {
        assert_eq!(d("quick", "quick", 2), Some(0));
        assert_eq!(d("quick", "quik", 2), Some(1));
        assert_eq!(d("quick", "quack", 2), Some(1));
        assert_eq!(d("quick", "quiick", 2), Some(1));
        assert_eq!(d("kitten", "sitting", 3), Some(3));
        assert_eq!(d("kitten", "sitting", 2), None);
    }

    #[test]
    fn swap_is_one_edit() {
        assert_eq!(d("quikc", "quick", 1), Some(1));
        assert_eq!(d("ab", "ba", 1), Some(1));
        // OSA, not full Damerau: "ca" -> "abc" costs 3
        assert_eq!(d("ca", "abc", 3), Some(3));
    }

    #[test]
    fn empty_and_length_gap() {
        assert_eq!(d("", "ab", 2), Some(2));
        assert_eq!(d("", "abc", 2), None);
        assert_eq!(d("abcdef", "ab", 2), None);
        assert_eq!(d("", "", 0), Some(0));
    }

    #[test]
    fn unicode_counts_chars_not_bytes() {
        assert_eq!(d("مدرسه", "مدرسة", 1), Some(1));
        assert_eq!(d("café", "cafe", 1), Some(1));
    }

    #[test]
    fn matches_a_plain_dp_on_random_words() {
        // reference: full matrix, no early exit
        fn full(a: &[char], b: &[char]) -> u32 {
            let mut t = vec![vec![0u32; b.len() + 1]; a.len() + 1];
            for i in 0..=a.len() {
                t[i][0] = i as u32;
            }
            for j in 0..=b.len() {
                t[0][j] = j as u32;
            }
            for i in 1..=a.len() {
                for j in 1..=b.len() {
                    let c = (a[i - 1] != b[j - 1]) as u32;
                    let mut v = (t[i - 1][j] + 1).min(t[i][j - 1] + 1).min(t[i - 1][j - 1] + c);
                    if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                        v = v.min(t[i - 2][j - 2] + 1);
                    }
                    t[i][j] = v;
                }
            }
            t[a.len()][b.len()]
        }

        let mut x = 0x9e3779b97f4a7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..2000 {
            let (la, lb) = ((next() % 7) as usize, (next() % 7) as usize);
            let a: Vec<char> = (0..la).map(|_| (b'a' + (next() % 4) as u8) as char).collect();
            let b: Vec<char> = (0..lb).map(|_| (b'a' + (next() % 4) as u8) as char).collect();
            let want = full(&a, &b);
            for max in 0..=3 {
                assert_eq!(distance(&a, &b, max), (want <= max).then_some(want), "{a:?} {b:?} max {max}");
            }
        }
    }

    #[test]
    fn auto_scales_with_length() {
        assert_eq!(auto_dist("go"), 0);
        assert_eq!(auto_dist("fox"), 1);
        assert_eq!(auto_dist("search"), 2);
    }
}
