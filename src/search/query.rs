//! Query syntax:
//!
//! ```text
//! rust search          either term may match (scores add up)
//! +rust +search        both required
//! rust -java           must not contain `java`
//! "inverted index"     exact phrase
//! +"inverted index" bm25
//! ```
//!
//! Text is run through the same analyzer as documents, so `Indexing` finds
//! `indexed`, and stopwords inside a phrase leave the right gap.

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
    // (term, offset from the first term of the phrase)
    Phrase(Vec<(String, u32)>),
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
        let mut clauses = Vec::new();
        let mut i = 0;

        while i < chars.len() {
            if chars[i].is_whitespace() {
                i += 1;
                continue;
            }
            let occur = match chars[i] {
                '+' => {
                    i += 1;
                    Occur::Must
                }
                '-' => {
                    i += 1;
                    Occur::MustNot
                }
                _ => Occur::Should,
            };
            if i >= chars.len() {
                break;
            }

            if chars[i] == '"' {
                i += 1;
                let s = i;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                let text: String = chars[s..i].iter().collect();
                i = (i + 1).min(chars.len());

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
                let s = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                let text: String = chars[s..i].iter().collect();
                // "wi-fi" analyses to two terms; each gets the same occur
                for t in an.analyze(&text) {
                    clauses.push(QueryClause { occur, clause: Clause::Term(t.term) });
                }
            }
        }
        Query { clauses }
    }

    pub fn terms(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for c in &self.clauses {
            match &c.clause {
                Clause::Term(t) => out.push(t),
                Clause::Phrase(p) => out.extend(p.iter().map(|x| x.0.as_str())),
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
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
    fn distinct_terms() {
        assert_eq!(parse("fox \"fox hunt\"").terms(), ["fox", "hunt"]);
    }
}
