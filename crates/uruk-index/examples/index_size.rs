//! Measure what an index actually costs, on a corpus big enough to mean it.
//!
//! `RESEARCH.md` §6 makes a numerical claim — roughly 3.5 GB per million
//! pages, with positions the largest component — and the brief sets a target
//! of 15–25% of the raw text. Neither is worth anything unmeasured, and a
//! four-document fixture cannot measure it: at that size the term dictionary
//! dominates and the ratio is meaningless.
//!
//! This generates a synthetic corpus with the statistics real prose has (a
//! Zipf-distributed vocabulary, so a few words are very common and most are
//! rare, which is what decides how well delta encoding does) and reports the
//! size breakdown.
//!
//! It is also the harness Phase 5 needs: benchmarking Simple-9, `PForDelta`
//! and Elias-Fano against the variable-byte baseline means running exactly
//! this and comparing the postings line.
//!
//! ```sh
//! cargo run --release --example index_size -p uruk-index -- 20000
//! ```

use std::path::PathBuf;

use uruk_crawl::store::{CrawlSummary, QualitySignals, Record, StoreWriter};
use uruk_index::build::{IndexConfig, build};

/// Words in the generated vocabulary. Real English web text runs to far more,
/// but most of that tail is junk the tokeniser drops anyway.
const VOCABULARY: usize = 60_000;
/// Body length. Around what an article runs to.
const WORDS_PER_DOC: usize = 800;

/// A deterministic generator, so two runs of the same size are comparable.
///
/// An LCG rather than a dependency: this needs to be reproducible and fast,
/// not statistically excellent.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn below(&mut self, limit: usize) -> usize {
        usize::try_from(self.next() % limit as u64).unwrap_or(0)
    }

    fn unit(&mut self) -> f64 {
        (self.next() % 1_000_000) as f64 / 1_000_000.0
    }
}

/// Build a vocabulary of pronounceable nonsense, so terms have realistic
/// lengths and share prefixes the way real words do — which is what the
/// dictionary's front coding is betting on.
fn vocabulary(rng: &mut Rng) -> Vec<String> {
    const ONSETS: [&str; 21] = [
        "b", "c", "d", "f", "g", "h", "j", "k", "l", "m", "n", "p", "r", "s", "t", "v", "w", "st",
        "tr", "pl", "br",
    ];
    const NUCLEI: [&str; 10] = ["a", "e", "i", "o", "u", "ai", "ea", "ou", "ie", "oo"];
    const CODAS: [&str; 12] = [
        "n", "t", "s", "r", "l", "d", "m", "k", "ng", "st", "nt", "ck",
    ];

    let mut words = Vec::with_capacity(VOCABULARY);
    for _ in 0..VOCABULARY {
        let syllables = 1 + rng.below(3);
        let mut word = String::new();
        for _ in 0..syllables {
            word.push_str(ONSETS[rng.below(ONSETS.len())]);
            word.push_str(NUCLEI[rng.below(NUCLEI.len())]);
            if rng.unit() < 0.6 {
                word.push_str(CODAS[rng.below(CODAS.len())]);
            }
        }
        words.push(word);
    }
    words
}

/// Zipf: the rank-`k` word appears about `1/k` as often as the commonest.
///
/// This is the distribution that makes an inverted index work — a handful of
/// terms have enormous posting lists with tiny gaps, and the long tail has
/// lists of one or two.
fn zipf_index(rng: &mut Rng, size: usize) -> usize {
    // Inverse-transform sampling of a 1/k distribution, which is cheap and
    // close enough for measuring compression.
    let u = rng.unit().max(1e-9);
    let scaled = (size as f64).powf(u).clamp(0.0, size as f64 - 1.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..size-1 on the line above"
    )]
    let index = scaled as usize;
    index.min(size - 1)
}

