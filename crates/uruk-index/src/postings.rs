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
/// varint  position group count
/// varint  byte length of each group, one per group
/// stream  position gaps, one independent stream per group
/// ```
///
/// # Why the positions are grouped
///
/// A position stream is sequential: to read one document's positions you must
/// decode every position before it. That made the query path decode an entire
/// term's positions to score the proximity of a hundred documents, which
/// measurement put at half the cost of the worst query (`RESEARCH.md` §6b).
///
/// Cutting the stream into independent groups of [`POSITION_GROUP`] documents
/// and writing each group's byte length lets a reader jump to the group it
/// wants. The cost is one varint per group — about two bytes per 128
/// documents, plus whatever the `PForDelta` blocks lose by restarting at a
/// group boundary. At ~72 positions per document a group holds some nine
/// thousand values, so the blocks still fill.
///
/// The group table is written *before* the groups rather than after, so a
/// reader that wants one group reads the head of the list and then seeks,
/// rather than reading to the end to find out where things are.
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

    // --- positions, in independently decodable groups ---
    let mut groups: Vec<Vec<u8>> = Vec::new();
    let mut deltas = Vec::new();
    for chunk in postings.chunks(POSITION_GROUP) {
        deltas.clear();
        for posting in chunk {
            // Reset at each document boundary, so gaps stay small rather than
            // carrying a document's whole length across the join.
            let mut previous = 0u32;
            for &position in &posting.positions {
                deltas.push(position - previous);
                previous = position;
            }
        }
        let mut encoded = Vec::new();
        STREAM.encode(&deltas, &mut encoded);
        groups.push(encoded);
    }

    write_varint(out, groups.len() as u64);
    for group in &groups {
        write_varint(out, group.len() as u64);
    }
    for group in &groups {
        out.extend_from_slice(group);
    }
}

/// Documents per independently decodable group of positions.
///
/// A hundred and twenty-eight, matching the `PForDelta` block size, so a group
/// is a whole number of blocks' worth of documents even though it is not a
/// whole number of blocks' worth of values.
///
/// The trade is between the table's size and how much a reader must decode to
/// reach one document: a group of 128 documents costs about two bytes of
/// table, and reading one document's positions decodes at most 127 others'.
/// Smaller groups would sharpen that and cost more table; larger ones the
/// reverse. This has not been tuned, because the win over decoding the whole
/// stream is already an order of magnitude and the next order would need a
/// workload to tune against.
pub const POSITION_GROUP: usize = 128;

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

/// A posting with its positions left unread.
///
/// # Why this exists
///
/// Positions are the largest part of the index and the slowest part to decode
/// — measured, on a 100,000-document corpus, at most of the time a query
/// spends. And a great many queries cannot use them:
///
/// - a **single-term** query has no proximity to compute and no phrase to
///   check, so its positions are decoded and then discarded;
/// - an **excluded** term (`-barley`) contributes nothing but a set of
///   document ids, whatever its positions say.
///
/// The four-stream layout makes skipping them free rather than merely
/// possible: positions are one contiguous stream at the end of a term's
/// record, so not reading them is a matter of stopping early. No seeking, and
/// no format change.
///
/// Kept as a separate type rather than a `Posting` with an empty `positions`
/// because the two are not interchangeable — `Posting`'s invariant is that its
/// positions match its counts, and a `Posting` that silently had none would
/// make every phrase query quietly return nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocPosting {
    pub doc: u32,
    pub counts: FieldCounts,
}

#[derive(Debug, thiserror::Error)]
#[error("posting list is corrupt at byte {offset}: {reason}")]
pub struct DecodeError {
    pub offset: usize,
    pub reason: &'static str,
}

fn fail(cursor: usize, reason: &'static str) -> DecodeError {
    DecodeError {
        offset: cursor,
        reason,
    }
}

/// Read the document, frequency and field-detail streams.
///
/// Everything a posting has except its positions, which are the last stream
/// and can be left unread. Returns the postings and, alongside each, how many
/// positions it has — which the caller needs to slice the position stream and
/// which is exactly the frequency, so it is never stored.
fn decode_heads(
    input: &[u8],
    cursor: &mut usize,
) -> Result<(Vec<DocPosting>, Vec<usize>), DecodeError> {
    let count = read_varint(input, cursor).ok_or_else(|| fail(*cursor, "truncated length"))?;
    let count = usize::try_from(count).map_err(|_| fail(*cursor, "implausible length"))?;
    if count == 0 {
        return Ok((Vec::new(), Vec::new()));
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

        wanted.push(usize::try_from(total).map_err(|_| fail(*cursor, "frequency too large"))?);
        postings.push(DocPosting { doc, counts });
    }

    Ok((postings, wanted))
}

