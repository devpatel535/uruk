//! Posting lists and the bytes they are stored as.
//!
//! A **posting list** is what hangs off each term in the index: the documents
//! that contain it, how often, and where. It is the bulk of the index, so how
//! it is encoded decides how big the index is.
//!
//! Two classic techniques, both from `RESEARCH.md` §2.3:
//!
//! **Delta encoding.** Document ids in a list are sorted, so store the gaps
//! rather than the numbers. `1000003, 1000009, 1000011` becomes `1000003, 6,
//! 2`. Positions within a document get the same treatment.
//!
//! **Variable-byte encoding.** Small numbers should cost one byte. Each byte
//! carries seven bits of payload and one continuation bit, so anything under
//! 128 is a single byte and the gaps produced by delta encoding almost always
//! are.
//!
//! Together these are the baseline the brief asks Phase 5 to benchmark
//! Simple-9, `PForDelta` and Elias-Fano against — which is exactly why this is
//! written out by hand rather than handed to a compression crate. Measuring
//! alternatives needs a baseline we control and can decode instrumentally.

use crate::codec::{Codec, PForDelta};
use crate::fields::{FIELD_COUNT, FieldCounts};

/// Append `value` in variable-byte form.
///
/// Seven payload bits per byte, high bit set on every byte but the last.
pub fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        // Truncation is the point: we are emitting the low seven bits.
        #[expect(clippy::cast_possible_truncation, reason = "masked to 7 bits")]
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
    #[expect(clippy::cast_possible_truncation, reason = "value < 0x80 here")]
    out.push(value as u8);
}

/// Read a variable-byte integer, advancing `cursor`.
///
/// Returns `None` on a truncated or overlong encoding rather than panicking:
/// this parses bytes from disk, which may be corrupt.
pub fn read_varint(input: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *input.get(*cursor)?;
        *cursor += 1;
        // 10 groups of 7 bits is 70, so the tenth byte must not shift past 63.
        if shift >= 64 {
            return None;
        }
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
}

/// One document's entry in a term's posting list.
///
/// # Invariant
///
/// `positions.len() == counts.total()`. Every occurrence counted in a field
/// contributes exactly one position, so storing the number of positions
/// separately would be storing the same number twice. [`encode`] relies on
/// this to leave it out, which measurement showed is worth about a byte per
/// posting — roughly a seventh of the whole postings section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    /// Document id within this segment.
    pub doc: u32,
    /// How often the term appears, per field. `BM25F` combines these rather
    /// than scoring each field separately (`RESEARCH.md` §2.4).
    pub counts: FieldCounts,
    /// Where the term appears, in token positions.
    ///
    /// Fields share one position space, separated by a gap wide enough that no
    /// phrase can straddle two of them. That is what lets `"clay tablets"`
    /// match a page *titled* "Clay tablets" — keeping positions for the body
    /// alone would have made a quoted title unsearchable, which is not a
    /// limitation the brief's promise about phrase search survives.
    ///
    /// The extra cost is small: a title, its headings and a URL come to a few
    /// dozen tokens against a body of hundreds.
    pub positions: Vec<u32>,
}

impl Posting {
    pub fn new(doc: u32) -> Self {
        Self {
            doc,
            counts: FieldCounts::default(),
            positions: Vec::new(),
        }
    }

    /// Total occurrences across all fields.
    pub fn total(&self) -> u32 {
        self.counts.total()
    }
}

