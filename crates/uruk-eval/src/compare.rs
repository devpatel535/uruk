//! Whether a ranking change actually helped, or whether it just moved.
//!
//! A judged set of fifty queries produces a mean nDCG to three decimal places,
//! and that precision is an illusion. Swap two queries for two others and the
//! third decimal moves. The question that matters is not "is the new number
//! bigger" but **"would a difference this large show up by chance?"**
//!
//! # The paired randomisation test
//!
//! Both configurations are run over the *same* queries, so each query yields a
//! pair of scores. Under the null hypothesis — the change made no difference —
//! the two numbers in each pair are interchangeable: which configuration
//! produced which is an accident.
//!
//! So: shuffle. For each trial, flip a coin per query and swap that query's
//! pair if it comes up heads. Recompute the mean difference. Do that many
//! thousand times and count how often the shuffled difference is at least as
//! large as the one actually observed. That fraction is the p-value.
//!
//! This is Fisher's randomisation test, and Smucker, Allan and Carterette
//! (2007) compared it against the t-test, the sign test, the Wilcoxon test and
//! the bootstrap on TREC data: randomisation and the bootstrap agreed with
//! each other, and the sign and Wilcoxon tests disagreed with both. It is also
//! the one that needs no assumption about how nDCG is distributed, which is
//! the honest position, because nobody knows.
//!
//! # What a p-value here is and is not
//!
//! It is: the probability of seeing a difference this big if the change did
//! nothing. It is not: the probability that the change did nothing, the size
//! of the improvement, or a licence to ship. A change can be real and useless,
//! and with fifty queries anything under about two nDCG points will not reach
//! significance no matter how many trials are run — which is a fact about the
//! query set, not about the change.

use crate::metrics::Scored;

/// Randomisation trials. Ten thousand puts the resolution of the p-value at
/// 0.0001, which is far finer than any decision made from it needs.
pub const DEFAULT_TRIALS: usize = 10_000;

/// The outcome of comparing two configurations over one query set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Comparison {
    pub queries: usize,
    /// Mean nDCG of the baseline.
    pub baseline: f64,
    /// Mean nDCG of the variant.
    pub variant: f64,
    /// `variant - baseline`. Positive means the change helped on average.
    pub difference: f64,
    /// Queries the variant scored better on.
    pub better: usize,
    /// Queries the variant scored worse on.
    pub worse: usize,
    /// Queries that did not move at all.
    pub unchanged: usize,
    /// Probability of a difference at least this large if the change did
    /// nothing. Two-sided.
    pub p_value: f64,
    pub trials: usize,
}

impl Comparison {
    /// The conventional 0.05 threshold — reported, not obeyed.
    ///
    /// The number is a convention with no special standing; it is here so that
    /// a report can print one word instead of asking every reader to remember
    /// what 0.05 means.
    pub fn significant(&self) -> bool {
        self.p_value < 0.05
    }

    /// One sentence a human can act on.
    pub fn verdict(&self) -> String {
        if self.queries < 20 {
            return format!(
                "{} queries is too few to conclude anything; treat this as a smoke test",
                self.queries
            );
        }
        if !self.significant() {
            return format!(
                "no detectable difference (p = {:.3}); the change moved {} queries up and {} down",
                self.p_value, self.better, self.worse
            );
        }
        let direction = if self.difference > 0.0 {
            "better"
        } else {
            "worse"
        };
        format!(
            "{:.4} nDCG {direction} (p = {:.3}), up on {} queries and down on {}",
            self.difference.abs(),
            self.p_value,
            self.better,
            self.worse
        )
    }
}

/// Deterministic generator, so a comparison run twice gives the same p-value.
///
/// A significance test whose answer changes between runs is a significance
/// test nobody can quote in a commit message.
struct Coin(u64);

impl Coin {
    fn flip(&mut self) -> bool {
        // xorshift64*: short, fast, and good enough for coin flips.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 63 == 1
    }
}