/// Decode a term's postings, positions included.
pub fn decode(input: &[u8], cursor: &mut usize) -> Result<Vec<Posting>, DecodeError> {
    let list = decode_list(input, cursor)?;
    let mut postings = Vec::with_capacity(list.docs.len());
    for (index, head) in list.docs.iter().enumerate() {
        postings.push(Posting {
            doc: head.doc,
            counts: head.counts,
            positions: list.positions(index)?,
        });
    }
    Ok(postings)
}

/// A term's postings with the positions left on the page.
///
/// The documents and their field counts are decoded up front, because every
/// query needs them. The positions are not: the group table is read so that
/// any one group can be found, and a group is decoded only when something asks
/// for a document inside it.
///
/// This is what lets the query path score proximity for a hundred candidates
/// out of eighty thousand without decoding eighty thousand documents' worth of
/// positions (`RESEARCH.md` §6b).
#[derive(Debug, Clone, Default)]
pub struct PostingList {
    /// The bytes of the whole list, as read from the segment.
    bytes: Vec<u8>,
    docs: Vec<DocPosting>,
    /// Positions per document, which is its total frequency.
    counts: Vec<usize>,
    /// Byte range of each group's position stream within `bytes`.
    groups: Vec<(usize, usize)>,
}

impl PostingList {
    pub fn docs(&self) -> &[DocPosting] {
        &self.docs
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// One document's positions, decoding only the group it falls in.
    pub fn positions(&self, index: usize) -> Result<Vec<u32>, DecodeError> {
        let mut found = self.positions_of(&[index])?;
        Ok(found.pop().unwrap_or_default())
    }

    /// Positions for several documents, decoding each group at most once.
    ///
    /// `indices` must be sorted ascending and within range; the walk over
    /// groups depends on it, and the caller always has them in order because
    /// they come from a posting list.
    ///
    /// # Panics
    ///
    /// Debug builds assert the ordering. Unsorted input would silently decode
    /// some groups repeatedly and return the right answer slowly, which is the
    /// kind of bug that survives for years.
    pub fn positions_of(&self, indices: &[usize]) -> Result<Vec<Vec<u32>>, DecodeError> {
        debug_assert!(
            indices.windows(2).all(|pair| pair[0] <= pair[1]),
            "positions_of needs its indices in ascending order"
        );

        let mut out = Vec::with_capacity(indices.len());
        let mut decoded_group = usize::MAX;
        let mut values: Vec<u32> = Vec::new();

        for &index in indices {
            if index >= self.docs.len() {
                return Err(fail(0, "position asked for a document past the list"));
            }
            let group = index / POSITION_GROUP;
            if group != decoded_group {
                values = self.decode_group(group)?;
                decoded_group = group;
            }

            // Where this document's positions start inside its group.
            let first = group * POSITION_GROUP;
            let skip: usize = self.counts[first..index].iter().sum();
            let count = self.counts[index];
            let slice = values
                .get(skip..skip + count)
                .ok_or_else(|| fail(0, "a position group is shorter than its documents"))?;

            let mut positions = Vec::with_capacity(count);
            let mut position = 0u32;
            for &delta in slice {
                position = position
                    .checked_add(delta)
                    .ok_or_else(|| fail(0, "position overflow"))?;
                positions.push(position);
            }
            out.push(positions);
        }
        Ok(out)
    }

    /// Every document's positions, in order.
    pub fn all_positions(&self) -> Result<Vec<Vec<u32>>, DecodeError> {
        self.positions_of(&(0..self.docs.len()).collect::<Vec<_>>())
    }

    fn decode_group(&self, group: usize) -> Result<Vec<u32>, DecodeError> {
        let &(start, end) = self
            .groups
            .get(group)
            .ok_or_else(|| fail(0, "position group out of range"))?;
        let first = group * POSITION_GROUP;
        let last = (first + POSITION_GROUP).min(self.counts.len());
        let expected: usize = self.counts[first..last].iter().sum();

        let bytes = self
            .bytes
            .get(start..end)
            .ok_or_else(|| fail(start, "position group runs past the list"))?;
        // A position cannot cost less than a bit, so a group claiming more
        // positions than its own bytes could hold is corrupt. Checked before
        // anything is sized from it: these counts came off a disk that may be
        // lying. The slack covers a stream that is entirely block headers.
        if expected > bytes.len().saturating_mul(8) + 8 {
            return Err(fail(start, "positions exceed the group's bytes"));
        }
        let mut cursor = 0;
        STREAM
            .decode(bytes, &mut cursor, expected)
            .ok_or_else(|| fail(start, "truncated positions"))
    }
}

/// Decode a term's postings, reading the group table but no positions.
pub fn decode_list(input: &[u8], cursor: &mut usize) -> Result<PostingList, DecodeError> {
    let (docs, counts) = decode_heads(input, cursor)?;
    if docs.is_empty() {
        return Ok(PostingList {
            bytes: Vec::new(),
            docs,
            counts,
            groups: Vec::new(),
        });
    }

    let expected_groups = docs.len().div_ceil(POSITION_GROUP);
    let group_count = read_varint(input, cursor)
        .ok_or_else(|| fail(*cursor, "truncated position group count"))?;
    if group_count != expected_groups as u64 {
        // The count is derivable from the document count, so a disagreement
        // means the bytes are not what they claim to be. Believing the stored
        // number instead would decode a plausible-looking wrong answer.
        return Err(fail(
            *cursor,
            "position group count does not match the documents",
        ));
    }

    let mut lengths = Vec::with_capacity(expected_groups);
    let mut total = 0usize;
    for _ in 0..expected_groups {
        let length = read_varint(input, cursor)
            .ok_or_else(|| fail(*cursor, "truncated position group length"))?;
        let length = usize::try_from(length).map_err(|_| fail(*cursor, "implausible group"))?;
        total = total.saturating_add(length);
        lengths.push(length);
    }
    if total > input.len().saturating_sub(*cursor) {
        return Err(fail(*cursor, "position groups exceed the remaining bytes"));
    }

    // The groups are held as one owned buffer so the list outlives the segment
    // read that produced it, with ranges into it rather than a vector of
    // vectors: one allocation instead of one per group.
    let start = *cursor;
    let bytes = input[start..start + total].to_vec();
    *cursor = start + total;

    let mut groups = Vec::with_capacity(lengths.len());
    let mut at = 0usize;
    for length in lengths {
        groups.push((at, at + length));
        at += length;
    }

    Ok(PostingList {
        bytes,
        docs,
        counts,
        groups,
    })
}

/// Decode a term's postings without reading the position stream.
///
/// For queries that cannot use positions: a single term has no proximity to
/// compute and no phrase to check, and an excluded term contributes only a set
/// of document ids. Positions are the largest and slowest part of the index,
/// so not reading them is the difference between a query that fits the brief's
/// latency budget and one that does not — see [`DocPosting`].
///
/// The cursor is left after the field-detail section rather than at the end of
/// the list. That is safe because the dictionary holds every term's starting
/// offset, so nothing reads these lists sequentially — but it does mean this
/// must not be used to walk a concatenation of lists.
pub fn decode_documents(input: &[u8], cursor: &mut usize) -> Result<Vec<DocPosting>, DecodeError> {
    decode_heads(input, cursor).map(|(postings, _)| postings)
}

#[cfg(test)]
mod tests {
    use super::{
        Codec, POSITION_GROUP, Posting, STREAM, decode, decode_list, encode, read_varint,
        write_varint,
    };
    use crate::fields::{FIELD_COUNT, Field};

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
        // A term only in the body should not pay for the other fields' zeros.
        let mut body_only = Posting::new(0);
        body_only.counts.set(Field::Body, 1);
        body_only.positions = vec![4];

