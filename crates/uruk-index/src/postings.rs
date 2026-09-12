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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    /// Document id within this segment.
    pub doc: u32,
    /// How often the term appears, per field. `BM25F` combines these rather
    /// than scoring each field separately (`RESEARCH.md` §2.4).
    pub counts: FieldCounts,
    /// Where the term appears in the body, in token positions.
    ///
    /// Body only. Phrase and proximity matching are body concerns, and storing
    /// positions for every field would roughly double the largest component of
    /// the index for a feature nothing uses.
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
/// The list must be sorted by document id; delta encoding depends on it, and
/// so does the skipping a query does at read time.
///
/// # Panics
///
/// Debug builds assert the ordering, because an unsorted list produces an
/// index that decodes without error and returns wrong answers, which is far
/// worse than a crash.
pub fn encode(postings: &[Posting], out: &mut Vec<u8>) {
    debug_assert!(
        postings.windows(2).all(|pair| pair[0].doc < pair[1].doc),
        "posting lists must be sorted by document id and free of duplicates"
    );

    write_varint(out, postings.len() as u64);
    let mut previous_doc = 0u32;

    for posting in postings {
        write_varint(out, u64::from(posting.doc - previous_doc));
        previous_doc = posting.doc;

        // A bitmask of which fields are non-zero, so a term that appears only
        // in the body costs one mask byte and one count rather than four
        // counts, three of which are zero.
        let mask = posting.counts.mask();
        out.push(mask);
        for field in 0..FIELD_COUNT {
            if mask & (1 << field) != 0 {
                write_varint(out, u64::from(posting.counts.get_index(field)));
            }
        }

        write_varint(out, posting.positions.len() as u64);
        let mut previous_position = 0u32;
        for &position in &posting.positions {
            write_varint(out, u64::from(position - previous_position));
            previous_position = position;
        }
    }
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
    // A corrupt length must not make us allocate gigabytes; the list cannot be
    // longer than one posting per remaining byte.
    if count > input.len().saturating_sub(*cursor) + 1 {
        return Err(fail(*cursor, "length exceeds the remaining bytes"));
    }

    let mut postings = Vec::with_capacity(count);
    let mut doc = 0u32;

    for _ in 0..count {
        let delta = read_varint(input, cursor).ok_or_else(|| fail(*cursor, "truncated doc gap"))?;
        let delta = u32::try_from(delta).map_err(|_| fail(*cursor, "doc gap out of range"))?;
        doc = doc
            .checked_add(delta)
            .ok_or_else(|| fail(*cursor, "doc id overflow"))?;

        let mask = *input
            .get(*cursor)
            .ok_or_else(|| fail(*cursor, "truncated field mask"))?;
        *cursor += 1;

        let mut counts = FieldCounts::default();
        for field in 0..FIELD_COUNT {
            if mask & (1 << field) != 0 {
                let value =
                    read_varint(input, cursor).ok_or_else(|| fail(*cursor, "truncated count"))?;
                let value =
                    u32::try_from(value).map_err(|_| fail(*cursor, "count out of range"))?;
                counts.set_index(field, value);
            }
        }

        let positions_len =
            read_varint(input, cursor).ok_or_else(|| fail(*cursor, "truncated position count"))?;
        let positions_len =
            usize::try_from(positions_len).map_err(|_| fail(*cursor, "implausible positions"))?;
        if positions_len > input.len().saturating_sub(*cursor) + 1 {
            return Err(fail(*cursor, "positions exceed the remaining bytes"));
        }

        let mut positions = Vec::with_capacity(positions_len);
        let mut position = 0u32;
        for _ in 0..positions_len {
            let gap = read_varint(input, cursor)
                .ok_or_else(|| fail(*cursor, "truncated position gap"))?;
            let gap = u32::try_from(gap).map_err(|_| fail(*cursor, "position gap out of range"))?;
            position = position
                .checked_add(gap)
                .ok_or_else(|| fail(*cursor, "position overflow"))?;
            positions.push(position);
        }

        postings.push(Posting {
            doc,
            counts,
            positions,
        });
    }
    Ok(postings)
}

#[cfg(test)]
mod tests {
    use super::{Posting, decode, encode, read_varint, write_varint};
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

    fn sample() -> Vec<Posting> {
        let mut first = Posting::new(3);
        first.counts.set(Field::Body, 4);
        first.counts.set(Field::Title, 1);
        first.positions = vec![0, 7, 19, 400];

        let mut second = Posting::new(1_000_004);
        second.counts.set(Field::Body, 1);
        second.positions = vec![12];

        let mut third = Posting::new(1_000_005);
        third.counts.set(Field::Url, 1);

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

        let mut all_fields = Posting::new(0);
        for field in Field::ALL {
            all_fields.counts.set(field, 1);
        }

        let (mut lean, mut fat) = (Vec::new(), Vec::new());
        encode(std::slice::from_ref(&body_only), &mut lean);
        encode(std::slice::from_ref(&all_fields), &mut fat);
        assert!(
            lean.len() < fat.len(),
            "the field mask is not saving anything"
        );
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
    fn a_wild_length_prefix_does_not_allocate() {
        // A corrupt length must not be believed. u64::MAX postings cannot fit
        // in four bytes of input.
        let mut buffer = Vec::new();
        write_varint(&mut buffer, u64::MAX);
        let mut cursor = 0;
        assert!(decode(&buffer, &mut cursor).is_err());
    }
}