/// Compare two configurations' per-query scores.
///
/// `baseline` and `variant` must be the same queries in the same order; the
/// pairing is what makes the test paired, and mismatched input would compare
/// unrelated numbers.
///
/// # Panics
///
/// If the two slices differ in length, which can only mean the caller ran
/// different query sets and the comparison would be meaningless.
pub fn compare(baseline: &[Scored], variant: &[Scored], trials: usize) -> Comparison {
    assert_eq!(
        baseline.len(),
        variant.len(),
        "a paired test needs the same queries on both sides"
    );

    let n = baseline.len();
    if n == 0 {
        return Comparison {
            queries: 0,
            baseline: 0.0,
            variant: 0.0,
            difference: 0.0,
            better: 0,
            worse: 0,
            unchanged: 0,
            p_value: 1.0,
            trials: 0,
        };
    }

    let differences: Vec<f64> = baseline
        .iter()
        .zip(variant)
        .map(|(before, after)| after.ndcg - before.ndcg)
        .collect();

    let observed = differences.iter().sum::<f64>() / n as f64;
    let better = differences.iter().filter(|d| **d > 0.0).count();
    let worse = differences.iter().filter(|d| **d < 0.0).count();

    // Shuffle the sign of each query's difference, which is exactly what
    // swapping that query's two scores does.
    let mut coin = Coin(0x9E37_79B9_7F4A_7C15);
    let mut at_least_as_extreme = 0usize;
    for _ in 0..trials {
        let mut total = 0.0;
        for &difference in &differences {
            total += if coin.flip() { -difference } else { difference };
        }
        if (total / n as f64).abs() >= observed.abs() {
            at_least_as_extreme += 1;
        }
    }

    // The observed arrangement is itself one of the possible shuffles, so it
    // is counted on both sides. Without this a p-value can come out at exactly
    // zero, which claims more certainty than any finite number of trials can
    // support.
    let p_value = (at_least_as_extreme + 1) as f64 / (trials + 1) as f64;

    Comparison {
        queries: n,
        baseline: baseline.iter().map(|s| s.ndcg).sum::<f64>() / n as f64,
        variant: variant.iter().map(|s| s.ndcg).sum::<f64>() / n as f64,
        difference: observed,
        better,
        worse,
        unchanged: n - better - worse,
        p_value,
        trials,
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_TRIALS, compare};
    use crate::metrics::Scored;

    fn scores(values: &[f64]) -> Vec<Scored> {
        values
            .iter()
            .map(|&ndcg| Scored {
                ndcg,
                precision: 0.0,
                reciprocal_rank: 0.0,
                recall: 0.0,
                coverage: 1.0,
                returned: 10,
            })
            .collect()
    }

    #[test]
    fn an_identical_configuration_is_never_significant() {
        let same = scores(&[0.5; 40]);
        let result = compare(&same, &same, DEFAULT_TRIALS);
        assert!(result.difference.abs() < f64::EPSILON);
        assert!(!result.significant(), "p = {}", result.p_value);
        assert_eq!(result.unchanged, 40);
    }

    #[test]
    fn a_large_consistent_improvement_is_detected() {
        let before = scores(&[0.4; 40]);
        let after = scores(&[0.7; 40]);
        let result = compare(&before, &after, DEFAULT_TRIALS);
        assert!(result.difference > 0.29);
        assert!(result.significant(), "p = {}", result.p_value);
        assert_eq!(result.better, 40);
    }

    #[test]
    fn noise_that_averages_to_nothing_is_not_significant() {
        // Half the queries up by a lot, half down by the same. A test that
        // called this a win would bless every change ever made.
        let before = scores(&[0.5; 40]);
        let mut after = Vec::new();
        for index in 0..40 {
            after.push(if index % 2 == 0 { 0.9 } else { 0.1 });
        }
        let result = compare(&before, &scores(&after), DEFAULT_TRIALS);
        assert!(!result.significant(), "p = {}", result.p_value);
        assert_eq!(result.better, 20);
        assert_eq!(result.worse, 20);
    }

    #[test]
    fn a_tiny_improvement_on_a_small_set_is_not_called_a_win() {
        // This is the case the whole module exists for: the mean went up, and
        // that is not evidence of anything.
        let before = scores(&[0.50, 0.60, 0.40, 0.55, 0.45, 0.50, 0.62, 0.38]);
        let after = scores(&[0.51, 0.59, 0.41, 0.56, 0.44, 0.51, 0.61, 0.39]);
        let result = compare(&before, &after, DEFAULT_TRIALS);
        assert!(result.difference > 0.0, "the mean did go up");
        assert!(
            result.verdict().contains("too few"),
            "verdict was {:?}",
            result.verdict()
        );
    }

    #[test]
    fn the_p_value_is_never_exactly_zero() {
        // Ten thousand trials cannot establish impossibility, and a printed
        // p = 0.000 would claim it did.
        let before = scores(&[0.0; 50]);
        let after = scores(&[1.0; 50]);
        let result = compare(&before, &after, DEFAULT_TRIALS);
        assert!(result.p_value > 0.0);
    }

    #[test]
    fn the_same_comparison_twice_gives_the_same_p_value() {
        let before = scores(&[0.3, 0.5, 0.7, 0.2, 0.9, 0.4, 0.6, 0.8, 0.1, 0.5]);
        let after = scores(&[0.4, 0.5, 0.6, 0.3, 0.9, 0.5, 0.6, 0.7, 0.2, 0.5]);
        let first = compare(&before, &after, DEFAULT_TRIALS);
        let second = compare(&before, &after, DEFAULT_TRIALS);
        assert!((first.p_value - second.p_value).abs() < f64::EPSILON);
    }

    #[test]
    fn an_empty_comparison_does_not_divide_by_zero() {
        let result = compare(&[], &[], DEFAULT_TRIALS);
        assert_eq!(result.queries, 0);
        assert!((result.p_value - 1.0).abs() < f64::EPSILON);
    }
}
