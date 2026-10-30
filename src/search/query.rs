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
//! ```
//!
//! AND / OR / NOT only count in capitals, lowercase `and` is a stopword like
//! always. Text is run through the same analyzer as documents, so `Indexing`
//! finds `indexed`, and stopwords inside a phrase leave the right gap.

use crate::analysis::Analyzer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occur {
    Must,
    Should,
    MustNot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clause {
    Term(String),
    // matches every indexed term starting with this (already folded, not stemmed)
    Prefix(String),
    // (term, offset from the first term of the phrase)
    Phrase(Vec<(String, u32)>),
    Group(Query),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryClause {
    pub occur: Occur,
    pub clause: Clause,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
            Clause::Phrase(p) => out.extend(p.iter().map(|x| x.0.as_str())),
            _ => {}
        });
        out.sort_unstable();
        out.dedup();
        out
    }
}

// deeper than this and the rest is read as plain text, so "((((((..." can't blow the stack
const MAX_DEPTH: usize = 16;

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

        let before = clauses.len();
        if chars[*i] == '(' && depth < MAX_DEPTH {
            *i += 1;
            let sub = parse_level(chars, i, an, depth + 1);
            if !sub.clauses.is_empty() {
                clauses.push(QueryClause { occur, clause: Clause::Group(sub) });
            }
        } else if chars[*i] == '"' {
            *i += 1;
            let s = *i;
            while *i < chars.len() && chars[*i] != '"' {
                *i += 1;
            }
            let text: String = chars[s..*i].iter().collect();
            *i = (*i + 1).min(chars.len());

            let toks = an.analyze(&text);
            match toks.len() {
                0 => {}
                1 => clauses.push(QueryClause { occur, clause: Clause::Term(toks[0].term.clone()) }),
                _ => {
                    let first = toks[0].position;
                    let parts = toks.into_iter().map(|t| (t.term, t.position - first)).collect();
                    clauses.push(QueryClause { occur, clause: Clause::Phrase(parts) });
                }
            }
        } else {
            let s = *i;
            while *i < chars.len() && !chars[*i].is_whitespace() && !(chars[*i] == ')' && depth > 0) {
                *i += 1;
            }
            let text: String = chars[s..*i].iter().collect();

            if occur == Occur::Should {
                match text.as_str() {
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

            let prefix = text.strip_suffix('*').and_then(|stem| an.fold_prefix(stem));
            if let Some(p) = prefix {
                clauses.push(QueryClause { occur, clause: Clause::Prefix(p) });
            } else {
                // "wi-fi" analyses to two terms; each gets the same occur
                for t in an.analyze(&text) {
                    clauses.push(QueryClause { occur, clause: Clause::Term(t.term) });
                }
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
        QueryClause { occur, clause: Clause::Term(t.into()) }
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
                QueryClause { occur: Occur::Must, clause: Clause::Prefix("runn".into()) },
                QueryClause { occur: Occur::MustNot, clause: Clause::Prefix("x".into()) },
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
    fn distinct_terms() {
        assert_eq!(parse("fox \"fox hunt\"").terms(), ["fox", "hunt"]);
    }
}
