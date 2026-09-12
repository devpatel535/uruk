//! Scoring, and explaining the score.
//!
//! BM25 is the base, as `RESEARCH.md` §2.4 insists: it is three ideas and
//! fifteen lines of arithmetic, it has been the baseline to beat for thirty
//! years, and nothing clever should be attempted until it is working and
//! measured.
//!
//! The three ideas:
//!
//! - a document containing your **rare** words beats one containing your common
//!   words (IDF);
//! - a word appearing ten times beats once, but not ten times as much — the
//!   benefit **saturates** (`k1`);
//! - a short document containing your word beats a long one containing it just
//!   as often, because length dilutes (`b`).
//!
//! `BM25F` extends this to fields. The important detail, and the reason it is
//! done this way from the start: per-field frequencies are combined **first**,
//! with weights and per-field length normalisation, and the saturation curve is
//! applied **once** to the combination. Scoring each field separately and
//! adding the results breaks saturation, so a term stuffed into a title could
//! outrank a genuinely relevant page.
//!
//! # Everything is explained
//!
//! The brief requires every result to be able to say why it ranked where it
//! did. That is not a debug mode bolted on afterwards — [`Explanation`] is what
//! scoring returns, and the total is the sum of parts that are each reported.
//! Tuning ranking from user feedback is impossible otherwise.

use uruk_index::fields::{FIELD_COUNT, Field, FieldCounts, FieldLengths, FieldWeights};
use uruk_index::segment::DocQuality;

/// BM25's saturation parameter.
///
/// The standard starting value. Higher means term frequency keeps mattering
/// for longer; lower means one occurrence is nearly as good as many.
pub const DEFAULT_K1: f64 = 1.2;

/// BM25's length-normalisation parameter, per field.
///
/// `b = 0.75` is standard for body text. Titles and URLs get less: a
/// three-word title is not "better" than a six-word one in the way a short
/// article is more focused than a long one, so normalising them hard just
/// rewards terse titles.
const fn default_b(field: Field) -> f64 {
    match field {
        Field::Body => 0.75,
        Field::Heading => 0.5,
        Field::Title | Field::Url => 0.3,
    }
}

/// Weights for the signals that are not term relevance.
///
/// All small. Text relevance is meant to dominate; `RESEARCH.md` §5.4 argues
/// these must not be tuned by eye before a judged query set exists, so they
/// are starting values and are marked as such.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub fields: FieldWeights,
    pub k1: f64,
    pub b: [f64; FIELD_COUNT],
    /// How much query terms appearing close together is worth.
    pub proximity: f64,
    /// How much the host's standing in the link graph is worth.
    ///
    /// Small, for two reasons `RESEARCH.md` §5.3 sets out. It is the signal
    /// spam attacks hardest, so a large weight is an invitation. And on a
    /// topical crawl the link graph is truncated — most in-links to any host
    /// come from pages we never fetched — so it is also the *weakest* evidence
    /// available, not merely the most dangerous.
    pub authority: f64,
    /// How much the content-quality proxies are worth. The smallest signal:
    /// it is a guess about a page, not evidence about a query, and unlike
    /// authority it is not even somebody else's opinion.
    pub quality: f64,
}

impl Default for Weights {
    fn default() -> Self {
        let mut b = [0.0; FIELD_COUNT];
        for field in Field::ALL {
            b[field.index()] = default_b(field);
        }
        Self {
            fields: FieldWeights::default(),
            k1: DEFAULT_K1,
            b,
            proximity: 0.6,
            authority: 0.3,
            quality: 0.4,
        }
    }
}

/// Corpus statistics a scorer needs. Global, not per segment — see
/// [`uruk_index::index::Index`].
#[derive(Debug, Clone, Copy)]
pub struct CorpusStats {
    /// Total documents.
    pub documents: u32,
    /// Average tokens per document, per field.
    pub average_lengths: [f64; FIELD_COUNT],
}

