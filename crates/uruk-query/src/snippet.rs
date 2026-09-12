//! Snippets: the piece of the page that shows why it matched.
//!
//! A snippet is cut from the **original** text, punctuation and capitals
//! intact, rather than reassembled from index terms — the point is to show the
//! author's sentence, not our tokenisation of it.
//!
//! Choosing where to cut is a small optimisation problem: find the window of
//! roughly the target length containing the most distinct query terms, and
//! among equally good windows prefer the earliest, since the opening of an
//! article is usually the most orienting.
//!
//! # Directives are obeyed here
//!
//! A page that sent `nosnippet` gets no snippet, and one that sent
//! `max-snippet:N` gets at most N characters. `RESEARCH.md` §5.9 argues these
//! have to be honoured from the first crawl rather than retrofitted: they are
//! how a site says "index me but do not quote me", and the crawler already
//! recorded them, so the only way to get this wrong is to ignore what we
//! stored.

use uruk_index::tokenize;

/// Characters a snippet aims for. Two lines in a terminal, and short enough
/// that a results page stays inside the brief's 20 KB budget.
pub const DEFAULT_LENGTH: usize = 240;

/// Shortest snippet worth cutting to. Below this a "snippet" is a fragment.
const MIN_LENGTH: usize = 40;

/// An extract from a page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snippet {
    /// The extract, with ellipses where text was cut.
    pub text: String,
    /// Byte ranges within `text` that matched a query term, for highlighting.
    /// Ranges are into `text`, not into the document.
    pub highlights: Vec<(usize, usize)>,
}

impl Snippet {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// What the page permits us to quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnippetPolicy {
    /// False when the page sent `nosnippet`.
    pub allowed: bool,
    /// Character cap from `max-snippet:N`, if any.
    pub max_chars: Option<usize>,
}

impl Default for SnippetPolicy {
    fn default() -> Self {
        Self {
            allowed: true,
            max_chars: None,
        }
    }
}

impl SnippetPolicy {
    /// The length to aim for, given a preference and what the page allows.
    fn budget(self, preferred: usize) -> Option<usize> {
        if !self.allowed {
            return None;
        }
        match self.max_chars {
            Some(0) => None,
            Some(cap) => Some(preferred.min(cap)),
            None => Some(preferred),
        }
    }
}

/// Build a snippet of `text` around `terms`.
///
/// Returns an empty snippet when the page forbids one.
pub fn snippet(
    text: &str,
    terms: &[&str],
    policy: SnippetPolicy,
    preferred_length: usize,
) -> Snippet {
    let Some(budget) = policy.budget(preferred_length) else {
        return Snippet::default();
    };
    if text.trim().is_empty() {
        return Snippet::default();
    }

    let tokens = tokenize::tokenize(text);
    let matches: Vec<&tokenize::Token> = tokens
        .iter()
        .filter(|token| terms.iter().any(|term| *term == token.term))
        .collect();

    // Nothing matched — it can have matched in the title or URL instead — so
    // fall back to the opening of the article, which is what a reader would
    // skim anyway.
    let (start, end) = if matches.is_empty() {
        (0, budget.min(text.len()))
    } else {
        best_window(&matches, text.len(), budget, terms.len())
    };

    let (start, end) = widen_to_boundaries(text, start, end);
    let mut out = String::with_capacity(end - start + 8);
    if start > 0 {
        out.push('…');
    }
    let body_start = out.len();
    out.push_str(text[start..end].trim());
    if end < text.len() {
        out.push('…');
    }

    // Highlight ranges, recomputed against the extract so callers never have
    // to know where in the document it came from.
    let trimmed_offset = text[start..end].len() - text[start..end].trim_start().len();
    let highlights = matches
        .iter()
        .filter(|token| token.start >= start && token.end <= end)
        .filter_map(|token| {
            let from = body_start + (token.start - start).checked_sub(trimmed_offset)?;
            let to = body_start + (token.end - start).checked_sub(trimmed_offset)?;
            (to <= out.len()).then_some((from, to))
        })
        .collect();

    Snippet {
        text: out,
        highlights,
    }
}

