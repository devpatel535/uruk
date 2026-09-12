//! Integer codecs, so the index's size can be argued about with numbers.
//!
//! Everything expensive in an inverted index is a list of ascending integers —
//! document ids and positions — stored as the gaps between them. How those
//! gaps are encoded decides how big the index is and how fast a query reads
//! it, and the brief asks for the alternatives to be benchmarked rather than
//! chosen by reputation.
//!
//! Three schemes, in increasing order of how much they know about their input:
//!
//! - [`Varint`] treats each value alone: seven payload bits per byte. Simple,
//!   never worse than five bytes, and the current baseline.
//! - [`Simple9`] packs as many values as will fit into one 32-bit word, using
//!   the same width for all of them. Good when neighbouring gaps are similar,
//!   which in a posting list they usually are.
//! - [`BitPacked`] takes a whole block of 128 values, finds the widest, and
//!   packs every value at that width. This is what Lucene does, and it is the
//!   scheme the measurement in `RESEARCH.md` §6 pointed at: variable-byte
//!   rounds every value up to a byte, so a gap of 3 and a gap of 100 both cost
//!   eight bits where seven of them are wasted on the first.
//!
//! # What is measured
//!
//! `cargo run --release --example codec_bench -p uruk-index` reports size and
//! decode throughput for each, on gap distributions with the shape real
//! posting lists have. The brief asks for the trade-off, not a conclusion.

use crate::postings::{read_varint, write_varint};

/// How a sequence of gaps is turned into bytes and back.
///
/// The interface is deliberately whole-list rather than value-at-a-time:
/// block schemes cannot encode one value in isolation, and pretending
/// otherwise would make the comparison dishonest by forcing them into
/// variable-byte's shape.
pub trait Codec {
    /// Name, for benchmark output.
    fn name(&self) -> &'static str;

    /// Append `values` to `out`.
    fn encode(&self, values: &[u32], out: &mut Vec<u8>);

    /// Read exactly `count` values starting at `cursor`.
    ///
    /// Returns `None` on truncated or malformed input, which is the only
    /// honest response to bytes that came off a disk.
    fn decode(&self, input: &[u8], cursor: &mut usize, count: usize) -> Option<Vec<u32>>;
}

/// Variable-byte: seven payload bits per byte, high bit continues.
#[derive(Debug, Clone, Copy, Default)]
pub struct Varint;

impl Codec for Varint {
    fn name(&self) -> &'static str {
        "varint"
    }

    fn encode(&self, values: &[u32], out: &mut Vec<u8>) {
        for &value in values {
            write_varint(out, u64::from(value));
        }
    }

    fn decode(&self, input: &[u8], cursor: &mut usize, count: usize) -> Option<Vec<u32>> {
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(u32::try_from(read_varint(input, cursor)?).ok()?);
        }
        Some(out)
    }
}

/// The nine ways Simple-9 divides 28 payload bits: `(values, bits each)`.
///
/// The selector occupies the top four bits of the word, leaving 28. Every
/// layout wastes whatever 28 is not divisible by, which is the scheme's
/// well-known small inefficiency.
const SIMPLE9_LAYOUTS: [(usize, u32); 9] = [
    (28, 1),
    (14, 2),
    (9, 3),
    (7, 4),
    (5, 5),
    (4, 7),
    (3, 9),
    (2, 14),
    (1, 28),
];

/// Simple-9: as many equal-width values as fit in one 32-bit word.
///
/// Values wider than 28 bits cannot be packed at all, so they fall back to a
/// marker word followed by the raw value. Posting-list gaps are almost never
/// that large, but "almost never" is not a guarantee we can encode against.
#[derive(Debug, Clone, Copy, Default)]
pub struct Simple9;

/// Selector value meaning "the next four bytes are one raw u32".
const SIMPLE9_ESCAPE: u32 = 15;