/// Encode a term's postings.
///
/// # Layout
///
/// Four streams rather than one interleaved record per document. Measurement
/// (`RESEARCH.md` §6) showed why: block codecs need long runs of similar
/// values to pay off, and interleaving a document gap, a frequency and two
/// positions gives them runs of one.
///
/// ```text
/// varint  document count
/// stream  document-id gaps          (PForDelta blocks, varint tail)
/// stream  (frequency << 1) | flag   (PForDelta blocks, varint tail)
/// bytes   field detail, only for the postings whose flag is set
/// stream  position gaps             (PForDelta blocks, varint tail)
/// ```
///
/// Two things are deliberately *not* stored, because they can be recovered:
///
/// - the number of positions, which is the total frequency;
/// - the body field's count, which is the total minus the other fields'.
///
/// And the field mask is written only for the minority of postings that touch
/// a field other than the body. Together these remove the fixed per-posting
/// bytes that the measurement identified as the largest single cost.
///
/// The list must be sorted by document id; delta encoding depends on it, and
/// so does the skipping a query does at read time.
///
/// # Panics
///
/// Debug builds assert the ordering and the position invariant. An unsorted
/// list, or one whose positions disagree with its counts, produces an index
/// that decodes without error and returns wrong answers, which is far worse
/// than a crash.
pub fn encode(postings: &[Posting], out: &mut Vec<u8>) {
    debug_assert!(
        postings.windows(2).all(|pair| pair[0].doc < pair[1].doc),
        "posting lists must be sorted by document id and free of duplicates"
    );
    debug_assert!(
        postings
            .iter()
            .all(|posting| posting.positions.len() as u64 == u64::from(posting.counts.total())),
        "a posting's position count must equal the sum of its field counts"
    );

    write_varint(out, postings.len() as u64);
    if postings.is_empty() {
        return;
    }

    // --- document-id gaps ---
    let mut gaps = Vec::with_capacity(postings.len());
    let mut previous = 0u32;
    for posting in postings {
        gaps.push(posting.doc - previous);
        previous = posting.doc;
    }
    STREAM.encode(&gaps, out);

    // --- frequencies, with a flag bit for "has a field other than the body" ---
    let mut frequencies = Vec::with_capacity(postings.len());
    for posting in postings {
        let flag = u32::from(other_fields_mask(posting.counts) != 0);
        frequencies.push((posting.counts.total() << 1) | flag);
    }
    STREAM.encode(&frequencies, out);

    // --- field detail for the minority that need it ---
    for posting in postings {
        let mask = other_fields_mask(posting.counts);
        if mask == 0 {
            continue;
        }
        out.push(mask);
        for field in 1..FIELD_COUNT {
            if mask & (1 << field) != 0 {
                write_varint(out, u64::from(posting.counts.get_index(field)));
            }
        }
    }

    // --- positions, one stream across every document ---
    let total: usize = postings.iter().map(|posting| posting.positions.len()).sum();
    let mut deltas = Vec::with_capacity(total);
    for posting in postings {
        // Reset at each document boundary, so gaps stay small rather than
        // carrying a document's whole length across the join.
        let mut previous = 0u32;
        for &position in &posting.positions {
            deltas.push(position - previous);
            previous = position;
        }
    }
    STREAM.encode(&deltas, out);
}

/// The codec every stream uses.
///
/// `PForDelta` measured best on document-id gaps and no worse than
/// variable-byte anywhere, and it falls back to variable-byte for any tail
/// shorter than a block — which is most terms.
const STREAM: PForDelta = PForDelta;

/// Which fields other than the body this posting touches.
///
/// The body is excluded because its count is recoverable: it is the total
/// frequency minus everything else.
fn other_fields_mask(counts: FieldCounts) -> u8 {
    // `Field::Body` is index 0 by construction, pinned by a test in `fields`.
    counts.mask() & !1
}

#[derive(Debug, thiserror::Error)]
#[error("posting list is corrupt at byte {offset}: {reason}")]
pub struct DecodeError {
    pub offset: usize,
    pub reason: &'static str,
}

