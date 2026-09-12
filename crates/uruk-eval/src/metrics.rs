//! Ranking metrics, and the one thing they will lie to you about.
//!
//! # nDCG, in plain words
//!
//! **DCG** — discounted cumulative gain — adds up how good the results are,
//! discounting each by how far down the page it is. A brilliant result at
//! position one is worth more than the same result at position nine, because
//! fewer people will ever see position nine. Gain is `2^grade - 1`, so the gap
//! between "what they wanted" and "useful but not it" is larger than the gap
//! between "useful" and "related"; the discount is `1 / log2(rank + 1)`.
//!
//! **nDCG** is that number divided by the best it could possibly have been,
//! given the documents that were judged. So 1.0 means "no ranking of the
//! judged documents would have been better" and 0.0 means "nothing relevant
//! came back". Dividing by the ideal is what makes two queries comparable: a
//! query with one good answer and a query with twenty otherwise score on
//! different scales.
//!
//! # The lie: unjudged documents
//!
//! A result that is not in the judged set is not "bad" — nobody looked at it.
//! But a metric has to do *something* with it, and there are only two honest
//! options:
//!
//! - **Treat it as grade 0.** Simple, standard for a closed corpus, and what
//!   [`Scored`] does. It systematically punishes any ranking that surfaces
//!   documents the judge never saw — which is exactly what a *better* ranker
//!   does on a corpus that has grown since the judgments were written.
//! - **Condense**: drop unjudged results and score what is left. This removes
//!   the bias against new documents and introduces a different one, since a
//!   ranker that returns nine unjudged results and one good one looks perfect.
//!
//! There is no third option that is not a guess. So this module does the first
//! and **reports the unjudged rate on every run** ([`Scored::coverage`]).
//! A comparison over a query set with low coverage is not evidence, and the
//! only way anybody will know that is if the number is printed next to the
//! score.

use crate::judgments::{JudgedQuery, RELEVANT_AT};

/// Results to score. The product is ten links, so ten is the depth that
/// matters; anything below it is a page most people never see.
pub const DEFAULT_DEPTH: usize = 10;

/// What one query scored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scored {
    /// Normalised discounted cumulative gain at the evaluation depth.
    pub ndcg: f64,
    /// Fraction of the top results that were judged relevant.
    pub precision: f64,
    /// Reciprocal of the rank of the first relevant result; 0 if none.
    pub reciprocal_rank: f64,
    /// Judged relevant documents that were actually returned, over the number
    /// that exist. Low recall with high precision means the engine is finding
    /// good pages but missing most of them.
    pub recall: f64,
    /// Fraction of returned results that had a judgment at all.
    ///
    /// **Read this before believing anything above it.** At 0.2, four results
    /// in five were scored as irrelevant because nobody looked at them.
    pub coverage: f64,
    /// Results actually returned, which may be fewer than the depth.
    pub returned: usize,
}

/// Gain for a grade: `2^grade - 1`.
///
/// Exponential rather than linear so that one excellent result outweighs
/// several mediocre ones, which is how a person actually experiences a results
/// page — they are looking for the answer, not for volume.
fn gain(grade: u8) -> f64 {
    f64::from((1u32 << grade) - 1)
}

/// Positional discount: `1 / log2(rank + 1)`, with `rank` one-based.
fn discount(rank: usize) -> f64 {
    1.0 / ((rank + 1) as f64).log2()
}

/// Score one query's ranking against its judgments.
///
/// `ranked` is the URLs the engine returned, best first.
pub fn score(query: &JudgedQuery, ranked: &[String], depth: usize) -> Scored {
    let top = &ranked[..ranked.len().min(depth)];

    let mut dcg = 0.0;
    let mut judged = 0usize;
    let mut relevant_found = 0usize;
    let mut reciprocal_rank = 0.0;

    for (index, url) in top.iter().enumerate() {
        let rank = index + 1;
        let grade = match query.grades.get(url) {
            Some(&grade) => {
                judged += 1;
                grade
            }
            // Unjudged counts as zero, and `coverage` below is how anyone
            // reading the result finds out how often that happened.
            None => 0,
        };
        dcg += gain(grade) * discount(rank);
        if grade >= RELEVANT_AT {
            relevant_found += 1;
            if reciprocal_rank == 0.0 {
                reciprocal_rank = 1.0 / rank as f64;
            }
        }
    }

    let ideal: f64 = query
        .ideal()
        .iter()
        .take(depth)
        .enumerate()
        .map(|(index, &grade)| gain(grade) * discount(index + 1))
        .sum();

    // An ideal of zero means every judgment for this query is a zero: the
    // judge decided nothing relevant exists. Any ranking is then equally
    // correct, and 1.0 says so without dividing by zero.
    let ndcg = if ideal > 0.0 { dcg / ideal } else { 1.0 };

    let denominator = top.len().max(1) as f64;
    let total_relevant = query.relevant();

    Scored {
        ndcg: ndcg.clamp(0.0, 1.0),
        precision: relevant_found as f64 / denominator,
        reciprocal_rank,
        recall: if total_relevant == 0 {
            1.0
        } else {
            relevant_found as f64 / total_relevant as f64
        },
        coverage: judged as f64 / denominator,
        returned: ranked.len(),
    }
}

/// The mean of each metric across a query set.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    pub queries: usize,
    pub ndcg: f64,
    pub precision: f64,
    pub mean_reciprocal_rank: f64,
    pub recall: f64,
    pub coverage: f64,
    /// Queries that returned nothing at all. A high count here means the
    /// corpus, not the ranking, is what needs work.
    pub empty: usize,
}

