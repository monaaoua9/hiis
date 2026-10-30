//! Autocomplete and "did you mean", both answered from the term dictionary.
//!
//! The dictionary holds stems, which are no good to show a user ("compress"
//! for "compression"), so every suggestion is mapped back to a word that really
//! occurs in a live document. That costs a few document reads per suggestion,
//! bounded by `SURFACE_DOCS`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::analysis::Analyzer;
use crate::index::segment::Segment;
use crate::search::fuzzy;

// how many documents we are willing to open looking for a readable form of one stem
const SURFACE_DOCS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub text: String,
    pub doc_freq: u64,
}

fn doc_freq(segs: &[Arc<Segment>], term: &str) -> u64 {
    segs.iter().filter_map(|s| s.term(term)).map(|e| e.doc_freq as u64).sum()
}

// a word from a live document that analyses to `stem` and passes `accept`
fn surface(segs: &[Arc<Segment>], an: &Analyzer, stem: &str, accept: impl Fn(&str) -> bool) -> Option<String> {
    let mut looked = 0;
    for seg in segs {
        let Some(entry) = seg.term(stem) else { continue };
        let Ok(mut cur) = seg.cursor(entry) else { continue };
        while let Ok(Some(local)) = cur.next() {
            if seg.is_deleted(local) {
                continue;
            }
            if let Some(text) = seg.text(local) {
                for t in an.analyze(&text) {
                    if t.term == stem {
                        let word = text[t.start..t.end].to_lowercase();
                        if accept(&word) {
                            return Some(word);
                        }
                    }
                }
            }
            looked += 1;
            if looked >= SURFACE_DOCS {
                return None;
            }
        }
    }
    None
}

/// Words that start with `prefix`, most widespread first. Typing past the end of a
/// stem works too: "compressi" still finds "compression".
pub fn complete(segs: &[Arc<Segment>], an: &Analyzer, prefix: &str, limit: usize) -> Vec<Suggestion> {
    let Some(p) = an.fold_prefix(prefix) else { return Vec::new() };
    if limit == 0 {
        return Vec::new();
    }

    let mut cands: HashMap<String, u64> = HashMap::new();
    for seg in segs {
        let terms = seg.terms();
        let from = terms.partition_point(|e| e.term.as_str() < p.as_str());
        for e in terms[from..].iter().take_while(|e| e.term.starts_with(&p)) {
            *cands.entry(e.term.clone()).or_default() += e.doc_freq as u64;
        }
        // stems cut words short: every proper prefix of what was typed is a possible stem
        let chars: Vec<char> = p.chars().collect();
        for n in chars.len().saturating_sub(4).max(2)..chars.len() {
            let stem: String = chars[..n].iter().collect();
            if let Some(e) = seg.term(&stem) {
                *cands.entry(stem).or_default() += e.doc_freq as u64;
            }
        }
    }

    let mut ranked: Vec<(String, u64)> = cands.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut out: Vec<Suggestion> = Vec::new();
    for (stem, df) in ranked.into_iter().take(limit * 3) {
        let Some(word) = surface(segs, an, &stem, |w| w.starts_with(&p)) else { continue };
        if out.iter().all(|s| s.text != word) {
            out.push(Suggestion { text: word, doc_freq: df });
        }
        if out.len() == limit {
            break;
        }
    }
    out
}

/// Closest real word for something that isn't in the index, if there is one.
pub fn correct_word(segs: &[Arc<Segment>], an: &Analyzer, word: &str) -> Option<Suggestion> {
    let toks = an.analyze(word);
    let [tok] = toks.as_slice() else { return None };
    if doc_freq(segs, &tok.term) > 0 {
        return None;
    }
    let dist = fuzzy::auto_dist(&tok.term).max(1);
    for c in fuzzy::expand(segs, &tok.term, dist, 8) {
        if c.dist == 0 {
            continue;
        }
        if let Some(w) = surface(segs, an, &c.term, |_| true) {
            return Some(Suggestion { text: w, doc_freq: c.doc_freq });
        }
    }
    None
}

/// Rewrites the words of a query that match nothing; None when there's nothing to fix.
/// Operators, quotes, parentheses and boosts are left where they are.
pub fn correct_query(segs: &[Arc<Segment>], an: &Analyzer, query: &str) -> Option<String> {
    let mut out = String::with_capacity(query.len());
    let mut changed = false;
    let mut word = String::new();

    let mut flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        // only plain alphabetic words; AND/OR/NOT and anything with wildcards or fuzziness is left alone
        let fix = if word.chars().all(char::is_alphabetic) && word.chars().count() >= 3 && !is_keyword(word) {
            correct_word(segs, an, word)
        } else {
            None
        };
        match fix {
            Some(s) => {
                out.push_str(&s.text);
                changed = true;
            }
            None => out.push_str(word),
        }
        word.clear();
    };

    // the characters that carry query syntax end a word; everything else (digits, ~, *, ^) is
    // glued to it, which makes "serch~" non-alphabetic and keeps it out of the fix
    for c in query.chars() {
        if c.is_whitespace() || matches!(c, '+' | '-' | '"' | '(' | ')') {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    changed.then_some(out)
}

fn is_keyword(w: &str) -> bool {
    matches!(w, "AND" | "OR" | "NOT")
}