impl Codec for Simple9 {
    fn name(&self) -> &'static str {
        "simple9"
    }

    fn encode(&self, values: &[u32], out: &mut Vec<u8>) {
        let mut at = 0usize;
        while at < values.len() {
            // A value too wide for any layout gets its own escape word.
            if values[at] >= 1 << 28 {
                out.extend_from_slice(&(SIMPLE9_ESCAPE << 28).to_le_bytes());
                out.extend_from_slice(&values[at].to_le_bytes());
                at += 1;
                continue;
            }

            // The best layout is the first that fits the most values, which is
            // why the table is ordered widest-count first.
            let mut chosen = SIMPLE9_LAYOUTS.len() - 1;
            for (selector, &(count, bits)) in SIMPLE9_LAYOUTS.iter().enumerate() {
                let available = values.len() - at;
                let take = count.min(available);
                if take == count || at + take == values.len() {
                    let fits = values[at..at + take]
                        .iter()
                        .all(|&v| bits_needed(v) <= bits);
                    if fits {
                        chosen = selector;
                        break;
                    }
                }
            }

            let (count, bits) = SIMPLE9_LAYOUTS[chosen];
            let take = count.min(values.len() - at);
            #[expect(clippy::cast_possible_truncation, reason = "chosen < 9")]
            let mut word = (chosen as u32) << 28;
            for (slot, &value) in values[at..at + take].iter().enumerate() {
                #[expect(clippy::cast_possible_truncation, reason = "slot < 28")]
                let shift = bits * slot as u32;
                word |= value << shift;
            }
            out.extend_from_slice(&word.to_le_bytes());
            at += take;
        }
    }

    fn decode(&self, input: &[u8], cursor: &mut usize, count: usize) -> Option<Vec<u32>> {
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            let word = read_u32(input, cursor)?;
            let selector = word >> 28;

            if selector == SIMPLE9_ESCAPE {
                out.push(read_u32(input, cursor)?);
                continue;
            }

            let &(slots, bits) = SIMPLE9_LAYOUTS.get(selector as usize)?;
            let mask = if bits >= 32 {
                u32::MAX
            } else {
                (1u32 << bits) - 1
            };
            for slot in 0..slots {
                if out.len() == count {
                    break;
                }
                #[expect(clippy::cast_possible_truncation, reason = "slot < 28")]
                let shift = bits * slot as u32;
                out.push((word >> shift) & mask);
            }
        }
        Some(out)
    }
}

/// Values per bit-packed block. 128 is Lucene's choice and a reasonable one:
/// big enough to amortise the width byte, small enough that one outlier only
/// widens its own block.
pub const BLOCK: usize = 128;

/// Frame of reference: whole blocks packed at the width the widest value needs.
///
/// A block costs one byte of width plus `128 × bits / 8`. Where variable-byte
/// spends eight bits on a gap of 3, this spends as many as the block's largest
/// gap actually requires — which for the long posting lists of common terms,
/// where gaps are small and similar, is the whole difference.
///
/// A trailing partial block falls back to variable-byte rather than padding to
/// 128, since padding would cost more than it saves on short lists.
#[derive(Debug, Clone, Copy, Default)]
pub struct BitPacked;

impl Codec for BitPacked {
    fn name(&self) -> &'static str {
        "bitpacked"
    }

    fn encode(&self, values: &[u32], out: &mut Vec<u8>) {
        let mut chunks = values.chunks_exact(BLOCK);
        for block in &mut chunks {
            let bits = block.iter().copied().map(bits_needed).max().unwrap_or(0);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "bits_needed returns 0..=32"
            )]
            out.push(bits as u8);
            if bits == 0 {
                // Every value is zero; the width alone says so.
                continue;
            }
            pack(block, bits, out);
        }
        for &value in chunks.remainder() {
            write_varint(out, u64::from(value));
        }
    }

    fn decode(&self, input: &[u8], cursor: &mut usize, count: usize) -> Option<Vec<u32>> {
        let mut out = Vec::with_capacity(count);
        let whole = count / BLOCK;

        for _ in 0..whole {
            let bits = u32::from(*input.get(*cursor)?);
            *cursor += 1;
            if bits == 0 {
                out.extend(std::iter::repeat_n(0u32, BLOCK));
                continue;
            }
            if bits > 32 {
                return None;
            }
            unpack(input, cursor, bits, &mut out)?;
        }
        for _ in 0..(count % BLOCK) {
            out.push(u32::try_from(read_varint(input, cursor)?).ok()?);
        }
        Some(out)
    }
}

