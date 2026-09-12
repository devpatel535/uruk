//! Text into terms.
//!
//! Two decisions were made deliberately in `RESEARCH.md` and are implemented
//! here rather than left to drift.
//!
//! **Stopwords are kept** (§5.7). Dropping "the", "of" and "to" was a
//! disk-space measure from the era when disks were small, and it breaks phrase
//! search: `"to be or not to be"` becomes an empty query. Since the brief
//! wants quoted phrases to actually work, common words stay in the index. The
//! cost of that is paid at query time by skipping blocks that cannot enter the
//! top ten, not at index time by throwing information away.
//!
//! **Nothing is stemmed** (§5.8). Reducing "running" to "run" at index time is
//! lossy and irreversible: once `run` is stored, an exact search for `running`
//! cannot be answered, and exact predictable matching is the product. If
//! recall turns out to be too low, a *separate* stemmed field can be added
//! later with a lower weight, which keeps the decision reversible.
//!
//! What is dropped is junk: tokens too long to be words, which are usually
//! base64, minified identifiers or tracking blobs. Heaps' law says vocabulary
//! grows without bound on web text, and on the web most of that growth is
//! noise that costs a dictionary entry each (§3.3).

/// Longest token we will index.
///
/// Real words in any language fit comfortably; German compounds are the
/// practical upper bound and they stay well under this. Above it, a "word" is
/// almost always a hash, a base64 fragment or a minified identifier.
pub const MAX_TERM_LEN: usize = 40;

/// Longest run of digits we treat as a term.
///
/// Years, page numbers and quantities are worth indexing. A 30-digit number is
/// an identifier that will never be searched for and costs a dictionary entry.
pub const MAX_DIGIT_RUN: usize = 12;

/// A term and where it appeared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub term: String,
    /// Ordinal within this field's token stream, counting from zero. Phrase
    /// search compares these; proximity scoring measures gaps between them.
    pub position: u32,
    /// Byte offset of the token in the original text.
    ///
    /// The index does not need this — positions are what postings store — but
    /// snippets do: showing the matching sentence means cutting the *original*
    /// text, punctuation and capitals intact, not reassembling terms.
    pub start: usize,
    /// Byte offset just past the token in the original text.
    pub end: usize,
}

/// Is this worth an entry in the term dictionary?
fn is_useful(term: &str) -> bool {
    if term.is_empty() || term.len() > MAX_TERM_LEN {
        return false;
    }
    // Long digit strings are identifiers, not words.
    if term.len() > MAX_DIGIT_RUN && term.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    true
}

/// Split text into lowercase terms with positions.
///
/// Splitting is on anything that is not alphanumeric, which keeps the rule
/// simple and works for any script `char::is_alphanumeric` knows about rather
/// than only for Latin text.
///
/// Positions count *every* token found, including ones later dropped as junk,
/// so that a discarded token still separates its neighbours. Otherwise a
/// phrase search for `"clay tablets"` would match text that read "clay
/// a3f9e2b1c8d4e6f0a7b2c9d1 tablets".
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut position = 0u32;
    let mut current = String::new();
    // Byte offset where the token being built started in the original text.
    let mut start = 0usize;

    let flush = |current: &mut String,
                 position: &mut u32,
                 span: (usize, usize),
                 tokens: &mut Vec<Token>| {
        if current.is_empty() {
            return;
        }
        if is_useful(current) {
            tokens.push(Token {
                term: std::mem::take(current),
                position: *position,
                start: span.0,
                end: span.1,
            });
        } else {
            current.clear();
        }
        // Incremented either way: a dropped token still occupies its slot.
        *position += 1;
    };

    for (offset, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if current.is_empty() {
                start = offset;
            }
            // Lowercasing is per-character; a few characters (ß, İ) expand to
            // several, which `to_lowercase` handles and `to_ascii_lowercase`
            // would not.
            current.extend(ch.to_lowercase());
            // A token already past the limit cannot become useful, so stop
            // growing it rather than building a megabyte-long string from a
            // minified bundle.
            if current.len() > MAX_TERM_LEN {
                current.truncate(MAX_TERM_LEN + 1);
            }
        } else {
            flush(&mut current, &mut position, (start, offset), &mut tokens);
        }
    }
    flush(
        &mut current,
        &mut position,
        (start, text.len()),
        &mut tokens,
    );
    tokens
}