/// One synthetic document's text: body, title and heading.
fn document(rng: &mut Rng, words: &[String]) -> (String, String, String) {
    let pick = |rng: &mut Rng, count: usize| {
        (0..count)
            .map(|_| words[zipf_index(rng, words.len())].as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    (pick(rng, WORDS_PER_DOC), pick(rng, 8), pick(rng, 6))
}

/// Write a synthetic crawl store, returning its summary and the bytes of text
/// that went into it.
fn generate(crawl_dir: &std::path::Path, documents: usize) -> (CrawlSummary, u64) {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let words = vocabulary(&mut rng);

    eprintln!("generating {documents} documents of ~{WORDS_PER_DOC} words...");
    let mut writer = StoreWriter::create(crawl_dir).expect("create store");
    let mut text_bytes = 0u64;

    for doc in 0..documents {
        let (body, title, heading) = document(&mut rng, &words);
        text_bytes += body.len() as u64;
        let host = doc % 500;
        writer
            .push(&Record {
                url: format!("https://host{host}.test/articles/{doc}"),
                final_url: format!("https://host{host}.test/articles/{doc}"),
                fetched_at: 1_700_000_000,
                status: 200,
                depth: 2,
                fingerprint: doc as u64,
                title,
                text: body,
                headings: vec![heading],
                lang: Some("en".into()),
                links: Vec::new(),
                quality: QualitySignals {
                    text_ratio: 0.4,
                    link_density: 0.1,
                    scripts: 3,
                    words: WORDS_PER_DOC,
                },
                snippet_allowed: true,
                max_snippet: None,
            })
            .expect("write record");
    }
    (
        writer
            .finish(&CrawlSummary::default())
            .expect("finish store"),
        text_bytes,
    )
}

fn report(
    documents: usize,
    text_bytes: u64,
    store_bytes: u64,
    manifest: &uruk_index::build::IndexManifest,
    elapsed: std::time::Duration,
) {
    let mb = |bytes: u64| bytes as f64 / 1_048_576.0;
    let pct = |bytes: u64| 100.0 * bytes as f64 / text_bytes as f64;
    let total = store_bytes + manifest.bytes_total;

    println!("\n=== {documents} documents, {WORDS_PER_DOC} words each ===");
    println!("indexed in {:.1}s", elapsed.as_secs_f64());
    println!();
    println!("  extracted text        {:8.1} MB", mb(text_bytes));
    println!(
        "  crawl store (zstd)    {:8.1} MB   {:5.1}% of text",
        mb(store_bytes),
        pct(store_bytes)
    );
    println!();
    println!(
        "  index total           {:8.1} MB   {:5.1}% of text",
        mb(manifest.bytes_total),
        pct(manifest.bytes_total)
    );
    println!(
        "    postings            {:8.1} MB   {:5.1}%",
        mb(manifest.bytes_postings),
        pct(manifest.bytes_postings)
    );
    println!(
        "    term dictionary     {:8.1} MB   {:5.1}%",
        mb(manifest.bytes_dictionary),
        pct(manifest.bytes_dictionary)
    );
    println!(
        "    host table          {:8.1} KB",
        manifest.bytes_hosts as f64 / 1024.0
    );
    println!(
        "    doc table           {:8.1} KB",
        manifest.bytes_doc_table as f64 / 1024.0
    );
    println!();
    println!("  distinct terms        {:8}", manifest.terms);
    println!("  postings              {:8}", manifest.postings);
    println!(
        "  bytes per posting     {:8.2}   <- the number Phase 5 has to beat",
        manifest.bytes_postings as f64 / manifest.postings as f64
    );
    println!();
    println!("  store + index         {:8.1} MB", mb(total));
    println!(
        "  per page              {:8.0} bytes",
        total as f64 / documents as f64
    );
    println!(
        "\n  extrapolated to 1M pages: {:.1} GB",
        total as f64 / documents as f64 * 1_000_000.0 / 1_073_741_824.0
    );
}

fn main() {
    let documents: usize = std::env::args()
        .nth(1)
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(20_000);

    let root: PathBuf = std::env::temp_dir().join(format!("uruk-index-size-{documents}"));
    let _ = std::fs::remove_dir_all(&root);
    let crawl_dir = root.join("crawl");
    let index_dir = root.join("index");

    let (summary, text_bytes) = generate(&crawl_dir, documents);

    eprintln!("indexing...");
    let started = std::time::Instant::now();
    let manifest = build(&IndexConfig {
        crawl_dir,
        out_dir: index_dir,
        docs_per_segment: 50_000,
        progress: false,
    })
    .expect("build index");

    report(
        documents,
        text_bytes,
        summary.bytes_written,
        &manifest,
        started.elapsed(),
    );
}