/// Write `BLOCK` values at `bits` each, least-significant bit first.
fn pack(block: &[u32], bits: u32, out: &mut Vec<u8>) {
    let mut buffer = 0u64;
    let mut filled = 0u32;

    for &value in block {
        buffer |= u64::from(value) << filled;
        filled += bits;
        while filled >= 8 {
            #[expect(clippy::cast_possible_truncation, reason = "taking the low byte")]
            out.push(buffer as u8);
            buffer >>= 8;
            filled -= 8;
        }
    }
    if filled > 0 {
        #[expect(clippy::cast_possible_truncation, reason = "taking the low byte")]
        out.push(buffer as u8);
    }
}

/// Read `BLOCK` values of `bits` each, appending to `out`.
fn unpack(input: &[u8], cursor: &mut usize, bits: u32, out: &mut Vec<u32>) -> Option<()> {
    let mask = if bits >= 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    };
    let mut buffer = 0u64;
    let mut filled = 0u32;

    for _ in 0..BLOCK {
        while filled < bits {
            let byte = *input.get(*cursor)?;
            *cursor += 1;
            buffer |= u64::from(byte) << filled;
            filled += 8;
        }
        #[expect(clippy::cast_possible_truncation, reason = "masked to `bits`")]
        let value = (buffer as u32) & mask;
        out.push(value);
        buffer >>= bits;
        filled -= bits;
    }
    Some(())
}

/// Bits required to represent `value`. Zero needs none.
fn bits_needed(value: u32) -> u32 {
    32 - value.leading_zeros()
}

fn read_u32(input: &[u8], cursor: &mut usize) -> Option<u32> {
    let bytes = input.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Share of a block that `PForDelta` is willing to treat as exceptions.
///
/// The classic figure. Lower means a narrower packed width but more patching;
/// higher means the width is dragged up by outliers, which is the failure
/// [`BitPacked`] has and this is meant to fix.
const PFOR_EXCEPTION_SHARE: f64 = 0.10;

/// `PForDelta`: bit-pack most of a block, patch the rest.
///
/// [`BitPacked`] has to widen a whole block to fit its largest value, and gaps
/// drawn from anything like a geometric distribution reliably contain one. So
/// pick the width that covers most of the block, store the values that do not
/// fit separately as *exceptions*, and patch them back in on decode.
///
/// Per block: the packed width, the exception count, the packed low values,
/// then each exception as a position and a full value.
#[derive(Debug, Clone, Copy, Default)]
pub struct PForDelta;

impl Codec for PForDelta {
    fn name(&self) -> &'static str {
        "pfordelta"
    }

    fn encode(&self, values: &[u32], out: &mut Vec<u8>) {
        let mut chunks = values.chunks_exact(BLOCK);
        for block in &mut chunks {
            let bits = pfor_width(block);
            let exceptions: Vec<(usize, u32)> = block
                .iter()
                .enumerate()
                .filter(|(_, value)| bits_needed(**value) > bits)
                .map(|(at, &value)| (at, value))
                .collect();

            #[expect(
                clippy::cast_possible_truncation,
                reason = "bits_needed returns 0..=32"
            )]
            out.push(bits as u8);
            #[expect(clippy::cast_possible_truncation, reason = "at most BLOCK exceptions")]
            out.push(exceptions.len() as u8);

            if bits > 0 {
                // Exceptions are packed as zero and replaced on decode, which
                // keeps the packed area a fixed size and simple to skip.
                let masked: Vec<u32> = block
                    .iter()
                    .map(|&value| if bits_needed(value) > bits { 0 } else { value })
                    .collect();
                pack(&masked, bits, out);
            }
            for (at, value) in exceptions {
                #[expect(clippy::cast_possible_truncation, reason = "at < BLOCK <= 255")]
                out.push(at as u8);
                write_varint(out, u64::from(value));
            }
        }
        for &value in chunks.remainder() {
            write_varint(out, u64::from(value));
        }
    }

    fn decode(&self, input: &[u8], cursor: &mut usize, count: usize) -> Option<Vec<u32>> {
        let mut out = Vec::with_capacity(count);
        let whole = count / BLOCK;

        for _ in 0..whole {
            let bits = u32::from(*input.get(*cursor)?);
            *cursor += 1;
            let exceptions = usize::from(*input.get(*cursor)?);
            *cursor += 1;
            if bits > 32 || exceptions > BLOCK {
                return None;
            }

            let base = out.len();
            if bits == 0 {
                out.extend(std::iter::repeat_n(0u32, BLOCK));
            } else {
                unpack(input, cursor, bits, &mut out)?;
            }
            for _ in 0..exceptions {
                let at = usize::from(*input.get(*cursor)?);
                *cursor += 1;
                let value = u32::try_from(read_varint(input, cursor)?).ok()?;
                *out.get_mut(base + at)? = value;
            }
        }
        for _ in 0..(count % BLOCK) {
            out.push(u32::try_from(read_varint(input, cursor)?).ok()?);
        }
        Some(out)
    }
}

