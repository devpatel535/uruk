//! Query syntax.
//!
//! The old operators, which the brief asks to actually work:
//!
//! | Written | Means |
//! |---|---|
//! | `clay tablets` | both words must appear — AND is the default, not OR |
//! | `"clay tablets"` | those words, adjacent, in that order |
//! | `-barley` | documents containing this are excluded |
//! | `-"clay tablets"` | documents containing this phrase are excluded |
//! | `site:a.test` | only pages from that host |
//!
//! Two details that are easy to get wrong and matter.
//!
//! **Query words go through the same tokeniser as the index.** If they did
//! not, a query would be looking for terms that were never stored. So
//! `Clay,` becomes `clay`, exactly as it did at index time.
//!
//! **A word that tokenises into several terms becomes a phrase.** `clay-tablets`
//! is two terms in the index, adjacent; searching for it should find that
//! adjacency rather than the two words scattered across a page.

use uruk_index::tokenize;

/// A sequence of terms that must appear adjacently and in order.
pub type Phrase = Vec<String>;

/// A parsed query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Every term that must appear somewhere. Phrase terms are included here
    /// too, because narrowing to documents containing all of them is how the
    /// candidate set is found before positions are checked.
    pub required: Vec<String>,
    /// Phrases whose terms must additionally be adjacent.
    pub phrases: Vec<Phrase>,
    /// Terms that must not appear.
    pub excluded: Vec<String>,
    /// Phrases that must not appear.
    pub excluded_phrases: Vec<Phrase>,
    /// `site:` filter, lowercased.
    pub site: Option<String>,
}

impl Query {
    /// Is there anything to search for?
    ///
    /// A query of only exclusions or only a `site:` filter matches everything,
    /// which is not a search — it is a request to dump the index.
    pub fn is_empty(&self) -> bool {
        self.required.is_empty()
    }

    /// Terms in the order a score breakdown should list them, without repeats.
    pub fn distinct_terms(&self) -> Vec<&str> {
        let mut seen = Vec::new();
        for term in &self.required {
            if !seen.contains(&term.as_str()) {
                seen.push(term.as_str());
            }
        }
        seen
    }
}

/// One chunk of raw query text, before it is tokenised.
#[derive(Debug, PartialEq, Eq)]
struct Chunk {
    text: String,
    negated: bool,
    quoted: bool,
}

/// Split the raw query into chunks, respecting quotes and leading `-`.
fn chunks(raw: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut negated = false;
    let mut in_quotes = false;
    let mut quoted = false;
    // True while we are at the start of a chunk, where `-` means "exclude"
    // rather than being a hyphen inside a word.
    let mut at_start = true;

    let finish =
        |current: &mut String, negated: &mut bool, quoted: &mut bool, out: &mut Vec<Chunk>| {
            if !current.is_empty() {
                out.push(Chunk {
                    text: std::mem::take(current),
                    negated: *negated,
                    quoted: *quoted,
                });
            }
            current.clear();
            *negated = false;
            *quoted = false;
        };

    for ch in raw.chars() {
        match ch {
            '"' => {
                if in_quotes {
                    // Closing quote ends the chunk even if it is empty, so
                    // `""` does not swallow what follows.
                    quoted = true;
                    finish(&mut current, &mut negated, &mut quoted, &mut out);
                    in_quotes = false;
                    at_start = true;
                } else {
                    in_quotes = true;
                    quoted = true;
                    at_start = false;
                }
            }
            '-' if at_start && !in_quotes => {
                negated = true;
                at_start = false;
            }
            c if c.is_whitespace() && !in_quotes => {
                finish(&mut current, &mut negated, &mut quoted, &mut out);
                at_start = true;
            }
            c => {
                current.push(c);
                at_start = false;
            }
        }
    }
    finish(&mut current, &mut negated, &mut quoted, &mut out);
    out
}

/// Parse a query string.
///
/// Never fails: anything unrecognisable becomes ordinary search terms, because
/// a search box that rejects input is worse than one that searches for what
/// was typed.
pub fn parse(raw: &str) -> Query {
    let mut query = Query::default();

    for chunk in chunks(raw) {
        // `site:` is only a filter when it is not inside quotes — someone
        // searching for the literal phrase should get the literal phrase.
        if !chunk.quoted
            && !chunk.negated
            && let Some(host) = chunk.text.strip_prefix("site:")
        {
            // Lowercase first: "WWW." would otherwise survive the strip.
            let host = host.trim().to_ascii_lowercase();
            let host = host.trim_start_matches("www.");
            if !host.is_empty() {
                query.site = Some(host.to_owned());
            }
            continue;
        }

        let terms: Vec<String> = tokenize::tokenize(&chunk.text)
            .into_iter()
            .map(|token| token.term)
            .collect();
        if terms.is_empty() {
            continue;
        }

        // A quoted chunk is always a phrase. An unquoted one becomes a phrase
        // only when tokenising split it, since those terms were adjacent in
        // the input and should be adjacent in the document.
        let is_phrase = terms.len() > 1;

        if chunk.negated {
            if is_phrase {
                query.excluded_phrases.push(terms);
            } else {
                query.excluded.extend(terms);
            }
        } else {
            if is_phrase {
                query.phrases.push(terms.clone());
            }
            query.required.extend(terms);
        }
    }

    query
}

