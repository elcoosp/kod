//! Stopwords for the keyword component of hybrid retrieval (D2-B2).
//!
//! The previous retrieval used a crude "length >= 4" filter, which
//! kept "with", "have", "when", "does" and a long tail of other
//! short-but-meaningless tokens while discarding any two/three-letter
//! content word ("GPU", "SQL", "RPC"). The docs on the old
//! `search_long_term_relevant` admitted this: "a proper stopword list
//! and stemming belong with the embedding work".
//!
//! This is that list, plus a lightweight suffix stemmer.

/// English stopwords. Lowercase. Curated from the classic NLTK list
/// minus tokens that are meaningful in a coding context (`no`, `not`,
/// `now`, `new`, `on`, `off`, `up`, `down`, `left`, `right`, `in`,
/// `out` — a "not" in a bug report matters). The list keeps only words
/// that are never content-bearing.
pub const ENGLISH: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "am",
    "an",
    "and",
    "any",
    "are",
    "aren't",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "can't",
    "cannot",
    "could",
    "couldn't",
    "did",
    "didn't",
    "do",
    "does",
    "doesn't",
    "doing",
    "don't",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "hadn't",
    "has",
    "hasn't",
    "have",
    "haven't",
    "having",
    "he",
    "he'd",
    "he'll",
    "he's",
    "her",
    "here",
    "here's",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "how's",
    "i",
    "i'd",
    "i'll",
    "i'm",
    "i've",
    "if",
    "into",
    "is",
    "isn't",
    "it",
    "it's",
    "its",
    "itself",
    "let's",
    "me",
    "more",
    "most",
    "mustn't",
    "my",
    "myself",
    "of",
    "once",
    "only",
    "or",
    "other",
    "ought",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "same",
    "shan't",
    "she",
    "she'd",
    "she'll",
    "she's",
    "should",
    "shouldn't",
    "so",
    "some",
    "such",
    "than",
    "that",
    "that's",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "there's",
    "these",
    "they",
    "they'd",
    "they'll",
    "they're",
    "they've",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "very",
    "was",
    "wasn't",
    "we",
    "we'd",
    "we'll",
    "we're",
    "we've",
    "were",
    "weren't",
    "what",
    "what's",
    "when",
    "when's",
    "where",
    "where's",
    "which",
    "while",
    "who",
    "who's",
    "whom",
    "why",
    "why's",
    "with",
    "won't",
    "would",
    "wouldn't",
    "you",
    "you'd",
    "you'll",
    "you're",
    "you've",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

/// French stopwords. Lowercase. Same principle: keep tokens that can
/// be content in a technical sentence.
pub const FRENCH: &[&str] = &[
    "au", "aux", "avec", "ce", "ces", "dans", "de", "des", "du", "elle", "en", "et", "eux", "il",
    "ils", "je", "la", "le", "les", "leur", "lui", "ma", "mais", "me", "mes", "moi", "mon", "ne",
    "nos", "notre", "nous", "on", "ou", "par", "pas", "pour", "qu", "que", "qui", "sa", "se",
    "ses", "son", "sur", "ta", "te", "tes", "toi", "ton", "tu", "un", "une", "vos", "votre",
    "vous", "c", "d", "j", "l", "m", "n", "s", "t", "y", "été", "étée", "étées", "étés", "étant",
    "suis", "es", "est", "sommes", "êtes", "sont", "sera", "seront", "serait", "soit", "fut",
    "avait", "avaient", "aura", "auront", "aie", "ayant", "eu", "eue", "eues", "eus",
];

/// True if `word` (already lowercased) is a stopword in either list.
pub fn is_stopword(word: &str) -> bool {
    ENGLISH.contains(&word) || FRENCH.contains(&word)
}

/// Light suffix stemmer. Not a full Porter/Snowball — those are
/// ~1500 LOC and overkill for a heuristic that feeds a weighted
/// scorer where the semantic component (when available) dominates.
///
/// The suffixes chosen cover the most common English and French
/// inflections seen in a coding memory: plurals, gerunds, past
/// participles, French plural and adverb endings. A word shorter
/// than the suffix + 3 chars is left alone (so `is` + `ing` does not
/// become `is`).
pub fn stem(word: &str) -> String {
    let suffixes: &[&str] = &[
        "ements", "ement", "ations", "ation", "iques", "ique", "ing", "ies", "ied", "est", "ed",
        "es", "s", "eurs", "eur",
    ];
    for suf in suffixes {
        if word.len() > suf.len() + 3 && word.ends_with(suf) {
            return word[..word.len() - suf.len()].to_string();
        }
    }
    word.to_string()
}