/// The narrowest width leaving no more than [`PFOR_EXCEPTION_SHARE`] of the
/// block as exceptions, charged for what the patching itself costs.
fn pfor_width(block: &[u32]) -> u32 {
    let allowed = {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a share of BLOCK is small and positive"
        )]
        let allowed = (block.len() as f64 * PFOR_EXCEPTION_SHARE) as usize;
        allowed
    };

    let mut widths = [0usize; 33];
    for &value in block {
        widths[bits_needed(value) as usize] += 1;
    }

    // Walk widths upward, counting how many values would still not fit.
    let mut over = block.len();
    for bits in 0..=32u32 {
        over -= widths[bits as usize];
        if over <= allowed {
            return bits;
        }
    }
    32
}

/// Every codec, for benchmarking and for tests that must hold of all of them.
pub fn all() -> Vec<Box<dyn Codec>> {
    vec![
        Box::new(Varint),
        Box::new(Simple9),
        Box::new(BitPacked),
        Box::new(PForDelta),
    ]
}

#[cfg(test)]
mod tests {
    use super::{BLOCK, BitPacked, Codec, PForDelta, Simple9, Varint, all, bits_needed};

    fn round_trip(codec: &dyn Codec, values: &[u32]) {
        let mut buffer = Vec::new();
        codec.encode(values, &mut buffer);
        let mut cursor = 0;
        let decoded = codec.decode(&buffer, &mut cursor, values.len());
        assert_eq!(
            decoded.as_deref(),
            Some(values),
            "{} failed to round-trip {} values",
            codec.name(),
            values.len()
        );
        assert_eq!(cursor, buffer.len(), "{} left bytes unread", codec.name());
    }

    #[test]
    fn bits_needed_is_right_at_the_boundaries() {
        assert_eq!(bits_needed(0), 0);
        assert_eq!(bits_needed(1), 1);
        assert_eq!(bits_needed(2), 2);
        assert_eq!(bits_needed(255), 8);
        assert_eq!(bits_needed(256), 9);
        assert_eq!(bits_needed(u32::MAX), 32);
    }

    #[test]
    fn every_codec_round_trips_an_empty_list() {
        for codec in all() {
            round_trip(codec.as_ref(), &[]);
        }
    }

    #[test]
    fn every_codec_round_trips_small_gaps() {
        // The common case: a frequent term, so the gaps are tiny.
        let values: Vec<u32> = (0..500).map(|i| 1 + (i % 7)).collect();
        for codec in all() {
            round_trip(codec.as_ref(), &values);
        }
    }

    #[test]
    fn every_codec_round_trips_mixed_widths() {
        let values = vec![0, 1, 127, 128, 255, 256, 65_535, 65_536, 3, 1, 1, 1];
        for codec in all() {
            round_trip(codec.as_ref(), &values);
        }
    }

    #[test]
    fn every_codec_round_trips_extremes() {
        let values = vec![u32::MAX, 0, u32::MAX, 1, 1 << 28, (1 << 28) - 1];
        for codec in all() {
            round_trip(codec.as_ref(), &values);
        }
    }

    #[test]
    fn every_codec_round_trips_across_block_boundaries() {
        // The sizes where a block scheme is most likely to be wrong.
        for count in [BLOCK - 1, BLOCK, BLOCK + 1, 2 * BLOCK, 2 * BLOCK + 5] {
            let values: Vec<u32> = (0..count)
                .map(|i| (u32::try_from(i).unwrap() * 37) % 1_000)
                .collect();
            for codec in all() {
                round_trip(codec.as_ref(), &values);
            }
        }
    }

