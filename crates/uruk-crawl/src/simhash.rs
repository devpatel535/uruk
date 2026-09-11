//! Near-duplicate detection with `SimHash`.
//!
//! The same article turns up on mirrors, syndication partners, print views and
//! `?utm_source=` variants that URL normalisation did not catch. Indexing it
//! five times wastes five result slots and inflates the index for no gain
//! (`RESEARCH.md` §3.2, §3.3).
//!
//! `SimHash` gives every document a 64-bit fingerprint with the property that
//! *similar documents get similar fingerprints* — unlike a normal hash, where
//! one changed byte changes everything. Two documents are near-duplicates if
//! their fingerprints differ in at most a few bit positions. Manku, Jain and
//! Das Sarma used exactly this at Google over 8 billion pages, with 64 bits and
//! a threshold of 3, which is what [`DEFAULT_THRESHOLD`] is.
//!
//! The lookup trick is the other half of their paper. Comparing a new
//! fingerprint against every stored one is quadratic and hopeless at scale.
//! Instead, split the 64 bits into [`BLOCKS`] equal blocks: if two
//! fingerprints differ in fewer bits than there are blocks, then by the
//! pigeonhole principle at least one block must be **identical**. So we keep
//! one hash map per block, look the new fingerprint's blocks up in each, and
//! only compare against that handful of candidates.
//!
//! That argument requires the threshold to be strictly less than the number of
//! blocks, which is why [`BLOCKS`] is 8 rather than Manku's 4: it is what
//! allows a threshold above 3 while keeping the lookup exact rather than
//! merely likely.
//!
//! # Choosing the threshold, from measurement
//!
//! Manku's threshold of 3 was calibrated for 8 billion pages, where a false
//! positive is expensive and documents carried richer weighted features. At
//! our scale it is too tight, and measuring on real prose says so plainly.
//! Distances observed by `tests/calibrate.rs` on 200–255-word articles:
//!
//! | change | bit distance |
//! |---|---|
//! | byte-identical | 0 |
//! | navigation header added | 2 |
//! | footer added | 4 |
//! | truncated by 1% / 5% / 10% | 2 / 5 / 7 |
//! | one word changed (median / p95 / max) | 2–4 / 5–7 / 10 |
//! | **unrelated documents (min over 40 pairs)** | **28** |
//!
//! The two populations are far apart: near-duplicates land under about 10
//! bits, unrelated documents never came closer than 28. A threshold of 3 sits
//! so low that a page which merely gained a footer is treated as new content.
//! [`DEFAULT_THRESHOLD`] is therefore 6 — above every realistic mirror,
//! reprint and truncation, and with a wide margin below 28.
//!
//! The errors are not symmetric and the threshold is biased accordingly. A
//! false negative indexes a mirror twice, which wastes a result slot. A false
//! positive silently discards a genuine document, which is much worse and
//! cannot be noticed after the fact.
//!
//! Short documents remain noisy — the fingerprint summarises a balance of
//! evidence, and a stub has little evidence to balance — so
//! [`MIN_RELIABLE_WORDS`] records the length below which only near-exact
//! matches should be trusted.

use std::collections::HashMap;

/// Bit differences allowed before two documents count as distinct.
///
/// Derived from measurement rather than inherited; see the module docs.
pub const DEFAULT_THRESHOLD: u32 = 6;

/// Below this many words, only near-exact matches are detected. See the
/// module docs: this is a property of the algorithm, not a tunable.
pub const MIN_RELIABLE_WORDS: usize = 200;

/// Words per shingle. Hashing single words would call any two documents with
/// the same vocabulary identical; three-word groups capture enough word order
/// to tell a rewrite from a reprint.
const SHINGLE: usize = 3;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x1000_0000_01b3;

