//! Query syntax:
//!
//! ```text
//! rust search          either term may match (scores add up)
//! +rust +search        both required
//! rust -java           must not contain `java`
//! "inverted index"     exact phrase
//! +"inverted index" bm25
//! compres*             any term starting with `compres` (not stemmed, max 256 terms)
//! (rust OR go) -java   groups nest, and take +/- like any other clause
//! rust AND search      AND makes both sides required, OR leaves them optional
//! rust NOT java        same as -java
//! serch~               fuzzy: indexed terms within 1-2 edits (by word length), same first letter
//! serch~1              fuzzy with an explicit distance, 0 to 2
//! "quick fox"~3        proximity: the words within 3 positions of a phrase match, any order
//! title:fox            only the `title` field (documents added with `add_fields`)
//! title:"quick fox"    fields work on phrases, prefixes, fuzzy words and groups: `body:(rust OR go)`
//! title^2 "a b"^1.5    boost: the clause's score is multiplied (groups too: `(a b)^3`)
//! +recent^0            boost 0 keeps the clause as a filter that adds nothing to the score
//! ```
//!
//! AND / OR / NOT only count in capitals, lowercase `and` is a stopword like
//! always. Text is run through the same analyzer as documents, so `Indexing`
//! finds `indexed`, and stopwords inside a phrase leave the right gap.