/// Tokenize a text for keyword scoring: lowercase, split on
/// non-alphanumerics, drop stopwords, stem.
pub fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .filter(|w| !is_stopword(w))
        .map(stem)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_english_words_are_stopwords() {
        for w in ["the", "and", "with", "for", "that", "when"] {
            assert!(is_stopword(w), "{w} should be a stopword");
        }
    }

    #[test]
    fn coding_short_words_are_kept() {
        for w in ["gpu", "sql", "rpc", "not", "new", "off", "api"] {
            assert!(!is_stopword(w), "{w} should NOT be a stopword");
        }
    }

    #[test]
    fn french_stopwords() {
        for w in ["le", "la", "les", "de", "du", "une", "est"] {
            assert!(is_stopword(w), "{w} should be a stopword");
        }
    }

    #[test]
    fn stem_strips_common_suffixes() {
        assert_eq!(stem("running"), "runn");
        assert_eq!(stem("parsers"), "parser");
        assert_eq!(stem("refactoring"), "refactor");
        assert_eq!(stem("tables"), "tabl");
    }

    #[test]
    fn stem_leaves_short_words_alone() {
        assert_eq!(stem("is"), "is");
        assert_eq!(stem("id"), "id");
        assert_eq!(stem("io"), "io");
    }

    #[test]
    fn tokens_filter_and_stem() {
        let t = tokens("The parsers are running with data");
        assert!(t.contains(&"parser".to_string()));
        assert!(t.contains(&"runn".to_string()));
        assert!(t.contains(&"data".to_string()));
        assert!(!t.contains(&"the".to_string()));
        assert!(!t.contains(&"are".to_string()));
    }

    #[test]
    fn tokens_handle_punctuation() {
        let t = tokens("fix(a, b): returns RPC value.");
        assert!(t.contains(&"fix".to_string()));
        assert!(t.contains(&"return".to_string()));
        assert!(t.contains(&"rpc".to_string()));
        assert!(t.contains(&"value".to_string()));
    }
}

#[cfg(test)]
mod coverage_stemming {
    //! The stemmer is a hand-written suffix list, not a Porter. Its
    //! correctness condition is "does not overshoot" — a word must
    //! not shrink below the suffix+3 threshold — because an
    //! over-aggressive stemmer makes unrelated entries collide in
    //! the keyword index and silently merges distinct facts.
    use super::*;

    #[test]
    fn stem_does_not_shrink_short_words() {
        // The rule: `word.len() > suffix.len() + 3`. `ing` is 3, so
        // the word must be at least 7 chars to strip; anything under
        // stays whole. This is what stops "sing", "ring", "king"
        // from collapsing to "s", "r", "k".
        for short in ["sing", "ring", "king", "wing"] {
            assert_eq!(stem(short), short, "{short} was mangled");
        }
    }

    #[test]
    fn stem_strips_s_plural_from_content_words() {
        assert_eq!(stem("parsers"), "parser");
        assert_eq!(stem("handlers"), "handler");
        assert_eq!(stem("functions"), "function");
    }

    #[test]
    fn stem_keeps_two_letter_words_alone() {
        assert_eq!(stem("is"), "is");
        assert_eq!(stem("id"), "id");
        assert_eq!(stem("io"), "io");
        assert_eq!(stem("or"), "or");
    }

    #[test]
    fn tokens_drops_a_pure_stopword_query() {
        let t = tokens("the and or but of");
        assert!(t.is_empty(), "expected no tokens, got {t:?}");
    }

    #[test]
    fn tokens_preserves_numbers_and_identifiers() {
        let t = tokens("version 42 release v2");
        assert!(t.contains(&"version".to_string()));
        assert!(t.contains(&"42".to_string()));
        assert!(t.contains(&"release".to_string()));
        // `v2` has no stopword, no suffix match; must survive intact.
        assert!(t.contains(&"v2".to_string()));
    }

    #[test]
    fn tokens_handles_emoji_and_non_ascii() {
        // A prompt with emoji must not panic or drop the surrounding
        // words. The splitter walks by char, not byte.
        let t = tokens("deploy 🚀 to prod");
        assert!(t.contains(&"deploy".to_string()));
        assert!(t.contains(&"prod".to_string()));
    }

    #[test]
    fn is_stopword_is_case_sensitive_and_lowercase_only() {
        // The contract: callers lowercase before asking. A regression
        // that made this case-insensitive would double the list and
        // change every retrieval score without a visible failure.
        assert!(is_stopword("the"));
        assert!(!is_stopword("The"));
    }

    #[test]
    fn is_stopword_keeps_technical_short_words() {
        // The list was curated to keep short-but-meaningful tokens.
        // A regression that swapped in the plain NLTK list would
        // drop these and quietly break retrieval on technical prose.
        for keep in ["gpu", "sql", "rpc", "api", "not", "new", "off"] {
            assert!(!is_stopword(keep), "{keep} should NOT be a stopword");
        }
    }
}
