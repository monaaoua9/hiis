pub struct RawToken<'a> {
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '\u{2019}'
}

// Arabic harakat, superscript alef and tatweel aren't alphanumeric but they
// sit inside words; splitting on them would shred vocalised text.
fn is_word_mark(c: char) -> bool {
    matches!(c, '\u{064B}'..='\u{065F}' | '\u{0670}' | '\u{0640}')
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_word_mark(c)
}

pub fn tokenize(text: &str) -> Vec<RawToken<'_>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut start: Option<usize> = None;

    for (i, &(b, c)) in chars.iter().enumerate() {
        // "don't" stays one token, a bare quote mark doesn't start or end one
        let inner_apos = is_apostrophe(c)
            && start.is_some()
            && chars.get(i + 1).is_some_and(|&(_, n)| n.is_alphanumeric());
        let word = is_word_char(c) || inner_apos;

        match (word, start) {
            (true, None) => start = Some(b),
            (false, Some(s)) => {
                out.push(RawToken { text: &text[s..b], start: s, end: b });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(RawToken { text: &text[s..], start: s, end: text.len() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<&str> {
        tokenize(s).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn splits_on_punctuation() {
        assert_eq!(words("Hello, world! foo_bar 42"), ["Hello", "world", "foo", "bar", "42"]);
    }

    #[test]
    fn keeps_inner_apostrophes() {
        assert_eq!(words("don't 'quoted' it's"), ["don't", "quoted", "it's"]);
    }

    #[test]
    fn offsets_point_into_source() {
        let s = "  ab cd";
        let t = tokenize(s);
        assert_eq!((t[0].start, t[0].end), (2, 4));
        assert_eq!(&s[t[1].start..t[1].end], "cd");
    }

    #[test]
    fn unicode_words() {
        assert_eq!(words("naïve café، مرحبا بالعالم"), ["naïve", "café", "مرحبا", "بالعالم"]);
    }

    #[test]
    fn vocalised_arabic_stays_whole() {
        assert_eq!(words("مَرْحَبًا").len(), 1);
    }
}