/// Decode a term's postings from `input`, starting at `cursor`.
pub fn decode(input: &[u8], cursor: &mut usize) -> Result<Vec<Posting>, DecodeError> {
    let fail = |cursor: usize, reason| DecodeError {
        offset: cursor,
        reason,
    };

    let count = read_varint(input, cursor).ok_or_else(|| fail(*cursor, "truncated length"))?;
    let count = usize::try_from(count).map_err(|_| fail(*cursor, "implausible length"))?;
    if count == 0 {
        return Ok(Vec::new());
    }
    // A corrupt length must not make us allocate gigabytes.
    if count > input.len().saturating_sub(*cursor) + 1 {
        return Err(fail(*cursor, "length exceeds the remaining bytes"));
    }

    let gaps = STREAM
        .decode(input, cursor, count)
        .ok_or_else(|| fail(*cursor, "truncated document gaps"))?;
    let frequencies = STREAM
        .decode(input, cursor, count)
        .ok_or_else(|| fail(*cursor, "truncated frequencies"))?;

    let mut postings = Vec::with_capacity(count);
    let mut wanted = Vec::with_capacity(count);
    let mut doc = 0u32;
    let mut positions_expected = 0usize;

    // Nothing here may allocate in proportion to a decoded value: `total` comes
    // off the disk and a corrupt segment can claim four billion positions for a
    // single document. The position vectors are sized after the bound check
    // below, once the claim has been weighed against the bytes that remain.
    for (&gap, &packed) in gaps.iter().zip(&frequencies) {
        doc = doc
            .checked_add(gap)
            .ok_or_else(|| fail(*cursor, "document id overflow"))?;
        let total = packed >> 1;
        let has_other_fields = packed & 1 == 1;

        let mut counts = FieldCounts::default();
        if has_other_fields {
            let mask = *input
                .get(*cursor)
                .ok_or_else(|| fail(*cursor, "truncated field mask"))?;
            *cursor += 1;
            let mut others = 0u32;
            for field in 1..FIELD_COUNT {
                if mask & (1 << field) != 0 {
                    let value = read_varint(input, cursor)
                        .ok_or_else(|| fail(*cursor, "truncated field count"))?;
                    let value =
                        u32::try_from(value).map_err(|_| fail(*cursor, "count out of range"))?;
                    counts.set_index(field, value);
                    others = others
                        .checked_add(value)
                        .ok_or_else(|| fail(*cursor, "field counts overflow"))?;
                }
            }
            // The body's count is whatever the other fields did not account for.
            let body = total
                .checked_sub(others)
                .ok_or_else(|| fail(*cursor, "field counts exceed the total frequency"))?;
            counts.set_index(0, body);
        } else {
            counts.set_index(0, total);
        }

        let count = usize::try_from(total).map_err(|_| fail(*cursor, "frequency too large"))?;
        positions_expected = positions_expected.saturating_add(count);
        wanted.push(count);
        postings.push(Posting {
            doc,
            counts,
            positions: Vec::new(),
        });
    }

    // A position cannot cost less than a bit, so a list claiming more positions
    // than the remaining bytes could hold is corrupt. The slack of one byte
    // covers a stream that is entirely block headers.
    if positions_expected > input.len().saturating_sub(*cursor).saturating_mul(8) + 8 {
        return Err(fail(*cursor, "positions exceed the remaining bytes"));
    }
    let deltas = STREAM
        .decode(input, cursor, positions_expected)
        .ok_or_else(|| fail(*cursor, "truncated positions"))?;

    let mut at = 0usize;
    for (posting, &count) in postings.iter_mut().zip(&wanted) {
        let slice = deltas
            .get(at..at + count)
            .ok_or_else(|| fail(*cursor, "short positions"))?;
        posting.positions.reserve_exact(count);
        let mut position = 0u32;
        for &delta in slice {
            position = position
                .checked_add(delta)
                .ok_or_else(|| fail(*cursor, "position overflow"))?;
            posting.positions.push(position);
        }
        at += count;
    }
    Ok(postings)
}

#[cfg(test)]
mod tests {
    use super::{Codec, Posting, STREAM, decode, encode, read_varint, write_varint};
    use crate::fields::Field;

