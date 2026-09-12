//! Which part of a page a term appeared in.
//!
//! A query term matching the title means more than the same term buried in
//! paragraph forty. The brief asks for this as a ranking signal, and
//! `RESEARCH.md` §2.4 argues for doing it the `BM25F` way from the start:
//! combine the per-field frequencies first, with weights, and apply BM25's
//! saturation curve once to the combination. Scoring each field separately and
//! adding the results breaks the saturation and has to be un-picked later.
//!
//! So the index stores a **count per field per document**, and the scorer
//! decides what they are worth.

/// The fields a term can appear in.
///
/// Anchor text is deliberately absent. It belongs to the *target* of a link
/// rather than the page it appears on, so collecting it needs the link graph,
/// which is Phase 6. Adding a variant here later costs a segment format
/// version bump and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Field {
    /// The article text, after boilerplate removal.
    Body = 0,
    /// The `<title>`.
    Title = 1,
    /// `h1`–`h3`.
    Heading = 2,
    /// Words in the URL itself.
    Url = 3,
}

/// Number of fields. Also the width of the bitmask in an encoded posting, so
/// it must stay at or below 8 without widening that mask.
pub const FIELD_COUNT: usize = 4;

// The encoder writes the field mask as a single byte. Adding a fifth, sixth,
// seventh or eighth field is free; a ninth silently would not fit, so fail the
// build instead.
const _: () = assert!(FIELD_COUNT <= 8);

impl Field {
    /// Every field, in storage order.
    pub const ALL: [Self; FIELD_COUNT] = [Self::Body, Self::Title, Self::Heading, Self::Url];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    /// Name used in the manifest and in score breakdowns.
    pub fn name(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Title => "title",
            Self::Heading => "heading",
            Self::Url => "url",
        }
    }
}

/// One count per field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldCounts([u32; FIELD_COUNT]);

impl FieldCounts {
    pub fn get(self, field: Field) -> u32 {
        self.0[field.index()]
    }

    pub fn set(&mut self, field: Field, value: u32) {
        self.0[field.index()] = value;
    }

    pub fn add(&mut self, field: Field, value: u32) {
        self.0[field.index()] = self.0[field.index()].saturating_add(value);
    }

    /// By storage index, for the encoder, which iterates the bitmask.
    pub fn get_index(self, index: usize) -> u32 {
        self.0[index]
    }

    pub fn set_index(&mut self, index: usize, value: u32) {
        self.0[index] = value;
    }

    /// Bit per non-zero field, so an encoded posting can skip the zeroes.
    pub fn mask(self) -> u8 {
        let mut mask = 0u8;
        for (index, &count) in self.0.iter().enumerate() {
            if count > 0 {
                mask |= 1 << index;
            }
        }
        mask
    }

    pub fn total(self) -> u32 {
        self.0.iter().copied().fold(0u32, u32::saturating_add)
    }

    pub fn is_empty(self) -> bool {
        self.total() == 0
    }
}

/// How long each field is, in tokens.
///
/// BM25 needs this: a term appearing twice in a ten-word title is a stronger
/// signal than twice in a thousand-word body, and length normalisation is what
/// expresses that.
pub type FieldLengths = FieldCounts;

/// Per-field weights for `BM25F`.
///
/// Starting values, not tuned ones. `RESEARCH.md` §5.4 argues the judged query
/// set should exist before anyone touches these, because otherwise tuning is
/// just moving numbers around and calling the result better.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldWeights([f64; FIELD_COUNT]);

impl Default for FieldWeights {
    fn default() -> Self {
        // Title above headings above URL above body. The ordering is the
        // brief's; the magnitudes are guesses awaiting evidence.
        let mut weights = [1.0; FIELD_COUNT];
        weights[Field::Body.index()] = 1.0;
        weights[Field::Title.index()] = 3.0;
        weights[Field::Heading.index()] = 2.0;
        weights[Field::Url.index()] = 1.5;
        Self(weights)
    }
}

impl FieldWeights {
    pub fn get(self, field: Field) -> f64 {
        self.0[field.index()]
    }

    pub fn set(&mut self, field: Field, weight: f64) {
        self.0[field.index()] = weight;
    }
}

#[cfg(test)]
mod tests {
    use super::{FIELD_COUNT, Field, FieldCounts, FieldWeights};

    #[test]
    fn every_field_has_a_distinct_stable_index() {
        let mut seen = std::collections::HashSet::new();
        for field in Field::ALL {
            assert!(seen.insert(field.index()), "{field:?} shares an index");
            assert!(field.index() < FIELD_COUNT);
            assert_eq!(Field::from_index(field.index()), Some(field));
        }
        // The indexes are written to disk, so pin them.
        assert_eq!(Field::Body.index(), 0);
        assert_eq!(Field::Title.index(), 1);
        assert_eq!(Field::Heading.index(), 2);
        assert_eq!(Field::Url.index(), 3);
    }

    #[test]
    fn counts_start_at_zero_and_accumulate() {
        let mut counts = FieldCounts::default();
        assert!(counts.is_empty());
        counts.add(Field::Body, 3);
        counts.add(Field::Body, 2);
        assert_eq!(counts.get(Field::Body), 5);
        assert_eq!(counts.total(), 5);
    }

    #[test]
    fn the_mask_marks_exactly_the_non_zero_fields() {
        let mut counts = FieldCounts::default();
        assert_eq!(counts.mask(), 0);
        counts.set(Field::Title, 1);
        assert_eq!(counts.mask(), 1 << Field::Title.index());
        counts.set(Field::Url, 2);
        assert_eq!(
            counts.mask(),
            (1 << Field::Title.index()) | (1 << Field::Url.index())
        );
    }

    #[test]
    fn counts_saturate_rather_than_overflow() {
        let mut counts = FieldCounts::default();
        counts.set(Field::Body, u32::MAX);
        counts.add(Field::Body, 10);
        assert_eq!(counts.get(Field::Body), u32::MAX);
        assert_eq!(counts.total(), u32::MAX);
    }

    #[test]
    fn default_weights_rank_title_above_body() {
        let weights = FieldWeights::default();
        assert!(weights.get(Field::Title) > weights.get(Field::Heading));
        assert!(weights.get(Field::Heading) > weights.get(Field::Url));
        assert!(weights.get(Field::Url) > weights.get(Field::Body));
    }

    #[test]
    fn field_names_are_stable_for_score_breakdowns() {
        assert_eq!(Field::Body.name(), "body");
        assert_eq!(Field::Title.name(), "title");
    }
}