/// Find the byte window of about `budget` holding the most distinct terms.
///
/// Slides the window over matched tokens rather than over characters, so the
/// cost is proportional to the number of matches and not to the length of the
/// document.
fn best_window(
    matches: &[&tokenize::Token],
    text_len: usize,
    budget: usize,
    distinct_terms: usize,
) -> (usize, usize) {
    let mut best = (0usize, 0usize, 0usize); // (distinct, span start, span end)

    for (index, anchor) in matches.iter().enumerate() {
        // A window that starts a little before the match reads better than one
        // that starts exactly on it.
        let lead = budget / 4;
        let start = anchor.start.saturating_sub(lead);
        let end = (start + budget).min(text_len);

        let mut seen: Vec<&str> = Vec::new();
        for token in &matches[index..] {
            if token.end > end {
                break;
            }
            if !seen.contains(&token.term.as_str()) {
                seen.push(token.term.as_str());
            }
        }
        // Also count matches before the anchor that still fall in the window.
        for token in matches[..index].iter().rev() {
            if token.start < start {
                break;
            }
            if !seen.contains(&token.term.as_str()) {
                seen.push(token.term.as_str());
            }
        }

        if seen.len() > best.0 {
            best = (seen.len(), start, end);
            // Every query term is in view; no later window can beat this, and
            // earlier is better, so stop.
            if seen.len() >= distinct_terms {
                break;
            }
        }
    }

    if best.0 == 0 {
        let anchor = matches[0];
        let start = anchor.start.saturating_sub(budget / 4);
        return (start, (start + budget).min(text_len));
    }
    (best.1, best.2)
}

