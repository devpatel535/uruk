//! The judged query set: a fixed list of queries with hand-marked answers.
//!
//! `RESEARCH.md` §5.4 is blunt about why this exists. Refusing click-through
//! data, dwell time and personalisation removes the strongest relevance signal
//! anyone has. What is left is a curated corpus and **a way to measure a
//! ranking change instead of arguing about it**. Without that, every hour of
//! tuning is an hour of guessing, and the guesses are unfalsifiable.
//!
//! # The file format
//!
//! Plain text, because a human writes and reviews this file and a diff of it
//! should be readable:
//!
//! ```text
//! # Lines beginning with # are comments.
//!
//! query: clay tablets
//!   3  https://example.org/cuneiform-archive
//!   2  https://example.com/mesopotamia
//!   0  https://spam.example/clay-tablets-cheap
//!
//! query: "barley rations"
//!   3  https://example.org/temple-accounts
//! ```
//!
//! A grade is 0 to 3. The query line is passed to the ordinary query parser,
//! so quoted phrases, `-exclusion` and `site:` all work and can be judged.
//!
//! # Grades, and what they have to mean
//!
//! A grade is worthless unless two people would give the same one. These are
//! deliberately coarse:
//!
//! | Grade | Meaning |
//! |---|---|
//! | 3 | What the searcher was looking for. They would stop here. |
//! | 2 | Genuinely useful and on-topic, but not the thing itself. |
//! | 1 | Related, and better than nothing. A reasonable tenth result. |
//! | 0 | Not relevant, or actively bad: spam, a stub, an unrelated page. |
//!
//! Anything judged 2 or better counts as "relevant" for the metrics that need
//! a yes-or-no answer.
//!
//! `examples/judgments.example.txt` is a template with the format, worked
//! examples, and the process for writing a real set — including the parts that
//! are easy to get wrong, like judging pages that did *not* come back so that
//! recall is measurable at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The most relevant a document can be.
pub const MAX_GRADE: u8 = 3;

/// The lowest grade that counts as relevant for binary metrics.
pub const RELEVANT_AT: u8 = 2;

#[derive(Debug, thiserror::Error)]
pub enum JudgmentError {
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}:{line}: {reason}")]
    Malformed {
        path: PathBuf,
        line: usize,
        reason: String,
    },
}

/// One query and everything judged for it.
#[derive(Debug, Clone)]
pub struct JudgedQuery {
    /// The query as typed, passed verbatim to the query parser.
    pub query: String,
    /// Line in the source file, so an error can point at it.
    pub line: usize,
    /// URL to grade. A URL absent from here is unjudged, which is not the same
    /// as a zero — see [`crate::metrics`].
    pub grades: BTreeMap<String, u8>,
}

impl JudgedQuery {
    /// Grades sorted best first: the ranking a perfect engine would return.
    pub fn ideal(&self) -> Vec<u8> {
        let mut grades: Vec<u8> = self.grades.values().copied().collect();
        grades.sort_unstable_by(|a, b| b.cmp(a));
        grades
    }

    /// Documents judged relevant, by the [`RELEVANT_AT`] threshold.
    pub fn relevant(&self) -> usize {
        self.grades
            .values()
            .filter(|&&grade| grade >= RELEVANT_AT)
            .count()
    }
}

/// A whole judged query set.
#[derive(Debug, Clone, Default)]
pub struct Judgments {
    pub queries: Vec<JudgedQuery>,
}

impl Judgments {
    pub fn len(&self) -> usize {
        self.queries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }

    /// Total graded documents across every query.
    pub fn graded(&self) -> usize {
        self.queries.iter().map(|query| query.grades.len()).sum()
    }

    pub fn load(path: &Path) -> Result<Self, JudgmentError> {
        let raw = std::fs::read_to_string(path).map_err(|source| JudgmentError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&raw, path)
    }