use super::fuzzy;
use crate::analysis::Analyzer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occur {
    Must,
    Should,
    MustNot,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Term(String),
    // matches every indexed term starting with this (already folded, not stemmed)
    Prefix(String),
    // (term, offset from the first term of the phrase)
    Phrase(Vec<(String, u32)>),
    // analysed term, max edits
    Fuzzy(String, u32),
    // phrase with slop: same shape as Phrase, plus how far the words may drift
    Near(Vec<(String, u32)>, u32),
    Group(Query),
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryClause {
    pub occur: Occur,
    pub clause: Clause,
    pub boost: f32,
}

impl QueryClause {
    pub fn new(occur: Occur, clause: Clause) -> Self {
        QueryClause { occur, clause, boost: 1.0 }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub clauses: Vec<QueryClause>,
}

impl Query {
    pub fn parse(input: &str, an: &Analyzer) -> Query {
        let chars: Vec<char> = input.chars().collect();
        let mut i = 0;
        parse_level(&chars, &mut i, an, 0)
    }

    /// Every clause, groups flattened out (the group clause itself is not reported).
    pub fn visit<'a>(&'a self, f: &mut impl FnMut(&'a Clause)) {
        for c in &self.clauses {
            match &c.clause {
                Clause::Group(g) => g.visit(f),
                other => f(other),
            }
        }
    }

    pub fn terms(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        self.visit(&mut |c| match c {
            Clause::Term(t) => out.push(t),
            Clause::Phrase(p) | Clause::Near(p, _) => out.extend(p.iter().map(|x| x.0.as_str())),
            _ => {}
        });
        out.sort_unstable();
        out.dedup();
        out
    }
}

// deeper than this and the rest is read as plain text, so "((((((..." can't blow the stack
const MAX_DEPTH: usize = 16;
const MAX_SLOP: u32 = 1000;

// "title:" in front of a clause; the name has to look like a field name and something must follow
fn read_field(chars: &[char], i: &mut usize) -> Option<String> {
    let mut j = *i;
    while j < chars.len() && (chars[j].is_ascii_lowercase() || chars[j].is_ascii_digit() || chars[j] == '_') {
        j += 1;
    }
    let name: String = chars[*i..j].iter().collect();
    let follows = chars.get(j + 1).is_some_and(|c| c.is_alphanumeric() || *c == '"' || *c == '(');
    if chars.get(j) == Some(&':') && follows && crate::fields::valid_name(&name) {
        *i = j + 1;
        return Some(name);
    }
    None
}

// point a parsed clause at one field; terms that already carry one are left alone
fn qualify(c: &mut Clause, f: &str) {
    let q = |t: &mut String| {
        if !t.contains(':') {
            *t = format!("{f}:{t}");
        }
    };
    match c {
        Clause::Term(t) | Clause::Prefix(t) | Clause::Fuzzy(t, _) => q(t),
        Clause::Phrase(p) | Clause::Near(p, _) => p.iter_mut().for_each(|x| q(&mut x.0)),
        Clause::Group(g) => g.clauses.iter_mut().for_each(|qc| qualify(&mut qc.clause, f)),
    }
}

const MAX_BOOST: f32 = 1000.0;

fn parse_boost(num: &str) -> Option<f32> {
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    num.parse::<f32>().ok().map(|b| b.min(MAX_BOOST))
}

// "word^2.5" -> ("word", Some(2.5)); anything that doesn't parse stays part of the word
fn split_boost(text: &str) -> (&str, Option<f32>) {
    if let Some((word, tail)) = text.rsplit_once('^') {
        if !word.is_empty() {
            if let Some(b) = parse_boost(tail) {
                return (word, Some(b));
            }
        }
    }
    (text, None)
}

// after a closing quote or paren
fn read_boost(chars: &[char], i: &mut usize) -> Option<f32> {
    if chars.get(*i) != Some(&'^') {
        return None;
    }
    let n = chars[*i + 1..].iter().take_while(|c| c.is_ascii_digit() || **c == '.').count();
    let b = parse_boost(&chars[*i + 1..*i + 1 + n].iter().collect::<String>())?;
    *i += 1 + n;
    Some(b)
}

// "word~" or "word~2"; the number is clamped to what fuzzy matching supports
fn split_fuzzy(text: &str) -> Option<(&str, Option<u32>)> {
    let (word, tail) = text.rsplit_once('~')?;
    if word.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let dist = tail.parse::<u32>().ok().map(|d| d.min(fuzzy::MAX_DIST));
    Some((word, dist))
}

fn parse_level(chars: &[char], i: &mut usize, an: &Analyzer, depth: usize) -> Query {
    let mut clauses: Vec<QueryClause> = Vec::new();
    let (mut and, mut not) = (false, false);

    while *i < chars.len() {
        let c = chars[*i];
        if c.is_whitespace() {
            *i += 1;
            continue;
        }
        if c == ')' && depth > 0 {
            *i += 1;
            break;
        }
        if c == ')' {
            *i += 1;
            continue;
        }

        let mut occur = match c {
            '+' => Occur::Must,
            '-' => Occur::MustNot,
            _ => Occur::Should,
        };
        if occur != Occur::Should {
            *i += 1;
            if *i >= chars.len() {
                break;
            }
        }

        let field = read_field(chars, i);
        let before = clauses.len();
        let boost: f32;
        if chars[*i] == '(' && depth < MAX_DEPTH {
            *i += 1;
            let sub = parse_level(chars, i, an, depth + 1);
            boost = read_boost(chars, i).unwrap_or(1.0);
            if !sub.clauses.is_empty() {
                clauses.push(QueryClause::new(occur, Clause::Group(sub)));
            }
        } else if chars[*i] == '"' {
            *i += 1;
            let s = *i;
            while *i < chars.len() && chars[*i] != '"' {
                *i += 1;
            }
            let text: String = chars[s..*i].iter().collect();
            *i = (*i + 1).min(chars.len());

            let mut slop = 0u32;
            if chars.get(*i) == Some(&'~') {
                let digits = chars[*i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
                if digits > 0 {
                    let n: String = chars[*i + 1..*i + 1 + digits].iter().collect();
                    slop = n.parse::<u32>().unwrap_or(MAX_SLOP).min(MAX_SLOP);
                    *i += 1 + digits;
                }
            }
            boost = read_boost(chars, i).unwrap_or(1.0);

            let toks = an.analyze(&text);
            match toks.len() {
                0 => {}
                1 => clauses.push(QueryClause::new(occur, Clause::Term(toks[0].term.clone()))),
                _ => {
                    let first = toks[0].position;
                    let parts = toks.into_iter().map(|t| (t.term, t.position - first)).collect();
                    let clause = if slop == 0 { Clause::Phrase(parts) } else { Clause::Near(parts, slop) };
                    clauses.push(QueryClause::new(occur, clause));
                }
            }
        } else {
            let s = *i;
            while *i < chars.len() && !chars[*i].is_whitespace() && !(chars[*i] == ')' && depth > 0) {
                *i += 1;
            }
            let text: String = chars[s..*i].iter().collect();
            let (text, b) = split_boost(&text);
            boost = b.unwrap_or(1.0);

            if occur == Occur::Should {
                match text {
                    "AND" => {
                        and = true;
                        continue;
                    }
                    "OR" => {
                        and = false;
                        continue;
                    }
                    "NOT" => {
                        not = true;
                        continue;
                    }
                    _ => {}
                }
            }

            if let Some((word, dist)) = split_fuzzy(text) {
                for t in an.analyze(word) {
                    let d = dist.unwrap_or_else(|| fuzzy::auto_dist(&t.term));
                    clauses.push(QueryClause::new(occur, Clause::Fuzzy(t.term, d)));
                }
            } else if let Some(p) = text.strip_suffix('*').and_then(|stem| an.fold_prefix(stem)) {
                clauses.push(QueryClause::new(occur, Clause::Prefix(p)));
            } else {
                // "wi-fi" analyses to two terms; each gets the same occur
                for t in an.analyze(text) {
                    clauses.push(QueryClause::new(occur, Clause::Term(t.term)));
                }
            }
        }

        if let Some(f) = &field {
            for qc in &mut clauses[before..] {
                qualify(&mut qc.clause, f);
            }
        }

        if clauses.len() > before {
            if not {
                occur = Occur::MustNot;
            } else if and && occur == Occur::Should {
                occur = Occur::Must;
            }
            for qc in &mut clauses[before..] {
                qc.occur = occur;
                qc.boost = boost;
            }
            // the left side of an AND is promoted too, unless it was already negated
            if and && before > 0 && clauses[before - 1].occur == Occur::Should {
                clauses[before - 1].occur = Occur::Must;
            }
            and = false;
            not = false;
        }
    }
    Query { clauses }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Query {
        Query::parse(s, &Analyzer::default())
    }

    fn term(occur: Occur, t: &str) -> QueryClause {
        QueryClause::new(occur, Clause::Term(t.into()))
    }

    #[test]
    fn plain_must_and_not() {
        let q = parse("rust +search -java");
        assert_eq!(
            q.clauses,
            [term(Occur::Should, "rust"), term(Occur::Must, "search"), term(Occur::MustNot, "java")]
        );
    }

    #[test]
    fn phrase_with_stopword_gap() {
        let q = parse("\"state of the art\"");
        assert_eq!(q.clauses[0].clause, Clause::Phrase(vec![("state".into(), 0), ("art".into(), 3)]));
    }

    #[test]
    fn one_word_phrase_is_a_term() {
        assert_eq!(parse("+\"Running\"").clauses, [term(Occur::Must, "run")]);
    }

    #[test]
    fn stopword_only_query_is_empty() {
        assert!(parse("the of and").clauses.is_empty());
    }

    #[test]
    fn unterminated_quote_and_lone_operators() {
        assert_eq!(parse("\"quick fox").clauses.len(), 1);
        assert!(parse("- + ").clauses.is_empty());
        assert!(parse("").clauses.is_empty());
    }

    #[test]
    fn hyphen_inside_word_is_not_negation() {
        let q = parse("wi-fi");
        assert_eq!(q.clauses, [term(Occur::Should, "wi"), term(Occur::Should, "fi")]);
    }

    #[test]
    fn prefix_is_folded_not_stemmed() {
        let q = parse("+Runn* -x*");
        assert_eq!(
            q.clauses,
            [
                QueryClause::new(Occur::Must, Clause::Prefix("runn".into())),
                QueryClause::new(Occur::MustNot, Clause::Prefix("x".into())),
            ]
        );
        // a lone star has nothing to expand, falls through to the normal path
        assert!(parse("*").clauses.is_empty());
    }

    #[test]
    fn keywords_set_occur() {
        let q = parse("rust AND search NOT java");
        assert_eq!(
            q.clauses,
            [term(Occur::Must, "rust"), term(Occur::Must, "search"), term(Occur::MustNot, "java")]
        );
        let q = parse("rust OR go");
        assert_eq!(q.clauses, [term(Occur::Should, "rust"), term(Occur::Should, "go")]);
        // lowercase stays a stopword
        assert_eq!(parse("rust and go").clauses.len(), 2);
    }

    #[test]
    fn and_not_still_requires_the_left_side() {
        let q = parse("rust AND NOT java");
        assert_eq!(q.clauses, [term(Occur::Must, "rust"), term(Occur::MustNot, "java")]);
    }

    #[test]
    fn groups_nest() {
        let q = parse("+(rust (go -zig)) java");
        assert_eq!(q.clauses.len(), 2);
        let Clause::Group(g) = &q.clauses[0].clause else { panic!("not a group") };
        assert_eq!(q.clauses[0].occur, Occur::Must);
        assert_eq!(g.clauses.len(), 2);
        assert!(matches!(g.clauses[1].clause, Clause::Group(_)));
        assert_eq!(q.terms(), ["go", "java", "rust", "zig"]);
    }

    #[test]
    fn empty_and_unclosed_groups() {
        assert!(parse("()").clauses.is_empty());
        assert_eq!(parse("(fox").clauses.len(), 1);
        assert_eq!(parse("fox)").clauses, [term(Occur::Should, "fox")]);
    }

    #[test]
    fn fuzzy_suffix() {
        let f = |t: &str, d: u32| QueryClause::new(Occur::Should, Clause::Fuzzy(t.into(), d));
        assert_eq!(parse("quikc~").clauses, [f("quikc", 1)]);
        assert_eq!(parse("quikcer~").clauses, [f("quikcer", 2)]);
        assert_eq!(parse("quick~0").clauses, [f("quick", 0)]);
        // clamped, and a stem is what gets compared
        assert_eq!(parse("running~9").clauses, [f("run", 2)]);
        assert_eq!(parse("fox~ ").clauses, [f("fox", 1)]);
        // not fuzzy: no word, or junk after the tilde
        assert!(parse("~").clauses.is_empty());
        assert_eq!(parse("fox~bar").clauses.len(), 2);
    }

    #[test]
    fn slop_after_the_closing_quote() {
        let q = parse("\"quick brown fox\"~4 +\"quick fox\"~0");
        assert_eq!(
            q.clauses[0].clause,
            Clause::Near(vec![("quick".into(), 0), ("brown".into(), 1), ("fox".into(), 2)], 4)
        );
        // ~0 is an ordinary phrase
        assert!(matches!(q.clauses[1].clause, Clause::Phrase(_)));
        // a one-word "phrase" with slop is still a term
        assert_eq!(parse("\"fox\"~3").clauses, [term(Occur::Should, "fox")]);
        // tilde without digits isn't slop
        assert_eq!(parse("\"quick fox\"~").clauses.len(), 1);
        assert_eq!(parse("\"quick fox\"~99999999999999999999").clauses.len(), 1);
    }

    #[test]
    fn boosts() {
        let q = parse("fox^2 \"quick brown\"^0.5 (a1 b1)^3 +cat^0 dog^x");
        let b: Vec<f32> = q.clauses.iter().map(|c| c.boost).collect();
        assert_eq!(b, [2.0, 0.5, 3.0, 0.0, 1.0, 1.0]);
        assert_eq!(parse("serch~1^4").clauses[0].boost, 4.0);
        assert_eq!(parse("comp*^2").clauses[0].boost, 2.0);
        assert_eq!(parse("fox^99999999").clauses[0].boost, MAX_BOOST);
        // no word before the caret, or a bare caret: not a boost
        assert_eq!(parse("^2").clauses.len(), 1);
        assert_eq!(parse("fox^").clauses[0].boost, 1.0);
    }

    #[test]
    fn field_prefixes() {
        let q = parse("title:fox +body:\"quick brown\" -title:comp* body:(rust OR title:go) tit:fuzzi~1");
        assert_eq!(q.clauses[0].clause, Clause::Term("title:fox".into()));
        assert_eq!(q.clauses[1].occur, Occur::Must);
        assert_eq!(
            q.clauses[1].clause,
            Clause::Phrase(vec![("body:quick".into(), 0), ("body:brown".into(), 1)])
        );
        assert_eq!(q.clauses[2].clause, Clause::Prefix("title:comp".into()));
        let Clause::Group(g) = &q.clauses[3].clause else { panic!() };
        assert_eq!(g.clauses[0].clause, Clause::Term("body:rust".into()));
        // an inner field wins over the outer one
        assert_eq!(g.clauses[1].clause, Clause::Term("title:go".into()));
        assert_eq!(q.clauses[4].clause, Clause::Fuzzy("tit:fuzzi".into(), 1));
    }

    #[test]
    fn colons_that_are_not_fields() {
        // uppercase / digit-first names, trailing colon, nothing after it
        for s in ["Title:fox", "1a:fox", "note: fox", "a:", "http://x.org"] {
            let q = parse(s);
            q.visit(&mut |c| {
                if let Clause::Term(t) = c {
                    assert!(!t.contains(':'), "{s}: {t}");
                }
            });
        }
    }

    #[test]
    fn field_boost_goes_through() {
        let q = parse("title:fox^3");
        assert_eq!(q.clauses[0].clause, Clause::Term("title:fox".into()));
        assert_eq!(q.clauses[0].boost, 3.0);
    }

    #[test]
    fn distinct_terms() {
        assert_eq!(parse("fox \"fox hunt\"").terms(), ["fox", "hunt"]);
    }
}
