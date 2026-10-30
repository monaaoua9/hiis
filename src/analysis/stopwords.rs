// must stay sorted, lookup is a binary search (checked in the test below)
const ENGLISH: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it", "no",
    "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these", "they", "this", "to",
    "was", "will", "with",
];

// folded forms (the analyzer runs this after folding): no hamza, teh marbuta as heh, maqsura as yeh
const ARABIC: &[&str] = &[
    "اذا",
    "التي",
    "الذي",
    "الذين",
    "الي",
    "ان",
    "انا",
    "انت",
    "او",
    "اي",
    "ايضا",
    "اين",
    "بعد",
    "بل",
    "به",
    "بها",
    "بين",
    "تلك",
    "ثم",
    "حتي",
    "ذلك",
    "علي",
    "عليه",
    "عليها",
    "عن",
    "عند",
    "في",
    "فيه",
    "فيها",
    "قبل",
    "قد",
    "كان",
    "كانت",
    "كل",
    "كما",
    "كيف",
    "لا",
    "لكن",
    "لم",
    "لماذا",
    "لن",
    "له",
    "لها",
    "ما",
    "متي",
    "مع",
    "من",
    "منه",
    "منها",
    "نحن",
    "هذا",
    "هذه",
    "هم",
    "هن",
    "هنا",
    "هناك",
    "هو",
    "هي",
    "يكون",
];

pub fn is_stopword(w: &str) -> bool {
    // the English list is all ascii and the Arabic one never is
    if w.is_ascii() {
        ENGLISH.binary_search(&w).is_ok()
    } else {
        ARABIC.binary_search(&w).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_sorted() {
        assert!(ENGLISH.windows(2).all(|w| w[0] < w[1]));
        assert!(ARABIC.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn lists_stay_in_their_script() {
        // is_stopword only looks in the list that matches the word's script
        assert!(ENGLISH.iter().all(|w| w.is_ascii()));
        assert!(ARABIC.iter().all(|w| !w.is_ascii()));
    }

    #[test]
    fn arabic_lookup() {
        assert!(is_stopword("في"));
        assert!(is_stopword("التي"));
        assert!(!is_stopword("كتاب"));
    }

    #[test]
    fn lookup() {
        assert!(is_stopword("the"));
        assert!(!is_stopword("theory"));
    }
}
