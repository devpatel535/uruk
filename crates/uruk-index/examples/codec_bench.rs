//! Size and decode speed for each posting codec.
//!
//! The brief asks for the trade-off rather than a conclusion, so this reports
//! both numbers for every scheme on data shaped like a real index's.
//!
//! Two workloads, because they have genuinely different statistics and a codec
//! can win one and lose the other:
//!
//! - **Document-id gaps.** A term in half the corpus has gaps of about two; a
//!   term in three documents has gaps in the hundreds of thousands. Posting
//!   lists are weighted by Zipf, so most *values* belong to common terms even
//!   though most *terms* are rare.
//! - **Position gaps.** Within a document, the distance between one occurrence
//!   of a word and the next. Small, and tightly clustered.
//!
//! ```sh
//! cargo run --release --example codec_bench -p uruk-index
//! ```

use std::time::Instant;

use uruk_index::codec::{Codec, all};

/// Documents in the hypothetical corpus the gaps are drawn from.
const CORPUS: u32 = 1_000_000;
/// Distinct terms to simulate.
const TERMS: usize = 4_000;
/// Times each decode is repeated, to get past timer noise.
const REPEATS: usize = 20;

/// Deterministic LCG, so runs are comparable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn unit(&mut self) -> f64 {
        (self.next() % 1_000_000) as f64 / 1_000_000.0
    }

    /// A geometric-ish draw with the given mean: the shape a gap between
    /// randomly scattered occurrences actually has.
    fn gap(&mut self, mean: f64) -> u32 {
        let u = self.unit().max(1e-9);
        let value = -mean * u.ln();
        let clamped = value.clamp(1.0, f64::from(u32::MAX) / 2.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 1..=u32::MAX/2 above"
        )]
        let gap = clamped as u32;
        gap
    }
}

/// Posting lists as document-id gaps, with Zipf-distributed frequencies.
fn docid_gaps(rng: &mut Rng) -> Vec<Vec<u32>> {
    let mut lists = Vec::with_capacity(TERMS);
    for rank in 1..=TERMS {
        // Zipf: the rank-k term appears in about 1/k of the documents the
        // commonest one does.
        let share = 0.5 / rank as f64;
        let frequency = (f64::from(CORPUS) * share).max(1.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "frequency is positive and below CORPUS"
        )]
        let count = (frequency as usize).min(CORPUS as usize);
        let mean = f64::from(CORPUS) / frequency;
        lists.push((0..count).map(|_| rng.gap(mean)).collect());
    }
    lists
}

/// Position gaps within documents: small, and clustered.
fn position_gaps(rng: &mut Rng) -> Vec<Vec<u32>> {
    let mut lists = Vec::with_capacity(TERMS);
    for rank in 1..=TERMS {
        // A word appearing often in an 800-word document has small gaps; a
        // word appearing once has a list of one.
        let occurrences = (200 / rank.max(1)).max(1);
        let mean = 800.0 / occurrences as f64;
        lists.push((0..occurrences).map(|_| rng.gap(mean)).collect());
    }
    lists
}

struct Measurement {
    name: &'static str,
    bytes: usize,
    bits_per_value: f64,
    decode_millions_per_second: f64,
}

fn measure(codec: &dyn Codec, lists: &[Vec<u32>]) -> Measurement {
    let values: usize = lists.iter().map(Vec::len).sum();

    // Encode every list into one buffer, as the segment does, remembering
    // where each starts so decoding can be measured on its own.
    let mut buffer = Vec::new();
    let mut offsets = Vec::with_capacity(lists.len());
    for list in lists {
        offsets.push(buffer.len());
        codec.encode(list, &mut buffer);
    }

    // Decode everything, repeatedly. `black_box` would be better; summing the
    // values is enough to stop the work being optimised away.
    let started = Instant::now();
    let mut checksum = 0u64;
    for _ in 0..REPEATS {
        for (list, &offset) in lists.iter().zip(&offsets) {
            let mut cursor = offset;
            let decoded = codec
                .decode(&buffer, &mut cursor, list.len())
                .expect("decodes");
            checksum = checksum.wrapping_add(u64::from(decoded.first().copied().unwrap_or(0)));
        }
    }
    let elapsed = started.elapsed();
    assert!(checksum > 0 || values == 0, "the decode was optimised away");

    Measurement {
        name: codec.name(),
        bytes: buffer.len(),
        bits_per_value: buffer.len() as f64 * 8.0 / values as f64,
        decode_millions_per_second: (values * REPEATS) as f64 / elapsed.as_secs_f64() / 1e6,
    }
}

fn report(workload: &str, lists: &[Vec<u32>]) {
    let values: usize = lists.iter().map(Vec::len).sum();
    println!("\n=== {workload} ===");
    println!("{} lists, {values} values\n", lists.len());
    println!(
        "  {:<12} {:>12} {:>12} {:>16} {:>10}",
        "codec", "bytes", "bits/value", "decode Mvals/s", "vs varint"
    );

    let mut baseline = 0usize;
    for codec in all() {
        let measured = measure(codec.as_ref(), lists);
        if measured.name == "varint" {
            baseline = measured.bytes;
        }
        let relative = if baseline == 0 {
            String::from("-")
        } else {
            format!("{:.0}%", 100.0 * measured.bytes as f64 / baseline as f64)
        };
        println!(
            "  {:<12} {:>12} {:>12.2} {:>16.1} {:>10}",
            measured.name,
            measured.bytes,
            measured.bits_per_value,
            measured.decode_millions_per_second,
            relative
        );
    }
}

fn main() {
    let mut rng = Rng(0xC0DE_C0DE_1234_5678);

    let docids = docid_gaps(&mut rng);
    report("document-id gaps (Zipf-weighted posting lists)", &docids);

    let positions = position_gaps(&mut rng);
    report("position gaps within documents", &positions);

    println!(
        "\nRead the size column against `index_size`'s bytes-per-posting line.\n\
         The position workload above is the pessimistic one: it measures each\n\
         document's positions as its own short list, which is what the segment\n\
         used to store. It now concatenates a term's positions into a single\n\
         stream, so the blocks fill and the real figure sits nearer the\n\
         document-id column than this one.\n"
    );
}