/// Tokenise a URL into searchable words.
///
/// A URL is punctuation-dense and its words are meaningful: `/blog/clay-tablets`
/// says what the page is about, and matching a query term in the URL is one of
/// the field boosts the brief asks for. The host is included because `site:`
/// filtering and host matching both want it.
pub fn tokenize_url(url: &str) -> Vec<Token> {
    // Strip the scheme so that every document does not contain "https".
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    tokenize(without_scheme)
}

#[cfg(test)]
mod tests {
    use super::{MAX_TERM_LEN, tokenize, tokenize_url};

    fn terms(text: &str) -> Vec<String> {
        tokenize(text).into_iter().map(|token| token.term).collect()
    }

    #[test]
    fn splits_on_punctuation_and_lowercases() {
        assert_eq!(terms("Clay, Tablets!"), ["clay", "tablets"]);
    }

    #[test]
    fn positions_count_from_zero_and_increase() {
        let tokens = tokenize("one two three");
        assert_eq!(
            tokens.iter().map(|t| t.position).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn stopwords_are_kept() {
        // Dropping these would make `"to be or not to be"` unsearchable.
        assert_eq!(
            terms("to be or not to be"),
            ["to", "be", "or", "not", "to", "be"]
        );
    }

    #[test]
    fn words_are_not_stemmed() {
        // An exact search for "running" has to remain answerable.
        assert_eq!(terms("running runs ran"), ["running", "runs", "ran"]);
    }

    #[test]
    fn numbers_are_indexed() {
        assert_eq!(
            terms("the year 1998 and page 42"),
            ["the", "year", "1998", "and", "page", "42"]
        );
    }

    #[test]
    fn long_digit_strings_are_dropped() {
        let terms = terms("order 123456789012345678901234567890 shipped");
        assert_eq!(terms, ["order", "shipped"]);
    }

    #[test]
    fn junk_tokens_are_dropped() {
        let junk = "a".repeat(MAX_TERM_LEN + 5);
        assert_eq!(terms(&format!("real {junk} words")), ["real", "words"]);
    }

    #[test]
    fn a_dropped_token_still_occupies_its_position() {
        // Otherwise a phrase search would match across the gap left by junk.
        let junk = "a".repeat(MAX_TERM_LEN + 5);
        let tokens = tokenize(&format!("clay {junk} tablets"));
        assert_eq!(tokens[0].term, "clay");
        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[1].term, "tablets");
        assert_eq!(
            tokens[1].position, 2,
            "the junk token should have consumed position 1"
        );
    }

    #[test]
    fn byte_offsets_point_back_at_the_original_text() {
        // Snippets cut the original, so the offsets have to be exact.
        let text = "The scribes of Uruk, pressing reed.";
        for token in tokenize(text) {
            assert_eq!(
                text[token.start..token.end].to_lowercase(),
                token.term,
                "offsets for {:?} do not match",
                token.term
            );
        }
    }

    #[test]
    fn offsets_are_correct_with_multibyte_characters() {
        let text = "café au lait — très bon";
        for token in tokenize(text) {
            // Slicing must land on character boundaries or this panics.
            assert!(!text[token.start..token.end].is_empty());
        }
    }

    #[test]
    fn non_latin_scripts_are_tokenised() {
        assert_eq!(terms("Москва и Урук"), ["москва", "и", "урук"]);
        assert_eq!(terms("東京 と 大阪"), ["東京", "と", "大阪"]);
    }

    #[test]
    fn multi_character_lowercasing_is_handled() {
        // ß lowercases to itself but uppercase ẞ maps to ß; İ expands.
        assert_eq!(terms("STRASSE Straße"), ["strasse", "straße"]);
    }

    #[test]
    fn empty_and_punctuation_only_text_yields_nothing() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   --- ,,, ").is_empty());
    }

    #[test]
    fn urls_are_split_into_words_without_the_scheme() {
        let terms: Vec<String> = tokenize_url("https://a.test/blog/clay-tablets")
            .into_iter()
            .map(|t| t.term)
            .collect();
        assert_eq!(terms, ["a", "test", "blog", "clay", "tablets"]);
        assert!(
            !terms.contains(&"https".to_owned()),
            "every page would match 'https'"
        );
    }

    #[test]
    fn a_hyphenated_word_becomes_two_terms() {
        // Deliberate: searching either half should find it, and a phrase
        // search for the whole thing still works because they are adjacent.
        let tokens = tokenize("clay-tablets");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[1].position, 1);
    }

    #[test]
    fn very_long_input_does_not_build_a_giant_token() {
        // A minified bundle is one enormous "word"; we must not allocate it.
        let bundle = "x".repeat(1_000_000);
        assert!(tokenize(&bundle).is_empty());
    }
}