/// Inverse document frequency: how surprising it is to see this term.
///
/// The `ln(1 + …)` form rather than the textbook `ln(…)`, because the textbook
/// one goes negative for a term in more than half the corpus, and a negative
/// contribution means matching a common word actively *hurts* a document.
pub fn idf(documents: u32, doc_frequency: u32) -> f64 {
    let n = f64::from(documents);
    let df = f64::from(doc_frequency.min(documents));
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// What one term contributed, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct TermScore {
    pub term: String,
    /// Documents containing it.
    pub doc_frequency: u32,
    pub idf: f64,
    /// Per-field occurrence counts in this document.
    pub counts: FieldCounts,
    /// Weighted, length-normalised frequency: the input to saturation.
    pub pseudo_frequency: f64,
    /// This term's share of the total.
    pub contribution: f64,
}

/// What the content-quality proxies said about the page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityScore {
    pub text_ratio: f32,
    pub link_density: f32,
    pub scripts: u32,
    /// Combined, in `0.0..=1.0`.
    pub factor: f64,
    pub contribution: f64,
}

/// Why a document scored what it did.
///
/// `total` is exactly `text_relevance + proximity + quality`; the parts are not
/// approximations of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    pub total: f64,
    /// The BM25F sum over query terms.
    pub text_relevance: f64,
    pub terms: Vec<TermScore>,
    /// Bonus for query terms appearing close together.
    pub proximity: f64,
    /// The closest the query terms came, in tokens. `None` for a single-term
    /// query, where proximity is meaningless.
    pub closest_span: Option<u32>,
    pub authority: AuthorityScore,
    pub quality: QualityScore,
}

/// What the host's standing in the link graph contributed, and why.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AuthorityScore {
    /// The host's authority in `0.0..=1.0`, as [`uruk_link::Authority`]
    /// computed it. Zero when no authority table was supplied, which is not
    /// the same as a host nobody links to — [`known`](Self::known) tells them
    /// apart so an explanation can say "not measured" rather than "worthless".
    pub factor: f64,
    /// Whether an authority table was consulted at all.
    pub known: bool,
    pub contribution: f64,
}

impl Explanation {
    /// Signals in the order the brief lists them, for display.
    pub fn signals(&self) -> Vec<(&'static str, f64)> {
        vec![
            ("text relevance (BM25F)", self.text_relevance),
            ("proximity", self.proximity),
            ("host authority", self.authority.contribution),
            ("content quality", self.quality.contribution),
        ]
    }
}

/// Scores documents against one query's terms.
#[derive(Debug, Clone)]
pub struct Scorer {
    weights: Weights,
    stats: CorpusStats,
}

impl Scorer {
    pub fn new(stats: CorpusStats, weights: Weights) -> Self {
        Self { weights, stats }
    }

    pub fn weights(&self) -> Weights {
        self.weights
    }

    /// `BM25F`'s pseudo-frequency: per-field counts, each normalised by that
    /// field's length and scaled by its weight, then added.
    ///
    /// This is the step that must happen before saturation.
    fn pseudo_frequency(&self, counts: FieldCounts, lengths: FieldLengths) -> f64 {
        let mut total = 0.0;
        for field in Field::ALL {
            let count = f64::from(counts.get(field));
            if count == 0.0 {
                continue;
            }
            let average = self.stats.average_lengths[field.index()];
            // With no corpus statistics for a field, skip normalisation rather
            // than divide by zero.
            let normalised = if average > 0.0 {
                let b = self.weights.b[field.index()];
                let length = f64::from(lengths.get(field));
                count / (1.0 - b + b * (length / average))
            } else {
                count
            };
            total += self.weights.fields.get(field) * normalised;
        }
        total
    }

    /// Score one term against one document.
    pub fn term(
        &self,
        term: &str,
        doc_frequency: u32,
        counts: FieldCounts,
        lengths: FieldLengths,
    ) -> TermScore {
        let idf = idf(self.stats.documents, doc_frequency);
        let pseudo = self.pseudo_frequency(counts, lengths);
        // Saturation, applied once to the combined frequency.
        let contribution = idf * pseudo / (self.weights.k1 + pseudo);

        TermScore {
            term: term.to_owned(),
            doc_frequency,
            idf,
            counts,
            pseudo_frequency: pseudo,
            contribution,
        }
    }

