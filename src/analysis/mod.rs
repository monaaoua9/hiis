//! Text analysis: raw text in, positioned index terms out.
//!
//! Pipeline: tokenize -> fold (lowercase, strip possessives/apostrophes,
//! Arabic orthographic folding) -> drop stopwords -> Porter stem.
//!
//! Token positions are assigned *before* stopword removal, so "quick the fox"
//! keeps `quick` and `fox` two apart. That is what makes phrase queries with
//! stopwords inside them match the same way at index and query time.

pub(crate) mod arabic;
mod stemmer;
mod stopwords;
mod tokenizer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub term: String,
    pub position: u32,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
pub struct AnalyzerConfig {
    pub remove_stopwords: bool,
    pub stem: bool,
    // light stemming for Arabic words (needs `stem` too); English rules leave them alone anyway
    pub arabic_stem: bool,
    // longer than this is almost always base64 / hashes / garbage, not a word
    pub max_token_chars: usize,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        AnalyzerConfig { remove_stopwords: true, stem: true, arabic_stem: true, max_token_chars: 64 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Analyzer {
    cfg: AnalyzerConfig,
}

impl Analyzer {
    pub fn new(cfg: AnalyzerConfig) -> Self {
        Analyzer { cfg }
    }

    pub fn analyze(&self, text: &str) -> Vec<Token> {
        let mut out = Vec::new();
        for (pos, raw) in tokenizer::tokenize(text).into_iter().enumerate() {
            if let Some(term) = self.normalize(raw.text) {
                out.push(Token { term, position: pos as u32, start: raw.start, end: raw.end });
            }
        }
        out
    }

    // for prefix queries: folded but not stemmed, since the user typed a fragment of a word
    pub fn fold_prefix(&self, raw: &str) -> Option<String> {
        let lowered = raw.to_lowercase();
        let folded: String = lowered.chars().filter_map(fold_char).collect();
        (!folded.is_empty()).then_some(folded)
    }

    fn normalize(&self, raw: &str) -> Option<String> {
        // most tokens are plain ascii; skip the unicode machinery for them
        let mut folded = if raw.is_ascii() { raw.to_ascii_lowercase() } else { raw.to_lowercase() };
        if let Some(n) = folded.strip_suffix("'s").or_else(|| folded.strip_suffix("\u{2019}s")).map(str::len)
        {
            folded.truncate(n);
        }
        if folded.chars().any(|c| fold_char(c) != Some(c)) {
            folded = folded.chars().filter_map(fold_char).collect();
        }
        if folded.is_empty() || folded.chars().count() > self.cfg.max_token_chars {
            return None;
        }
        if self.cfg.remove_stopwords && stopwords::is_stopword(&folded) {
            return None;
        }
        Some(match (self.cfg.stem, arabic::is_arabic(&folded)) {
            (false, _) => folded,
            (true, false) => stemmer::stem(folded),
            (true, true) if self.cfg.arabic_stem => arabic::stem(&folded),
            (true, true) => folded,
        })
    }
}

// None = drop the char
fn fold_char(c: char) -> Option<char> {
    match c {
        '\'' | '\u{2019}' => None,
        // tashkeel + tatweel
        '\u{064B}'..='\u{065F}' | '\u{0670}' | '\u{0640}' => None,
        // alef with hamza/madda -> bare alef
        '\u{0622}' | '\u{0623}' | '\u{0625}' => Some('\u{0627}'),
        // alef maqsura -> yeh, teh marbuta -> heh
        '\u{0649}' => Some('\u{064A}'),
        '\u{0629}' => Some('\u{0647}'),
        _ => Some(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(s: &str) -> Vec<String> {
        Analyzer::default().analyze(s).into_iter().map(|t| t.term).collect()
    }

    #[test]
    fn full_pipeline() {
        assert_eq!(terms("The Quick Foxes are Running!"), ["quick", "fox", "run"]);
    }

    #[test]
    fn possessives_and_contractions() {
        assert_eq!(terms("John's dog doesn't bark"), ["john", "dog", "doesnt", "bark"]);
    }

    #[test]
    fn stopword_gap_is_kept_in_positions() {
        let toks = Analyzer::default().analyze("quick the fox");
        assert_eq!(toks.iter().map(|t| t.position).collect::<Vec<_>>(), [0, 2]);
    }

    #[test]
    fn arabic_folding() {
        assert_eq!(terms("أحمد"), terms("احمد"));
        assert_eq!(terms("مَدْرَسَة"), terms("مدرسه"));
    }

    #[test]
    fn arabic_forms_collapse() {
        assert_eq!(terms("المدرسة"), terms("مدرسه"));
        assert_eq!(terms("المعلمون"), terms("معلمين"));
        assert_eq!(terms("المدرسة"), ["مدرس"]);
        // the Latin side is untouched
        assert_eq!(terms("Running مدرسة"), ["run", "مدرس"]);
    }

    #[test]
    fn arabic_stopwords_leave_a_gap() {
        let toks = Analyzer::default().analyze("كتاب في المكتبة");
        assert_eq!(toks.iter().map(|t| t.position).collect::<Vec<_>>(), [0, 2]);
    }

    #[test]
    fn arabic_stemming_can_be_switched_off() {
        let a = Analyzer::new(AnalyzerConfig { arabic_stem: false, ..Default::default() });
        let t: Vec<_> = a.analyze("المدرسة").into_iter().map(|t| t.term).collect();
        assert_eq!(t, ["المدرسه"]);
    }

    #[test]
    fn config_switches() {
        let a = Analyzer::new(AnalyzerConfig { remove_stopwords: false, stem: false, ..Default::default() });
        let t: Vec<_> = a.analyze("The Running").into_iter().map(|t| t.term).collect();
        assert_eq!(t, ["the", "running"]);
    }

    #[test]
    fn overlong_tokens_dropped() {
        assert!(terms(&"x".repeat(200)).is_empty());
    }
}