/// FNV-1a. Chosen over [`std::hash::DefaultHasher`] because fingerprints are
/// written to disk and compared across runs: this has to produce the same
/// number in five years' time, which the standard hasher does not promise.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Split text into lowercase alphanumeric words.
fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Compute the 64-bit `SimHash` fingerprint of a document's text.
///
/// Returns 0 for text with no words, which callers should treat as "no
/// fingerprint" rather than as a document to be deduplicated against.
pub fn fingerprint(text: &str) -> u64 {
    let words = words(text);
    if words.is_empty() {
        return 0;
    }

    // One running total per bit position. A shingle whose hash has bit `i` set
    // pushes column `i` up; one with it clear pushes it down. The sign of each
    // column at the end becomes that bit of the fingerprint, so a bit only
    // flips when the *weight of evidence* across the whole document shifts —
    // which is what makes the fingerprint stable under small edits.
    let mut columns = [0i64; 64];
    let mut buffer = String::new();

    // A document shorter than one shingle still deserves a fingerprint, so fall
    // back to hashing its words individually.
    let windows = if words.len() >= SHINGLE {
        words.len() - SHINGLE + 1
    } else {
        1
    };
    for start in 0..windows {
        buffer.clear();
        let end = (start + SHINGLE).min(words.len());
        for (i, word) in words[start..end].iter().enumerate() {
            if i > 0 {
                buffer.push(' ');
            }
            // Case folding here rather than in `words` avoids allocating a
            // lowercase copy of the whole document.
            buffer.extend(word.chars().flat_map(char::to_lowercase));
        }

        let hash = fnv1a(buffer.as_bytes());
        for (bit, column) in columns.iter_mut().enumerate() {
            if hash & (1u64 << bit) == 0 {
                *column -= 1;
            } else {
                *column += 1;
            }
        }
    }

    let mut fingerprint = 0u64;
    for (bit, &column) in columns.iter().enumerate() {
        if column > 0 {
            fingerprint |= 1u64 << bit;
        }
    }
    fingerprint
}

/// Number of differing bits between two fingerprints.
pub fn distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Blocks the fingerprint is split into for candidate lookup.
///
/// The pigeonhole argument in the module docs holds only while the threshold
/// is strictly less than this, so raising the threshold means raising this
/// too. [`SeenFingerprints::with_threshold`] enforces the relationship.
/// Largest threshold the block lookup stays exact for: one less than
/// [`BLOCKS`], by the pigeonhole argument above.
pub const MAX_EXACT_THRESHOLD: u32 = 7;

pub const BLOCKS: usize = MAX_EXACT_THRESHOLD as usize + 1;
const BLOCK_BITS: u32 = 8;
const BLOCK_MASK: u64 = (1 << BLOCK_BITS) - 1;

// The blocks must tile the fingerprint exactly, or some bits would never be
// looked at and the pigeonhole guarantee would quietly stop holding.
const _: () = assert!(BLOCKS * (BLOCK_BITS as usize) == 64);

fn block(fingerprint: u64, index: usize) -> u64 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "index < BLOCKS, so the shift fits"
    )]
    let shift = (index as u32) * BLOCK_BITS;
    (fingerprint >> shift) & BLOCK_MASK
}

/// A set of seen fingerprints supporting "is anything near this?" queries.
#[derive(Debug, Default)]
pub struct SeenFingerprints {
    fingerprints: Vec<u64>,
    /// One map per block: block value -> positions in `fingerprints`.
    buckets: [HashMap<u64, Vec<usize>>; BLOCKS],
    threshold: u32,
}

impl SeenFingerprints {
    pub fn new() -> Self {
        Self::with_threshold(DEFAULT_THRESHOLD)
    }

    /// Build with a specific threshold.
    ///
    /// # Panics
    ///
    /// Panics above [`MAX_EXACT_THRESHOLD`]. Past that the block lookup would
    /// start missing duplicates without saying so, and a deduplicator that
    /// quietly stops deduplicating is worse than one that refuses to start.
    pub fn with_threshold(threshold: u32) -> Self {
        assert!(
            threshold <= MAX_EXACT_THRESHOLD,
            "threshold {threshold} exceeds {MAX_EXACT_THRESHOLD}; raise BLOCKS to go higher"
        );
        Self {
            threshold,
            ..Self::default()
        }
    }