#[cfg(test)]
mod tests {
    use super::{Query, parse};

    fn required(raw: &str) -> Vec<String> {
        parse(raw).required
    }

    #[test]
    fn bare_words_are_all_required() {
        // AND by default, which is the whole point.
        assert_eq!(required("clay tablets"), ["clay", "tablets"]);
    }

    #[test]
    fn words_are_tokenised_the_same_way_the_index_was() {
        // Otherwise a query looks for terms that were never stored.
        assert_eq!(required("Clay, TABLETS!"), ["clay", "tablets"]);
    }

    #[test]
    fn extra_whitespace_is_harmless() {
        assert_eq!(required("   clay    tablets  "), ["clay", "tablets"]);
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
    }

    #[test]
    fn a_quoted_group_becomes_a_phrase_and_is_still_required() {
        let query = parse("\"clay tablets\"");
        assert_eq!(
            query.phrases,
            [vec!["clay".to_owned(), "tablets".to_owned()]]
        );
        // Also required, because that is how candidates are narrowed before
        // positions are checked.
        assert_eq!(query.required, ["clay", "tablets"]);
    }

    #[test]
    fn a_hyphenated_word_becomes_a_phrase() {
        // It is two adjacent terms in the index; find that adjacency.
        let query = parse("clay-tablets");
        assert_eq!(
            query.phrases,
            [vec!["clay".to_owned(), "tablets".to_owned()]]
        );
        assert!(
            query.excluded.is_empty(),
            "the hyphen is not an exclusion here"
        );
    }

    #[test]
    fn a_leading_minus_excludes() {
        let query = parse("clay -barley");
        assert_eq!(query.required, ["clay"]);
        assert_eq!(query.excluded, ["barley"]);
    }

    #[test]
    fn a_minus_inside_a_word_is_not_an_exclusion() {
        let query = parse("well-known");
        assert!(query.excluded.is_empty());
        assert_eq!(query.required, ["well", "known"]);
    }

    #[test]
    fn a_negated_phrase_is_excluded_as_a_phrase() {
        let query = parse("uruk -\"clay tablets\"");
        assert_eq!(query.required, ["uruk"]);
        assert_eq!(
            query.excluded_phrases,
            [vec!["clay".to_owned(), "tablets".to_owned()]]
        );
        assert!(query.excluded.is_empty());
    }

    #[test]
    fn site_filters_by_host() {
        let query = parse("clay site:a.test");
        assert_eq!(query.site.as_deref(), Some("a.test"));
        assert_eq!(
            query.required,
            ["clay"],
            "the filter is not also a search term"
        );
    }

    #[test]
    fn site_ignores_case_and_a_www_prefix() {
        assert_eq!(parse("site:WWW.A.TEST").site.as_deref(), Some("a.test"));
    }

    #[test]
    fn a_quoted_site_is_searched_for_literally() {
        // Someone looking for the text "site:a.test" should find it.
        let query = parse("\"site:a.test\"");
        assert!(query.site.is_none());
        assert!(!query.required.is_empty());
    }

    #[test]
    fn everything_at_once() {
        let query = parse("clay \"reed stylus\" -barley site:a.test");
        assert_eq!(query.required, ["clay", "reed", "stylus"]);
        assert_eq!(
            query.phrases,
            [vec!["reed".to_owned(), "stylus".to_owned()]]
        );
        assert_eq!(query.excluded, ["barley"]);
        assert_eq!(query.site.as_deref(), Some("a.test"));
    }

    #[test]
    fn a_query_of_only_exclusions_is_empty() {
        // It would otherwise match the entire index, which is not a search.
        assert!(parse("-barley").is_empty());
        assert!(parse("site:a.test").is_empty());
    }

    #[test]
    fn an_unclosed_quote_still_parses() {
        // A search box that rejects input is worse than one that searches for
        // what was typed.
        let query = parse("\"clay tablets");
        assert_eq!(query.required, ["clay", "tablets"]);
    }

    #[test]
    fn an_empty_quoted_group_does_not_swallow_what_follows() {
        assert_eq!(parse("\"\" clay").required, ["clay"]);
    }

    #[test]
    fn repeated_terms_are_listed_once_for_a_breakdown() {
        let query = parse("clay clay tablets");
        assert_eq!(query.required, ["clay", "clay", "tablets"]);
        assert_eq!(query.distinct_terms(), ["clay", "tablets"]);
    }

    #[test]
    fn stopwords_survive_into_the_query() {
        // They are in the index, so they must be searchable.
        assert_eq!(parse("\"to be or not to be\"").phrases[0].len(), 6);
    }

    #[test]
    fn a_default_query_matches_nothing() {
        assert!(Query::default().is_empty());
    }
}