        let mut all_fields = Posting::new(0);
        for field in Field::ALL {
            all_fields.counts.set(field, 1);
        }
        // One position per counted occurrence, as the invariant requires, one
        // per field. Derived from FIELD_COUNT so that adding a field does not
        // quietly turn this into a test of something else.
        all_fields.positions = (0..FIELD_COUNT)
            .map(|field| u32::try_from(field).expect("fits") * 1_000 + 4)
            .collect();

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
        // The floor for one posting: a document gap, a frequency, a position,
        // and the two-byte position group table. Everything else is derived —
        // the number of positions is the frequency, and the body's count is
        // the frequency minus the other fields, of which there are none here.
        // No mask byte, no count byte.
        //
        // The group table is what a single-document list pays for skipping:
        // one varint saying there is one group and one saying how long it is.
        // Two bytes on a list this small is proportionally a lot, and on a
        // real index it is about 0.2% — the table is per *list*, and the lists
        // that matter are the long ones it was added for.
        let mut one = Posting::new(0);
        one.counts.set(Field::Body, 1);
        one.positions = vec![7];

        let mut buffer = Vec::new();
        encode(std::slice::from_ref(&one), &mut buffer);
        // count(1) + docgap(1) + frequency(1) + groups(1) + length(1) + position(1) = 6.
        assert_eq!(buffer.len(), 6, "unexpected encoding: {buffer:?}");

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
        // A group table that is itself well formed: one group, four bytes. The
        // lie is entirely in the frequency, which is the number the reader
        // would otherwise size an allocation from.
        write_varint(&mut buffer, 1);
        write_varint(&mut buffer, 4);
        buffer.extend_from_slice(&[0, 0, 0, 0]);