    fn round_trip_varint(value: u64) {
        let mut buffer = Vec::new();
        write_varint(&mut buffer, value);
        let mut cursor = 0;
        assert_eq!(
            read_varint(&buffer, &mut cursor),
            Some(value),
            "value {value}"
        );
        assert_eq!(
            cursor,
            buffer.len(),
            "decoder did not consume exactly the written bytes"
        );
    }

    #[test]
    fn varints_round_trip_across_the_range() {
        for value in [
            0,
            1,
            127,
            128,
            129,
            255,
            256,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            round_trip_varint(value);
        }
    }

    #[test]
    fn small_numbers_cost_one_byte() {
        // The whole reason for delta encoding: gaps are small, so they are cheap.
        let mut buffer = Vec::new();
        for value in 0..128u64 {
            write_varint(&mut buffer, value);
        }
        assert_eq!(buffer.len(), 128);
    }

    #[test]
    fn a_truncated_varint_is_an_error_not_a_panic() {
        // 0x80 means "another byte follows", and there isn't one.
        let mut cursor = 0;
        assert_eq!(read_varint(&[0x80], &mut cursor), None);
    }

    #[test]
    fn an_overlong_varint_is_rejected() {
        // Eleven continuation bytes would shift past the width of a u64.
        let bytes = [0x80u8; 12];
        let mut cursor = 0;
        assert_eq!(read_varint(&bytes, &mut cursor), None);
    }

    /// Postings obeying the invariant: one position per counted occurrence.
    fn sample() -> Vec<Posting> {
        let mut first = Posting::new(3);
        first.counts.set(Field::Body, 4);
        first.counts.set(Field::Title, 1);
        first.positions = vec![0, 7, 19, 400, 5_000];

        let mut second = Posting::new(1_000_004);
        second.counts.set(Field::Body, 1);
        second.positions = vec![12];

        let mut third = Posting::new(1_000_005);
        third.counts.set(Field::Url, 1);
        third.positions = vec![3_200];

        vec![first, second, third]
    }

    #[test]
    fn postings_round_trip() {
        let postings = sample();
        let mut buffer = Vec::new();
        encode(&postings, &mut buffer);

        let mut cursor = 0;
        assert_eq!(decode(&buffer, &mut cursor).unwrap(), postings);
        assert_eq!(cursor, buffer.len());
    }

    #[test]
    fn an_empty_list_round_trips() {
        let mut buffer = Vec::new();
        encode(&[], &mut buffer);
        let mut cursor = 0;
        assert_eq!(decode(&buffer, &mut cursor).unwrap(), Vec::new());
    }

    #[test]
    fn several_lists_can_be_concatenated_and_read_back_in_order() {
        // This is how the segment stores them: one after another, with the
        // dictionary holding each list's offset.
        let mut buffer = Vec::new();
        let first = sample();
        let second = vec![Posting::new(9)];
        let offset_of_second = {
            encode(&first, &mut buffer);
            buffer.len()
        };
        encode(&second, &mut buffer);

        let mut cursor = offset_of_second;
        assert_eq!(decode(&buffer, &mut cursor).unwrap(), second);
    }

    #[test]
    fn absent_fields_cost_nothing() {
        // A term only in the body should not pay for three zero counts.
        let mut body_only = Posting::new(0);
        body_only.counts.set(Field::Body, 1);
        body_only.positions = vec![4];

        let mut all_fields = Posting::new(0);
        for field in Field::ALL {
            all_fields.counts.set(field, 1);
        }
        // One position per counted occurrence, as the invariant requires.
        all_fields.positions = vec![4, 1_004, 2_004, 3_004];

        let (mut lean, mut fat) = (Vec::new(), Vec::new());
        encode(std::slice::from_ref(&body_only), &mut lean);
        encode(std::slice::from_ref(&all_fields), &mut fat);
        assert!(
            lean.len() < fat.len(),
            "the field mask is not saving anything"
        );
    }

