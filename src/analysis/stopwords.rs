// must stay sorted, lookup is a binary search (checked in the test below)
const ENGLISH: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it", "no",
    "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these", "they", "this", "to",
    "was", "will", "with",
];

pub fn is_stopword(w: &str) -> bool {
    ENGLISH.binary_search(&w).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_sorted() {
        assert!(ENGLISH.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn lookup() {
        assert!(is_stopword("the"));
        assert!(!is_stopword("theory"));
    }
}