    /// Find an already-seen fingerprint within the threshold, if any.
    ///
    /// Only fingerprints sharing at least one block are compared, which is
    /// exact while the threshold is below [`BLOCKS`]: with fewer differing
    /// bits than blocks, some block must be untouched.
    pub fn find_near(&self, fingerprint: u64) -> Option<u64> {
        if fingerprint == 0 {
            return None;
        }
        for index in 0..BLOCKS {
            let Some(candidates) = self.buckets[index].get(&block(fingerprint, index)) else {
                continue;
            };
            for &position in candidates {
                let candidate = self.fingerprints[position];
                if distance(fingerprint, candidate) <= self.threshold {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// Record a fingerprint. Returns false for the empty fingerprint, which is
    /// not stored.
    pub fn insert(&mut self, fingerprint: u64) -> bool {
        if fingerprint == 0 {
            return false;
        }
        let position = self.fingerprints.len();
        self.fingerprints.push(fingerprint);
        for index in 0..BLOCKS {
            self.buckets[index]
                .entry(block(fingerprint, index))
                .or_default()
                .push(position);
        }
        true
    }

    /// Insert unless something near it is already present.
    ///
    /// Returns the fingerprint it duplicates, or `None` if it was new and has
    /// now been recorded. This is the call sites' preferred entry point: doing
    /// the check and the insert together removes the window in which the same
    /// document could pass the check twice.
    pub fn insert_unless_duplicate(&mut self, fingerprint: u64) -> Option<u64> {
        if let Some(existing) = self.find_near(fingerprint) {
            return Some(existing);
        }
        self.insert(fingerprint);
        None
    }

    pub fn len(&self) -> usize {
        self.fingerprints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fingerprints.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_THRESHOLD, MIN_RELIABLE_WORDS, SeenFingerprints, distance, fingerprint};

    /// Deterministic prose of a requested length, with a realistic vocabulary
    /// distribution. Used instead of a fixed string so that tests can be
    /// explicit about the document length they depend on.
    fn article(words: usize) -> String {
        const VOCAB: [&str; 36] = [
            "the",
            "tablet",
            "of",
            "uruk",
            "records",
            "grain",
            "and",
            "debt",
            "a",
            "scribe",
            "pressed",
            "reed",
            "into",
            "wet",
            "clay",
            "then",
            "dried",
            "it",
            "in",
            "sun",
            "what",
            "survives",
            "is",
            "not",
            "literature",
            "but",
            "bookkeeping",
            "which",
            "outlasted",
            "every",
            "empire",
            "that",
            "produced",
            "them",
            "over",
            "centuries",
        ];
        (0..words)
            .map(|i| VOCAB[(i * 7 + i / 11) % VOCAB.len()])
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn identical_text_gives_an_identical_fingerprint() {
        let text = article(300);
        assert_eq!(fingerprint(&text), fingerprint(&text));
    }

    #[test]
    fn fingerprints_are_stable_across_runs() {
        // Pinned so that a change to the hash function is caught here rather
        // than by silently invalidating every fingerprint already on disk.
        assert_eq!(
            fingerprint("the quick brown fox"),
            2_600_275_743_550_894_345
        );
    }

    #[test]
    fn case_and_punctuation_do_not_matter() {
        assert_eq!(fingerprint("Clay, tablets!"), fingerprint("clay tablets"));
    }

    #[test]
    fn a_small_edit_barely_moves_the_fingerprint_on_a_real_article() {
        let original = article(MIN_RELIABLE_WORDS);
        let edited = original.replacen("grain", "barley", 1);
        let moved = distance(fingerprint(&original), fingerprint(&edited));
        assert!(
            moved <= DEFAULT_THRESHOLD,
            "a one-word edit moved {moved} bits"
        );
    }

    #[test]
    fn short_documents_are_knowingly_unreliable() {
        // Documents this short are why MIN_RELIABLE_WORDS exists. Asserting the
        // limitation keeps it a known property rather than a latent surprise.
        let original = article(20);
        let edited = original.replacen("grain", "barley", 1);
        let moved = distance(fingerprint(&original), fingerprint(&edited));
        assert!(
            moved > DEFAULT_THRESHOLD,
            "expected a short document to be noisy, moved {moved}"
        );
    }

    #[test]
    fn unrelated_text_is_far_away_at_any_length() {
        // The failure mode we must never have is a false positive.
        for words in [20usize, 100, 500] {
            let other: String =
                std::iter::repeat_n("borrow checker lifetimes generics traits", words / 5)
                    .collect::<Vec<_>>()
                    .join(" ");
            let moved = distance(fingerprint(&article(words)), fingerprint(&other));
            assert!(
                moved > DEFAULT_THRESHOLD,
                "{words} words: unrelated text only {moved} bits apart"
            );
        }
    }

    #[test]
    fn word_order_matters() {
        // Shingling buys this; a bag-of-words hash would call these equal.
        let forward = fingerprint("alpha beta gamma delta epsilon zeta");
        let shuffled = fingerprint("zeta epsilon delta gamma beta alpha");
        assert!(distance(forward, shuffled) > DEFAULT_THRESHOLD);
    }

    #[test]
    fn empty_text_has_no_fingerprint() {
        assert_eq!(fingerprint(""), 0);
        assert_eq!(fingerprint("   !!!  "), 0);
    }

    #[test]
    fn very_short_text_still_gets_a_fingerprint() {
        assert_ne!(fingerprint("two words"), 0);
        assert_ne!(fingerprint("one"), 0);
    }

    #[test]
    fn the_index_finds_a_near_duplicate() {
        let mut seen = SeenFingerprints::new();
        let original = article(400);
        assert!(seen.insert(fingerprint(&original)));

        let mirrored = original.replacen("grain", "barley", 1);
        assert!(seen.find_near(fingerprint(&mirrored)).is_some());
    }

    #[test]
    fn the_index_finds_an_exact_duplicate_of_any_length() {
        // Short pages still dedupe when they are genuinely identical, which is
        // the common case for boilerplate stubs and mirrored error pages.
        let mut seen = SeenFingerprints::new();
        seen.insert(fingerprint("page not found"));
        assert!(seen.find_near(fingerprint("page not found")).is_some());
    }

    #[test]
    fn the_index_does_not_confuse_distinct_documents() {
        let mut seen = SeenFingerprints::new();
        seen.insert(fingerprint(&article(300)));
        let unrelated = "something else entirely, concerning lifetimes and the borrow checker";
        assert!(seen.find_near(fingerprint(unrelated)).is_none());
    }

    #[test]
    fn insert_unless_duplicate_reports_the_original() {
        let mut seen = SeenFingerprints::new();
        let original = article(400);
        let first = fingerprint(&original);
        assert_eq!(seen.insert_unless_duplicate(first), None);
        assert_eq!(seen.len(), 1);

        let again = fingerprint(&original.replacen("grain", "barley", 1));
        assert_eq!(seen.insert_unless_duplicate(again), Some(first));
        // The duplicate must not have been stored.
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn the_empty_fingerprint_is_never_stored_or_matched() {
        let mut seen = SeenFingerprints::new();
        assert!(!seen.insert(0));
        assert!(seen.is_empty());
        assert!(seen.find_near(0).is_none());
    }

    #[test]
    fn block_lookup_agrees_with_brute_force() {
        // The pigeonhole shortcut must not miss anything a full scan finds.
        let mut seen = SeenFingerprints::new();
        let mut stored = Vec::new();
        for i in 0..500u64 {
            let fp = fingerprint(&format!("document number {i}: {}", article(60)));
            seen.insert(fp);
            stored.push(fp);
        }
        for i in 0..500u64 {
            let probe = fingerprint(&format!("document number {i}: {}", article(60)));
            let brute = stored
                .iter()
                .any(|&s| distance(probe, s) <= DEFAULT_THRESHOLD);
            assert_eq!(
                seen.find_near(probe).is_some(),
                brute,
                "disagreement at {i}"
            );
        }
    }
}