        let mut cursor = 0;
        let error = decode(&buffer, &mut cursor).expect_err("an absurd frequency was believed");
        assert_eq!(error.reason, "positions exceed the group's bytes");
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
    fn reading_positions_lazily_agrees_with_reading_them_all() {
        // The whole point of the group table is that these two paths differ in
        // cost and not in answer. A list long enough to span several groups,
        // with varying frequencies so the group boundaries do not fall on
        // tidy offsets.
        let count = u32::try_from(POSITION_GROUP * 3 + 7).expect("fits");
        let postings: Vec<Posting> = (0..count)
            .map(|doc| {
                let mut posting = Posting::new(doc * 2);
                let count = 1 + (doc % 5);
                posting.counts.set(Field::Body, count);
                posting.positions = (0..count).map(|i| doc % 400 + i * 3).collect();
                posting
            })
            .collect();

        let mut buffer = Vec::new();
        encode(&postings, &mut buffer);

        let mut cursor = 0;
        let list = decode_list(&buffer, &mut cursor).expect("decodes");
        assert_eq!(cursor, buffer.len(), "the list did not consume its bytes");
        assert_eq!(list.len(), postings.len());

        // One at a time, out of the middle of a group.
        for index in [0, 1, POSITION_GROUP - 1, POSITION_GROUP, POSITION_GROUP + 5] {
            assert_eq!(
                list.positions(index).expect("positions"),
                postings[index].positions,
                "document {index}"
            );
        }

        // And the batch path, which walks each group once.
        let every: Vec<Vec<u32>> = list.all_positions().expect("all positions");
        let expected: Vec<Vec<u32>> = postings.iter().map(|p| p.positions.clone()).collect();
        assert_eq!(every, expected);
    }

    #[test]
    fn a_lazy_read_touches_only_the_group_it_needs() {
        // Not a timing test — a structural one. Corrupt every group but the
        // first, then read a document from the first group. If the reader is
        // decoding more than it was asked for, this fails.
        let count = u32::try_from(POSITION_GROUP * 2).expect("fits");
        let postings: Vec<Posting> = (0..count)
            .map(|doc| {
                let mut posting = Posting::new(doc);
                posting.counts.set(Field::Body, 2);
                posting.positions = vec![doc % 300, doc % 300 + 9];
                posting
            })
            .collect();

        let mut buffer = Vec::new();
        encode(&postings, &mut buffer);
        let mut cursor = 0;
        let list = decode_list(&buffer, &mut cursor).expect("decodes");
        assert_eq!(list.groups.len(), 2);

        // Scribble over the second group's bytes.
        let (start, end) = list.groups[1];
        let mut damaged = list.clone();
        for byte in &mut damaged.bytes[start..end] {
            *byte = 0xFF;
        }

        assert_eq!(
            damaged.positions(0).expect("the first group is untouched"),
            postings[0].positions
        );
    }

    #[test]
    fn a_group_table_that_disagrees_with_the_documents_is_refused() {
        // The number of groups follows from the number of documents, so a
        // stored count that disagrees means the bytes are not what they say
        // they are. Believing the stored number would decode a plausible
        // wrong answer.
        let mut buffer = Vec::new();
        write_varint(&mut buffer, 1);
        STREAM.encode(&[0], &mut buffer);
        STREAM.encode(&[2], &mut buffer); // frequency 1, no other fields
        write_varint(&mut buffer, 9); // nine groups for one document
        let mut cursor = 0;
        let error = decode(&buffer, &mut cursor).expect_err("a bad group count was believed");
        assert_eq!(
            error.reason,
            "position group count does not match the documents"
        );
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