/// Move the cut points out to character boundaries, and to whitespace where
/// one is close, so a snippet does not start or end mid-word.
fn widen_to_boundaries(text: &str, mut start: usize, mut end: usize) -> (usize, usize) {
    start = start.min(text.len());
    end = end.clamp(start, text.len());

    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }

    // Pull the start forward to just after the nearest space, within reason.
    if start > 0 {
        let search_from = start.saturating_sub(MIN_LENGTH);
        if let Some(space) = text[search_from..start].rfind(char::is_whitespace) {
            start = search_from + space + 1;
        }
    }
    // Push the end out to the next space, so the last word is whole.
    if end < text.len() {
        let limit = (end + MIN_LENGTH).min(text.len());
        if let Some(space) = text[end..limit].find(char::is_whitespace) {
            end += space;
        }
    }
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    (start, end.max(start))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LENGTH, Snippet, SnippetPolicy, snippet};

    const ARTICLE: &str = "The scribes of Uruk were not writing poetry. They were counting \
sheep, and they needed the count to survive the walk from the pen to the temple storehouse. \
What they invented, without meaning to, was a way of making a promise outlive the person who \
made it. A clay tablet recorded that a quantity of barley had changed hands, in a form that \
could be checked later by someone who had not been present at the transaction. That is the \
whole idea, and every ledger written since is a footnote to it.";

    fn cut(terms: &[&str]) -> Snippet {
        snippet(ARTICLE, terms, SnippetPolicy::default(), DEFAULT_LENGTH)
    }

    #[test]
    fn the_snippet_contains_the_matched_term() {
        let found = cut(&["barley"]);
        assert!(found.text.contains("barley"), "got: {}", found.text);
    }

    #[test]
    fn the_snippet_is_cut_from_the_original_text_not_the_terms() {
        // Capitals and punctuation survive: we show the author's sentence.
        let found = cut(&["uruk"]);
        assert!(found.text.contains("Uruk"), "got: {}", found.text);
    }

    #[test]
    fn the_snippet_respects_the_length_budget() {
        let found = cut(&["barley"]);
        // Allowing for ellipses and the widening to word boundaries.
        assert!(
            found.text.chars().count() <= DEFAULT_LENGTH + 80,
            "length {}",
            found.text.len()
        );
        assert!(found.text.len() >= 40);
    }

    #[test]
    fn a_window_covering_several_terms_is_preferred() {
        // "clay" and "barley" are close together; "scribes" is far away.
        let found = cut(&["clay", "barley"]);
        assert!(found.text.contains("clay"), "got: {}", found.text);
        assert!(found.text.contains("barley"), "got: {}", found.text);
    }

    #[test]
    fn a_cut_snippet_is_marked_with_ellipses() {
        let found = cut(&["barley"]);
        assert!(found.text.starts_with('…'), "got: {}", found.text);
    }

    #[test]
    fn a_match_at_the_start_needs_no_leading_ellipsis() {
        let found = cut(&["scribes"]);
        assert!(!found.text.starts_with('…'), "got: {}", found.text);
        assert!(found.text.starts_with("The scribes"));
    }

    #[test]
    fn highlights_point_at_the_matched_words_in_the_snippet() {
        let found = cut(&["barley"]);
        assert!(!found.highlights.is_empty());
        for &(from, to) in &found.highlights {
            assert_eq!(&found.text[from..to].to_lowercase(), "barley");
        }
    }

    #[test]
    fn a_query_that_matched_elsewhere_falls_back_to_the_opening() {
        // The term matched in the title or URL, not the body.
        let found = cut(&["nowhere"]);
        assert!(found.text.starts_with("The scribes"), "got: {}", found.text);
        assert!(found.highlights.is_empty());
    }

    #[test]
    fn nosnippet_produces_nothing() {
        let policy = SnippetPolicy {
            allowed: false,
            max_chars: None,
        };
        assert!(snippet(ARTICLE, &["barley"], policy, DEFAULT_LENGTH).is_empty());
    }

    #[test]
    fn max_snippet_zero_produces_nothing() {
        let policy = SnippetPolicy {
            allowed: true,
            max_chars: Some(0),
        };
        assert!(snippet(ARTICLE, &["barley"], policy, DEFAULT_LENGTH).is_empty());
    }

    #[test]
    fn max_snippet_caps_the_length() {
        let policy = SnippetPolicy {
            allowed: true,
            max_chars: Some(60),
        };
        let found = snippet(ARTICLE, &["barley"], policy, DEFAULT_LENGTH);
        assert!(!found.is_empty());
        // The cap plus ellipses and word-boundary widening.
        assert!(
            found.text.chars().count() < 160,
            "length {}",
            found.text.chars().count()
        );
    }

    #[test]
    fn empty_text_produces_nothing() {
        assert!(snippet("", &["clay"], SnippetPolicy::default(), DEFAULT_LENGTH).is_empty());
        assert!(snippet("   ", &["clay"], SnippetPolicy::default(), DEFAULT_LENGTH).is_empty());
    }

    #[test]
    fn no_terms_still_gives_the_opening() {
        let found = snippet(ARTICLE, &[], SnippetPolicy::default(), DEFAULT_LENGTH);
        assert!(found.text.starts_with("The scribes"));
    }

    #[test]
    fn multibyte_text_is_never_cut_mid_character() {
        // Slicing off a character boundary panics, so this is the test that
        // matters most for arbitrary crawled text.
        let text = "café au lait — très bon. ".repeat(40);
        for terms in [vec!["café"], vec!["très"], vec!["bon"], vec![]] {
            let found = snippet(&text, &terms, SnippetPolicy::default(), 100);
            assert!(!found.text.is_empty());
        }
    }

    #[test]
    fn a_document_shorter_than_the_budget_is_shown_whole() {
        let short = "Clay tablets from Uruk.";
        let found = snippet(short, &["clay"], SnippetPolicy::default(), DEFAULT_LENGTH);
        assert_eq!(found.text, short);
        assert!(!found.text.contains('…'));
    }
}