    /// Turn a span between query terms into a bonus.
    ///
    /// `span` is the width of the smallest window containing one occurrence of
    /// every term. Terms sitting next to each other give a span of exactly
    /// `terms - 1`, and the bonus falls away as they spread out.
    fn proximity_bonus(&self, span: u32, terms: usize) -> f64 {
        if terms < 2 {
            return 0.0;
        }
        let tightest = (terms - 1) as f64;
        let gap = f64::from(span) - tightest;
        self.weights.proximity * terms as f64 / (1.0 + gap.max(0.0))
    }

    /// Combine the content-quality proxies into `0.0..=1.0`.
    ///
    /// Reading: a page that is mostly text scores well, a page whose words are
    /// mostly link text scores badly, and scripts count against on the
    /// argument that they measure how much of the page is advertising and
    /// tracking rather than writing (`RESEARCH.md` §4).
    fn quality_factor(quality: DocQuality) -> f64 {
        // A text-to-markup ratio above 0.5 is already excellent, so the scale
        // tops out there rather than expecting the impossible.
        let text = f64::from(quality.text_ratio).clamp(0.0, 0.5) / 0.5;
        let links = 1.0 - f64::from(quality.link_density).clamp(0.0, 1.0);
        let scripts = 1.0 / (1.0 + f64::from(quality.scripts) / 15.0);
        (text * links * scripts).clamp(0.0, 1.0)
    }