    #[test]
    fn every_codec_round_trips_all_zeroes() {
        // A whole block of zeroes should cost almost nothing and still decode.
        let values = vec![0u32; BLOCK * 2];
        for codec in all() {
            round_trip(codec.as_ref(), &values);
        }
    }

    #[test]
    fn several_lists_can_be_concatenated() {
        // How the index stores them: one after another in one file.
        for codec in all() {
            let first: Vec<u32> = (0..200).map(|i| i % 11).collect();
            let second: Vec<u32> = (0..BLOCK + 3)
                .map(|i| u32::try_from(i).unwrap() % 5)
                .collect();

            let mut buffer = Vec::new();
            codec.encode(&first, &mut buffer);
            let boundary = buffer.len();
            codec.encode(&second, &mut buffer);

            let mut cursor = 0;
            assert_eq!(
                codec.decode(&buffer, &mut cursor, first.len()).unwrap(),
                first
            );
            assert_eq!(
                cursor,
                boundary,
                "{} misjudged where the first list ended",
                codec.name()
            );
            assert_eq!(
                codec.decode(&buffer, &mut cursor, second.len()).unwrap(),
                second
            );
        }
    }

    #[test]
    fn truncated_input_is_refused_rather_than_guessed_at() {
        let values: Vec<u32> = (0..BLOCK + 20)
            .map(|i| u32::try_from(i).unwrap() % 300)
            .collect();
        for codec in all() {
            let mut buffer = Vec::new();
            codec.encode(&values, &mut buffer);

            for cut in [1usize, buffer.len() / 3, buffer.len() / 2, buffer.len() - 1] {
                let mut cursor = 0;
                if let Some(decoded) = codec.decode(&buffer[..cut], &mut cursor, values.len()) {
                    assert_ne!(
                        decoded,
                        values,
                        "{} decoded truncated input as the full list",
                        codec.name()
                    );
                }
            }
        }
    }

    #[test]
    fn bit_packing_beats_variable_byte_on_small_similar_gaps() {
        // The case that matters: a common term's posting list, where gaps are
        // small and alike. Variable-byte spends a whole byte on each.
        let values: Vec<u32> = (0..BLOCK * 8)
            .map(|i| 1 + (u32::try_from(i).unwrap() % 6))
            .collect();

        let mut varint = Vec::new();
        Varint.encode(&values, &mut varint);
        let mut packed = Vec::new();
        BitPacked.encode(&values, &mut packed);
        let mut simple = Vec::new();
        Simple9.encode(&values, &mut simple);

        assert!(
            packed.len() < varint.len() / 2,
            "bit-packed {} vs varint {}",
            packed.len(),
            varint.len()
        );
        assert!(
            simple.len() < varint.len(),
            "simple9 {} vs varint {}",
            simple.len(),
            varint.len()
        );
    }

    #[test]
    fn patching_beats_plain_bit_packing_when_a_block_has_outliers() {
        // The whole reason PForDelta exists: one large gap in a block of small
        // ones widens every value under frame-of-reference, and patching does
        // not let it.
        let mut values: Vec<u32> = (0..BLOCK * 4)
            .map(|i| 1 + (u32::try_from(i).unwrap() % 4))
            .collect();
        for block in 0..4 {
            values[block * BLOCK + 5] = 900_000;
        }

        let mut packed = Vec::new();
        BitPacked.encode(&values, &mut packed);
        let mut patched = Vec::new();
        PForDelta.encode(&values, &mut patched);

        assert!(
            patched.len() < packed.len() / 2,
            "pfordelta {} vs bitpacked {}",
            patched.len(),
            packed.len()
        );
        round_trip(&PForDelta, &values);
    }

    #[test]
    fn variable_byte_wins_on_short_lists() {
        // Honest the other way: most terms are rare, and a block scheme has
        // nothing to amortise its width byte over.
        let values = vec![7u32, 3, 91];
        let mut varint = Vec::new();
        Varint.encode(&values, &mut varint);
        let mut packed = Vec::new();
        BitPacked.encode(&values, &mut packed);
        assert!(
            packed.len() >= varint.len(),
            "a three-value list should not favour blocks"
        );
    }

    #[test]
    fn codec_names_are_distinct() {
        let names: Vec<&str> = all().iter().map(|codec| codec.name()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate codec names: {names:?}"
        );
    }
}
