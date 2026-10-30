//! Snippets with the query words marked.
//!
//! The document is re-analysed (token offsets point back into the original
//! text), every token the query would have matched is flagged, and the window
//! of `max_chars` that covers the most distinct query terms wins. Matching is
//! done on analysed terms, so `running` is marked for the query `run`, and
//! prefix / fuzzy clauses mark what they would have matched.

use std::collections::HashSet;

use crate::analysis::{Analyzer, Token};
use crate::search::fuzzy;
use crate::search::query::{Clause, Occur, Query};

#[derive(Debug, Clone)]
pub struct HighlightOpts {
    pub pre: String,
    pub post: String,
    pub max_chars: usize,
    /// escape `& < > "` in the document text (not in `pre` / `post`); on by default
    /// because the default markers are HTML
    pub escape_html: bool,
}

impl Default for HighlightOpts {
    fn default() -> Self {
        HighlightOpts { pre: "<em>".into(), post: "</em>".into(), max_chars: 160, escape_html: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// marked words in the snippet (runs of adjacent words count each word)
    pub matches: usize,
}

#[derive(Default)]
struct Matcher {
    exact: HashSet<String>,
    prefixes: Vec<String>,
    fuzzy: Vec<(Vec<char>, u32)>,
}

impl Matcher {
    fn from_query(q: &Query) -> Matcher {
        let mut m = Matcher::default();
        m.collect(q);
        m
    }

    fn collect(&mut self, q: &Query) {
        for qc in &q.clauses {
            if qc.occur == Occur::MustNot {
                continue;
            }
            match &qc.clause {
                Clause::Term(t) => {
                    self.exact.insert(t.clone());
                }
                Clause::Prefix(p) => self.prefixes.push(p.clone()),
                Clause::Phrase(p) | Clause::Near(p, _) => {
                    self.exact.extend(p.iter().map(|x| x.0.clone()));
                }
                Clause::Fuzzy(t, d) => self.fuzzy.push((t.chars().collect(), *d)),
                Clause::Group(g) => self.collect(g),
            }
        }
    }

    fn is_match(&self, term: &str) -> bool {
        if self.exact.contains(term) || self.prefixes.iter().any(|p| term.starts_with(p.as_str())) {
            return true;
        }
        if self.fuzzy.is_empty() {
            return false;
        }
        let t: Vec<char> = term.chars().collect();
        // same first-letter rule as the search side
        self.fuzzy.iter().any(|(q, d)| q.first() == t.first() && fuzzy::distance(q, &t, *d).is_some())
    }
}

pub fn highlight(an: &Analyzer, text: &str, q: &Query, opts: &HighlightOpts) -> Snippet {
    let m = Matcher::from_query(q);
    let toks = an.analyze(text);
    let flagged: Vec<bool> = toks.iter().map(|t| m.is_match(&t.term)).collect();

    // char offsets of every token, so the width budget is in characters, not bytes
    let mut cs = Vec::with_capacity(toks.len());
    let mut ce = Vec::with_capacity(toks.len());
    let (mut at_byte, mut at_char) = (0usize, 0usize);
    for t in &toks {
        at_char += text[at_byte..t.start].chars().count();
        cs.push(at_char);
        at_char += text[t.start..t.end].chars().count();
        ce.push(at_char);
        at_byte = t.end;
    }

    let Some((first, last)) = best_window(&toks, &flagged, &cs, &ce, opts.max_chars) else {
        return Snippet { text: head(text, opts), matches: 0 };
    };

    // spend the rest of the budget around the matches: a third before, the rest after
    let used = ce[last] - cs[first];
    let spare = opts.max_chars.saturating_sub(used);
    let (mut a, mut b) = (first, last);
    let back = spare / 3;
    while a > 0 && ce[last] - cs[a - 1] <= used + back {
        a -= 1;
    }
    while b + 1 < toks.len() && ce[b + 1] - cs[a] <= opts.max_chars {
        b += 1;
    }

    // stopwords aren't tokens, so the first/last token can have plain text beyond it
    let from = if a == 0 { 0 } else { toks[a].start };
    let to = if b + 1 == toks.len() { text.len() } else { toks[b].end };

    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    let mut at = from;
    let mut marked = 0;
    let mut i = a;
    while i <= b {
        if !flagged[i] {
            i += 1;
            continue;
        }
        // extend over neighbours that are separated by whitespace only
        let mut j = i;
        while j < b && flagged[j + 1] && text[toks[j].end..toks[j + 1].start].trim().is_empty() {
            j += 1;
        }
        push_text(&mut out, &text[at..toks[i].start], opts);
        out.push_str(&opts.pre);
        push_text(&mut out, &text[toks[i].start..toks[j].end], opts);
        out.push_str(&opts.post);
        marked += j - i + 1;
        at = toks[j].end;
        i = j + 1;
    }
    push_text(&mut out, &text[at..to], opts);
    if to < text.len() {
        out.push('…');
    }
    Snippet { text: out, matches: marked }
}

// (first, last) token indexes of the densest window, None if nothing matched
fn best_window(
    toks: &[Token],
    flagged: &[bool],
    cs: &[usize],
    ce: &[usize],
    max_chars: usize,
) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize, (usize, usize))> = None;
    let mut seen: Vec<&str> = Vec::new();