    /// Score a document, and say why.
    ///
    /// `authority` is the host's standing in `0.0..=1.0`, or `None` when no
    /// authority table was supplied. `None` and `Some(0.0)` are deliberately
    /// different: the first means we did not look, the second means we looked
    /// and nobody links there.
    pub fn document(
        &self,
        terms: Vec<TermScore>,
        quality: DocQuality,
        closest_span: Option<u32>,
        authority: Option<f64>,
    ) -> Explanation {
        let text_relevance: f64 = terms.iter().map(|term| term.contribution).sum();

        let proximity = closest_span.map_or(0.0, |span| self.proximity_bonus(span, terms.len()));

        let factor = Self::quality_factor(quality);
        let quality = QualityScore {
            text_ratio: quality.text_ratio,
            link_density: quality.link_density,
            scripts: quality.scripts,
            factor,
            contribution: self.weights.quality * factor,
        };

        let authority = AuthorityScore {
            factor: authority.unwrap_or(0.0).clamp(0.0, 1.0),
            known: authority.is_some(),
            contribution: self.weights.authority * authority.unwrap_or(0.0).clamp(0.0, 1.0),
        };

        Explanation {
            total: text_relevance + proximity + authority.contribution + quality.contribution,
            text_relevance,
            terms,
            proximity,
            closest_span,
            authority,
            quality,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CorpusStats, DocQuality, Scorer, Weights, idf};
    use uruk_index::fields::{FIELD_COUNT, Field, FieldCounts};

    fn stats() -> CorpusStats {
        let mut average_lengths = [0.0; FIELD_COUNT];
        average_lengths[Field::Body.index()] = 100.0;
        average_lengths[Field::Title.index()] = 8.0;
        average_lengths[Field::Heading.index()] = 10.0;
        average_lengths[Field::Url.index()] = 6.0;
        CorpusStats {
            documents: 1_000,
            average_lengths,
        }
    }

    fn scorer() -> Scorer {
        Scorer::new(stats(), Weights::default())
    }

    fn counts(field: Field, n: u32) -> FieldCounts {
        let mut counts = FieldCounts::default();
        counts.set(field, n);
        counts
    }

    fn lengths(body: u32, title: u32) -> FieldCounts {
        let mut lengths = FieldCounts::default();
        lengths.set(Field::Body, body);
        lengths.set(Field::Title, title);
        lengths
    }

    #[test]
    fn a_rare_term_is_worth_more_than_a_common_one() {
        assert!(idf(1_000, 1) > idf(1_000, 500));
    }

    #[test]
    fn idf_never_goes_negative() {
        // The textbook form does, and a negative contribution would mean
        // matching a common word actively hurts a document.
        for df in [1u32, 500, 999, 1_000] {
            assert!(idf(1_000, df) >= 0.0, "df {df} gave {}", idf(1_000, df));
        }
    }

    #[test]
    fn idf_survives_a_frequency_larger_than_the_corpus() {
        assert!(idf(10, 1_000).is_finite());
    }

    #[test]
    fn more_occurrences_score_higher_but_saturate() {
        let scorer = scorer();
        let at = |n| {
            scorer
                .term("clay", 50, counts(Field::Body, n), lengths(100, 5))
                .contribution
        };

        let (one, two, ten, twenty) = (at(1), at(2), at(10), at(20));
        assert!(two > one, "two occurrences should beat one");
        assert!(ten > two);
        // Saturation: the step from 10 to 20 is much smaller than 1 to 2.
        assert!(twenty - ten < two - one, "term frequency is not saturating");
    }

    #[test]
    fn a_shorter_document_scores_higher_for_the_same_count() {
        let scorer = scorer();
        let short = scorer
            .term("clay", 50, counts(Field::Body, 3), lengths(50, 5))
            .contribution;
        let long = scorer
            .term("clay", 50, counts(Field::Body, 3), lengths(400, 5))
            .contribution;
        assert!(short > long, "length normalisation is not working");
    }

    #[test]
    fn a_title_match_beats_a_body_match() {
        let scorer = scorer();
        let title = scorer
            .term("clay", 50, counts(Field::Title, 1), lengths(100, 8))
            .contribution;
        let body = scorer
            .term("clay", 50, counts(Field::Body, 1), lengths(100, 8))
            .contribution;
        assert!(title > body, "the title field boost is not applied");
    }

    #[test]
    fn fields_are_combined_before_saturation_not_after() {
        // The BM25F property. Scoring fields separately and adding would make
        // a term in two fields worth strictly the sum of the two separately;
        // combining first then saturating makes it worth less than that.
        let scorer = scorer();
        let title_only = scorer
            .term("clay", 50, counts(Field::Title, 1), lengths(100, 8))
            .contribution;
        let body_only = scorer
            .term("clay", 50, counts(Field::Body, 1), lengths(100, 8))
            .contribution;

        let mut both = FieldCounts::default();
        both.set(Field::Title, 1);
        both.set(Field::Body, 1);
        let combined = scorer.term("clay", 50, both, lengths(100, 8)).contribution;

        assert!(combined > title_only, "two fields should beat one");
        assert!(
            combined < title_only + body_only,
            "saturation was applied per field rather than once to the combination"
        );
    }

    #[test]
    fn adjacent_terms_earn_the_largest_proximity_bonus() {
        let scorer = scorer();
        let terms = vec![
            scorer.term("clay", 50, counts(Field::Body, 1), lengths(100, 5)),
            scorer.term("tablets", 50, counts(Field::Body, 1), lengths(100, 5)),
        ];
        let quality = DocQuality {
            text_ratio: 0.4,
            link_density: 0.1,
            scripts: 2,
        };

        // A span of 1 means the two terms are adjacent.
        let adjacent = scorer.document(terms.clone(), quality, Some(1), None);
        let scattered = scorer.document(terms.clone(), quality, Some(60), None);
        let unknown = scorer.document(terms, quality, None, None);

        assert!(adjacent.proximity > scattered.proximity);
        assert!(scattered.proximity > 0.0);
        assert!(
            unknown.proximity.abs() < f64::EPSILON,
            "no span means no bonus"
        );
        assert!(adjacent.total > scattered.total);
    }

    #[test]
    fn a_single_term_query_earns_no_proximity_bonus() {
        let scorer = scorer();
        let terms = vec![scorer.term("clay", 50, counts(Field::Body, 1), lengths(100, 5))];
        let quality = DocQuality::default();
        assert!(
            scorer
                .document(terms, quality, Some(0), None)
                .proximity
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn a_page_that_is_mostly_chrome_scores_lower() {
        let scorer = scorer();
        let terms = vec![scorer.term("clay", 50, counts(Field::Body, 2), lengths(100, 5))];

        let article = DocQuality {
            text_ratio: 0.45,
            link_density: 0.05,
            scripts: 1,
        };
        let link_farm = DocQuality {
            text_ratio: 0.05,
            link_density: 0.9,
            scripts: 40,
        };

        let good = scorer.document(terms.clone(), article, None, None);
        let bad = scorer.document(terms, link_farm, None, None);

        assert!(good.quality.factor > bad.quality.factor);
        assert!(good.total > bad.total);
        assert!((0.0..=1.0).contains(&good.quality.factor));
        assert!((0.0..=1.0).contains(&bad.quality.factor));
    }

    #[test]
    fn the_total_is_exactly_the_sum_of_the_reported_parts() {
        // The breakdown has to be the real thing, not an approximation of it,
        // or tuning from it is guesswork.
        let scorer = scorer();
        let terms = vec![
            scorer.term("clay", 50, counts(Field::Body, 3), lengths(120, 6)),
            scorer.term("tablets", 12, counts(Field::Title, 1), lengths(120, 6)),
        ];
        let quality = DocQuality {
            text_ratio: 0.3,
            link_density: 0.2,
            scripts: 5,
        };
        // With authority measured, and again without: both have to add up, or
        // the promise that every result can say why it ranked is a promise
        // about a number that does not reconcile.
        for standing in [None, Some(0.0), Some(0.75), Some(1.0)] {
            let explanation = scorer.document(terms.clone(), quality, Some(4), standing);

            let summed: f64 = explanation.signals().iter().map(|(_, value)| value).sum();
            assert!(
                (explanation.total - summed).abs() < 1e-12,
                "parts do not sum to the total with authority {standing:?}"
            );

            let from_terms: f64 = explanation.terms.iter().map(|t| t.contribution).sum();
            assert!((explanation.text_relevance - from_terms).abs() < 1e-12);
        }
    }

    #[test]
    fn an_unmeasured_authority_is_not_the_same_as_a_measured_zero() {
        // Both contribute nothing to the score, but a result page that says
        // "0.00" where it should say "not measured" is telling the user the
        // engine looked and found nothing, which is a different claim.
        let scorer = Scorer::new(stats(), Weights::default());
        let terms = vec![scorer.term("clay", 50, counts(Field::Body, 3), lengths(120, 6))];
        let quality = DocQuality {
            text_ratio: 0.3,
            link_density: 0.2,
            scripts: 5,
        };

        let unmeasured = scorer.document(terms.clone(), quality, None, None);
        let measured_zero = scorer.document(terms, quality, None, Some(0.0));

        assert!(!unmeasured.authority.known);
        assert!(measured_zero.authority.known);
        assert!((unmeasured.total - measured_zero.total).abs() < f64::EPSILON);
    }

    #[test]
    fn authority_moves_a_result_but_cannot_carry_it() {
        // The weight is small on purpose. A perfectly authoritative host must
        // not outrank a page that actually matches the query better.
        let scorer = Scorer::new(stats(), Weights::default());
        let quality = DocQuality {
            text_ratio: 0.4,
            link_density: 0.1,
            scripts: 0,
        };

        let relevant = vec![scorer.term("clay", 50, counts(Field::Body, 8), lengths(120, 6))];
        let barely = vec![scorer.term("clay", 50, counts(Field::Body, 1), lengths(800, 6))];

        let good_page_no_authority = scorer.document(relevant, quality, None, Some(0.0));
        let weak_page_best_authority = scorer.document(barely, quality, None, Some(1.0));

        assert!(
            good_page_no_authority.total > weak_page_best_authority.total,
            "authority outranked relevance: {} vs {}",
            good_page_no_authority.total,
            weak_page_best_authority.total
        );
        // But it is not decorative either: the same page with standing beats
        // itself without.
        let terms = vec![scorer.term("clay", 50, counts(Field::Body, 3), lengths(120, 6))];
        let without = scorer.document(terms.clone(), quality, None, Some(0.0));
        let with = scorer.document(terms, quality, None, Some(1.0));
        assert!(with.total > without.total);
    }

    #[test]
    fn scoring_an_empty_corpus_does_not_produce_nonsense() {
        let scorer = Scorer::new(
            CorpusStats {
                documents: 0,
                average_lengths: [0.0; FIELD_COUNT],
            },
            Weights::default(),
        );
        let term = scorer.term("clay", 0, counts(Field::Body, 1), lengths(10, 2));
        assert!(term.contribution.is_finite());
        assert!(term.idf.is_finite());
    }
}