impl Summary {
    pub fn of(scores: &[Scored]) -> Self {
        if scores.is_empty() {
            return Self::default();
        }
        let n = scores.len() as f64;
        let mean = |pick: fn(&Scored) -> f64| scores.iter().map(pick).sum::<f64>() / n;
        Self {
            queries: scores.len(),
            ndcg: mean(|s| s.ndcg),
            precision: mean(|s| s.precision),
            mean_reciprocal_rank: mean(|s| s.reciprocal_rank),
            recall: mean(|s| s.recall),
            coverage: mean(|s| s.coverage),
            empty: scores.iter().filter(|s| s.returned == 0).count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_DEPTH, Summary, score};
    use crate::judgments::Judgments;
    use std::path::Path;

    fn query(raw: &str) -> crate::judgments::JudgedQuery {
        Judgments::parse(raw, Path::new("t"))
            .expect("parses")
            .queries
            .remove(0)
    }

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(|&s| s.to_string()).collect()
    }

    #[test]
    fn a_perfect_ranking_scores_one() {
        let judged = query("query: q\n 3 https://a/1\n 2 https://a/2\n 1 https://a/3\n");
        let scored = score(
            &judged,
            &urls(&["https://a/1", "https://a/2", "https://a/3"]),
            DEFAULT_DEPTH,
        );
        assert!((scored.ndcg - 1.0).abs() < 1e-12, "ndcg {}", scored.ndcg);
        assert!((scored.coverage - 1.0).abs() < 1e-12);
        assert!((scored.reciprocal_rank - 1.0).abs() < 1e-12);
    }

    #[test]
    fn the_exact_reverse_scores_worse_but_not_zero() {
        // Reversing is bad, not catastrophic: the good documents are still on
        // the page. A metric that called this zero would be unable to tell it
        // apart from returning nothing.
        let judged = query("query: q\n 3 https://a/1\n 2 https://a/2\n 1 https://a/3\n");
        let forward = score(
            &judged,
            &urls(&["https://a/1", "https://a/2", "https://a/3"]),
            DEFAULT_DEPTH,
        );
        let backward = score(
            &judged,
            &urls(&["https://a/3", "https://a/2", "https://a/1"]),
            DEFAULT_DEPTH,
        );
        assert!(backward.ndcg < forward.ndcg);
        assert!(backward.ndcg > 0.5, "ndcg {}", backward.ndcg);
    }

    #[test]
    fn returning_nothing_relevant_scores_zero() {
        let judged = query("query: q\n 3 https://a/1\n");
        let scored = score(
            &judged,
            &urls(&["https://b/9", "https://b/8"]),
            DEFAULT_DEPTH,
        );
        assert!(scored.ndcg.abs() < f64::EPSILON);
        assert!(scored.reciprocal_rank.abs() < f64::EPSILON);
        assert!(scored.coverage.abs() < f64::EPSILON, "nothing was judged");
    }

    #[test]
    fn position_matters_more_than_count() {
        // One excellent result at the top beats three mediocre ones, which is
        // the whole reason gain is exponential.
        let judged =
            query("query: q\n 3 https://a/best\n 1 https://a/x\n 1 https://a/y\n 1 https://a/z\n");
        let top = score(&judged, &urls(&["https://a/best"]), DEFAULT_DEPTH);
        let spread = score(
            &judged,
            &urls(&["https://a/x", "https://a/y", "https://a/z"]),
            DEFAULT_DEPTH,
        );
        assert!(top.ndcg > spread.ndcg, "{} vs {}", top.ndcg, spread.ndcg);
    }

    #[test]
    fn coverage_reports_how_much_of_the_ranking_was_never_judged() {
        // This is the number that decides whether the rest of the row means
        // anything. Two of four results judged is 0.5, and a comparison at
        // that coverage is barely evidence.
        let judged = query("query: q\n 3 https://a/1\n 0 https://a/2\n");
        let scored = score(
            &judged,
            &urls(&[
                "https://a/1",
                "https://a/2",
                "https://new/1",
                "https://new/2",
            ]),
            DEFAULT_DEPTH,
        );
        assert!((scored.coverage - 0.5).abs() < 1e-12, "{}", scored.coverage);
    }

    #[test]
    fn a_query_judged_to_have_no_good_answer_does_not_divide_by_zero() {
        let judged = query("query: q\n 0 https://a/1\n");
        let scored = score(&judged, &urls(&["https://a/1"]), DEFAULT_DEPTH);
        assert!((scored.ndcg - 1.0).abs() < 1e-12);
        assert!((scored.recall - 1.0).abs() < 1e-12);
    }

    #[test]
    fn results_below_the_depth_are_not_scored() {
        // The product is ten links. A brilliant result at position eleven did
        // not help anybody, and the metric must not pretend it did.
        let judged = query("query: q\n 3 https://a/deep\n");
        let mut ranking: Vec<String> = (0..10).map(|i| format!("https://filler/{i}")).collect();
        ranking.push("https://a/deep".to_owned());
        let scored = score(&judged, &ranking, DEFAULT_DEPTH);
        assert!(scored.ndcg.abs() < f64::EPSILON);
    }

    #[test]
    fn an_empty_summary_is_zeros_rather_than_a_panic() {
        let summary = Summary::of(&[]);
        assert_eq!(summary.queries, 0);
        assert!(summary.ndcg.abs() < f64::EPSILON);
    }
}