    /// Parse the text format. `path` is only used to make errors locatable.
    pub fn parse(raw: &str, path: &Path) -> Result<Self, JudgmentError> {
        let fail = |line: usize, reason: String| JudgmentError::Malformed {
            path: path.to_path_buf(),
            line,
            reason,
        };

        let mut queries: Vec<JudgedQuery> = Vec::new();
        for (index, text) in raw.lines().enumerate() {
            let line = index + 1;
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if let Some(query) = trimmed.strip_prefix("query:") {
                let query = query.trim();
                if query.is_empty() {
                    return Err(fail(line, "a query line with no query".to_owned()));
                }
                if let Some(earlier) = queries.iter().find(|seen| seen.query == query) {
                    // Two blocks for one query would silently merge, and the
                    // second block's author would never know the first existed.
                    return Err(fail(
                        line,
                        format!("{query:?} is already judged at line {}", earlier.line),
                    ));
                }
                queries.push(JudgedQuery {
                    query: query.to_owned(),
                    line,
                    grades: BTreeMap::new(),
                });
                continue;
            }

            let Some(current) = queries.last_mut() else {
                return Err(fail(line, "a judgment before any `query:` line".to_owned()));
            };

            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let grade = parts.next().unwrap_or_default();
            let url = parts.next().unwrap_or_default().trim();
            let grade: u8 = grade
                .parse()
                .map_err(|_| fail(line, format!("{grade:?} is not a grade")))?;
            if grade > MAX_GRADE {
                return Err(fail(
                    line,
                    format!("grade {grade} is above the maximum of {MAX_GRADE}"),
                ));
            }
            if url.is_empty() {
                return Err(fail(line, "a grade with no URL".to_owned()));
            }
            if current.grades.insert(url.to_owned(), grade).is_some() {
                return Err(fail(line, format!("{url} is judged twice for this query")));
            }
        }

        Ok(Self { queries })
    }
}

#[cfg(test)]
mod tests {
    use super::{Judgments, MAX_GRADE};
    use std::path::Path;

    fn parse(raw: &str) -> Judgments {
        Judgments::parse(raw, Path::new("test.txt")).expect("parses")
    }

    #[test]
    fn a_judged_set_round_trips_through_the_format() {
        let set = parse(
            "# a comment\n\
             \n\
             query: clay tablets\n\
             \x20 3  https://a.test/tablets\n\
             \x20 0  https://spam.test/clay\n\
             \n\
             query: \"barley rations\"\n\
             \x20 2  https://a.test/accounts\n",
        );
        assert_eq!(set.len(), 2);
        assert_eq!(set.graded(), 3);
        assert_eq!(set.queries[0].query, "clay tablets");
        assert_eq!(set.queries[0].grades["https://a.test/tablets"], 3);
        assert_eq!(set.queries[1].query, "\"barley rations\"");
    }

    #[test]
    fn the_ideal_ranking_is_the_grades_sorted_best_first() {
        let set =
            parse("query: q\n  1 https://a.test/1\n  3 https://a.test/3\n  2 https://a.test/2\n");
        assert_eq!(set.queries[0].ideal(), vec![3, 2, 1]);
        assert_eq!(set.queries[0].relevant(), 2);
    }

    #[test]
    fn a_duplicate_query_block_is_an_error_not_a_merge() {
        // Silently merging would hide one author's judgments under another's.
        let error = Judgments::parse(
            "query: q\n  3 https://a.test/1\nquery: q\n  0 https://a.test/2\n",
            Path::new("test.txt"),
        );
        assert!(error.is_err(), "a duplicate query block was accepted");
    }

    #[test]
    fn a_judgment_before_any_query_is_an_error() {
        assert!(Judgments::parse("  3 https://a.test/x\n", Path::new("t")).is_err());
    }

    #[test]
    fn a_grade_out_of_range_is_refused() {
        let error = Judgments::parse(
            &format!("query: q\n  {} https://a.test/x\n", MAX_GRADE + 1),
            Path::new("t"),
        );
        assert!(error.is_err());
    }

    #[test]
    fn a_url_judged_twice_for_one_query_is_an_error() {
        assert!(
            Judgments::parse(
                "query: q\n  3 https://a.test/x\n  1 https://a.test/x\n",
                Path::new("t")
            )
            .is_err()
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored_everywhere() {
        let set = parse("\n# top\nquery: q\n\n# mid\n  3 https://a.test/x\n\n# end\n");
        assert_eq!(set.len(), 1);
        assert_eq!(set.graded(), 1);
    }
}