    for s in (0..toks.len()).filter(|&s| flagged[s]) {
        seen.clear();
        let mut total = 0;
        let mut last = s;
        for e in s..toks.len() {
            // a single word wider than the budget still gets its own window
            if e > s && ce[e] - cs[s] > max_chars {
                break;
            }
            if flagged[e] {
                total += 1;
                last = e;
                if !seen.contains(&toks[e].term.as_str()) {
                    seen.push(&toks[e].term);
                }
            }
        }
        let score = (seen.len(), total);
        if best.is_none_or(|b| score > b.2) {
            best = Some((s, last, score));
        }
    }
    best.map(|b| (b.0, b.1))
}

// no match at all: just the start of the text, cut at a word boundary
fn head(text: &str, opts: &HighlightOpts) -> String {
    let mut out = String::new();
    let cut = text.char_indices().nth(opts.max_chars).map_or(text.len(), |(b, _)| b);
    let mut s = &text[..cut];
    if cut < text.len() {
        if let Some(sp) = s.rfind(char::is_whitespace) {
            s = &s[..sp];
        }
    }
    push_text(&mut out, s.trim(), opts);
    if cut < text.len() {
        out.push('…');
    }
    out
}

fn push_text(out: &mut String, s: &str, opts: &HighlightOpts) {
    if !opts.escape_html {
        out.push_str(s);
        return;
    }
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, query: &str, max: usize) -> Snippet {
        let an = Analyzer::default();
        let q = Query::parse(query, &an);
        let opts = HighlightOpts { max_chars: max, ..HighlightOpts::default() };
        highlight(&an, text, &q, &opts)
    }

    #[test]
    fn marks_stemmed_matches() {
        let s = run("Foxes were running across the field", "run fox", 200);
        assert_eq!(s.text, "<em>Foxes</em> were <em>running</em> across the field");
        assert_eq!(s.matches, 2);
    }

    #[test]
    fn adjacent_words_share_one_marker() {
        let s = run("a quick brown fox jumped", "\"quick brown\"", 200);
        assert_eq!(s.text, "a <em>quick brown</em> fox jumped");
        assert_eq!(s.matches, 2);
        // punctuation between them breaks the run
        let s = run("quick, brown", "quick brown", 200);
        assert_eq!(s.text, "<em>quick</em>, <em>brown</em>");
    }

    #[test]
    fn negated_terms_are_not_marked() {
        let s = run("fox and dog", "fox -dog", 200);
        assert_eq!(s.text, "<em>fox</em> and dog");
    }

    #[test]
    fn prefix_and_fuzzy_clauses() {
        let s = run("compression of postings", "compres* postng~", 200);
        assert_eq!(s.text, "<em>compression</em> of <em>postings</em>");
    }

    #[test]
    fn picks_the_densest_window() {
        let filler = "lorem ipsum dolor sit amet ".repeat(20);
        let text = format!("fox {filler} the fox and the hound ran off {filler}");
        let s = run(&text, "fox hound", 60);
        assert!(s.text.contains("<em>fox</em> and the <em>hound</em>"), "{}", s.text);
        assert!(s.text.starts_with('…') && s.text.ends_with('…'));
        assert!(s.text.chars().count() < 60 + 40);
    }

    #[test]
    fn no_match_returns_the_head() {
        let s = run("one two three four five six seven", "zebra", 14);
        assert_eq!(s.matches, 0);
        assert_eq!(s.text, "one two three…");
        assert_eq!(run("short", "zebra", 50).text, "short");
    }

    #[test]
    fn html_is_escaped_but_markers_are_not() {
        let s = run("<b>fox</b> & \"co\"", "fox", 200);
        assert_eq!(s.text, "&lt;b&gt;<em>fox</em>&lt;/b&gt; &amp; &quot;co&quot;");
        let an = Analyzer::default();
        let q = Query::parse("fox", &an);
        let raw = HighlightOpts { escape_html: false, pre: "[".into(), post: "]".into(), ..Default::default() };
        assert_eq!(highlight(&an, "a <fox>", &q, &raw).text, "a <[fox]>");
    }

    #[test]
    fn multibyte_text_and_budget_in_chars() {
        let s = run("مرحبا بكم في مدرسة الكبيرة جدا", "مدرسه", 200);
        assert_eq!(s.text, "مرحبا بكم في <em>مدرسة</em> الكبيرة جدا");
        // "é" is two bytes; slicing must stay on char boundaries
        let s = run("café fox ünïcode tail text here", "fox", 12);
        assert!(s.text.contains("<em>fox</em>"));
    }

    #[test]
    fn empty_inputs() {
        assert_eq!(run("", "fox", 50).text, "");
        assert_eq!(run("fox", "", 50).text, "fox");
    }
}
