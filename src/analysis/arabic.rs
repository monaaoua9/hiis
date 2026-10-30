//! Light Arabic stemming: strip the article and clitic prefixes and the common
//! plural / pronoun suffixes, nothing root-based. Input is already folded
//! (alef variants -> alef, teh marbuta -> heh, alef maqsura -> yeh, no marks).
//!
//! Stems never get shorter than three letters, which keeps short words intact
//! and bounds the damage when the rules are wrong; they only have to be wrong
//! the same way at index and query time.

// longest first, the first one that fits wins
const PREFIXES: &[&str] = &["وال", "بال", "كال", "فال", "ولل", "لل", "ال"];
const SUFFIXES: &[&str] = &["ها", "ان", "ات", "ون", "ين", "يه", "ي", "ه"];

const MIN_STEM: usize = 3;

pub fn is_arabic(word: &str) -> bool {
    word.chars().next().is_some_and(|c| matches!(c, '\u{0600}'..='\u{06FF}'))
}

/// The word without its leading article / clitic ("المدرسه" -> "مدرسه"), if it has one.
pub fn strip_article(word: &str) -> Option<&str> {
    for p in ["وال", "بال", "كال", "فال", "ال"] {
        if let Some(rest) = word.strip_prefix(p) {
            if rest.chars().count() >= MIN_STEM {
                return Some(rest);
            }
        }
    }
    None
}

pub fn stem(word: &str) -> String {
    // the one very common word the suffix rule would mangle
    if word == "الله" {
        return word.to_string();
    }
    let mut chars: Vec<char> = word.chars().collect();

    let mut stripped = false;
    for p in PREFIXES {
        let n = p.chars().count();
        if chars.len() >= n + MIN_STEM && chars.iter().take(n).copied().eq(p.chars()) {
            chars.drain(..n);
            stripped = true;
            break;
        }
    }
    // a lone waw is "and"; only taken off when a real word of four letters is left
    if !stripped && chars.first() == Some(&'و') && chars.len() >= 5 {
        chars.remove(0);
    }

    for s in SUFFIXES {
        let n = s.chars().count();
        if chars.len() >= n + MIN_STEM && chars.iter().skip(chars.len() - n).copied().eq(s.chars()) {
            chars.truncate(chars.len() - n);
            break;
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_and_feminine_ending() {
        assert_eq!(stem("المدرسه"), "مدرس");
        assert_eq!(stem("مدرسه"), "مدرس");
        assert_eq!(stem("بالمدرسه"), "مدرس");
        assert_eq!(stem("وللمدرسه"), "مدرس");
    }

    #[test]
    fn plurals_meet_the_singular() {
        for w in ["معلمون", "معلمين", "معلمات", "المعلمون", "والمعلمين"] {
            assert_eq!(stem(w), "معلم", "{w}");
        }
    }

    #[test]
    fn pronoun_suffixes() {
        assert_eq!(stem("كتابها"), "كتاب");
        assert_eq!(stem("كتابه"), "كتاب");
    }

    #[test]
    fn short_words_survive() {
        assert_eq!(stem("علم"), "علم");
        assert_eq!(stem("بيت"), "بيت");
        assert_eq!(stem("ال"), "ال");
        assert_eq!(stem("الله"), "الله");
        // would leave two letters
        assert_eq!(stem("كان"), "كان");
        assert_eq!(stem("وزير"), "وزير");
    }

    #[test]
    fn leading_waw_only_with_a_long_remainder() {
        assert_eq!(stem("وكتاب"), "كتاب");
        assert_eq!(stem("ولد"), "ولد");
    }

    #[test]
    fn strip_article_only_when_something_is_left() {
        assert_eq!(strip_article("المدرسه"), Some("مدرسه"));
        assert_eq!(strip_article("والمدرسه"), Some("مدرسه"));
        assert_eq!(strip_article("الله"), None);
        assert_eq!(strip_article("مدرسه"), None);
    }

    #[test]
    fn script_detection() {
        assert!(is_arabic("كتاب"));
        assert!(!is_arabic("book"));
        assert!(!is_arabic(""));
    }
}