    #[test]
    fn a_body_only_posting_costs_nothing_it_does_not_have_to() {
        // The floor for one posting: a document gap, a frequency, a position.
        // Everything else is derived — the number of positions is the
        // frequency, and the body's count is the frequency minus the other
        // fields, of which there are none here. No mask byte, no count byte.
        let mut one = Posting::new(0);
        one.counts.set(Field::Body, 1);
        one.positions = vec![7];

        let mut buffer = Vec::new();
        encode(std::slice::from_ref(&one), &mut buffer);
        // count(1) + docgap(1) + frequency(1) + position(1) = 4.
        assert_eq!(buffer.len(), 4, "unexpected encoding: {buffer:?}");

        let mut cursor = 0;
        assert_eq!(decode(&buffer, &mut cursor).unwrap(), vec![one]);
    }

    #[test]
    fn large_gaps_still_round_trip() {
        let postings = vec![Posting::new(0), Posting::new(u32::MAX)];
        let mut buffer = Vec::new();
        encode(&postings, &mut buffer);
        let mut cursor = 0;
        assert_eq!(decode(&buffer, &mut cursor).unwrap(), postings);
    }

    #[test]
    fn corrupt_input_is_reported_rather_than_trusted() {
        // Every truncation of a valid list must be an error, never a panic and
        // never a plausible-looking wrong answer.
        let mut buffer = Vec::new();
        encode(&sample(), &mut buffer);

        for cut in 1..buffer.len() {
            let mut cursor = 0;
            let truncated = &buffer[..cut];
            if let Ok(decoded) = decode(truncated, &mut cursor) {
                assert_ne!(
                    decoded,
                    sample(),
                    "a truncated list decoded as the full one"
                );
            }
        }
    }

    #[test]
    fn a_wild_frequency_does_not_allocate() {
        // The length prefix is not the only number from disk that sizes an
        // allocation. A posting claiming four billion positions must be
        // rejected against the bytes that are actually there, before any vector
        // is sized from it.
        let mut buffer = Vec::new();
        write_varint(&mut buffer, 1); // one posting
        STREAM.encode(&[0], &mut buffer); // document 0
        // The low bit is the "has other fields" flag, so this is a frequency of
        // just under two billion, with no field detail to follow.
        STREAM.encode(&[u32::MAX - 1], &mut buffer);

        let mut cursor = 0;
        let error = decode(&buffer, &mut cursor).expect_err("an absurd frequency was believed");
        assert_eq!(error.reason, "positions exceed the remaining bytes");
    }

    #[test]
    fn a_long_list_costs_far_less_per_posting_than_a_short_one() {
        // The point of the four-stream layout: with a term's positions gathered
        // into one stream, the blocks fill and the per-posting cost collapses.
        // A single posting cannot do better than a few bytes; a thousand of
        // them should average well under two.
        let postings: Vec<Posting> = (0..1_000u32)
            .map(|doc| {
                let mut posting = Posting::new(doc * 3);
                posting.counts.set(Field::Body, 2);
                posting.positions = vec![doc % 700, doc % 700 + 40];
                posting
            })
            .collect();

        let mut buffer = Vec::new();
        encode(&postings, &mut buffer);
        let per_posting = buffer.len() as f64 / postings.len() as f64;
        assert!(
            per_posting < 4.0,
            "{per_posting:.2} bytes per posting: the streams are not filling blocks"
        );

        let mut cursor = 0;
        assert_eq!(decode(&buffer, &mut cursor).unwrap(), postings);
    }

    #[test]
    fn a_wild_length_prefix_does_not_allocate() {
        // A corrupt length must not be believed. u64::MAX postings cannot fit
        // in four bytes of input.
        let mut buffer = Vec::new();
        write_varint(&mut buffer, u64::MAX);
        let mut cursor = 0;
        assert!(decode(&buffer, &mut cursor).is_err());
    }
}
