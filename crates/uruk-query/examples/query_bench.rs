//! How long a query actually takes, against the brief's sub-200ms promise.
//!
//! Principle 4 says the index is "small on disk, fast to search", with a
//! sub-200ms target. `index_size` measured the first half at 20,000 and
//! 100,000 documents. The second half had never been measured at all, which is
//! an odd thing to leave unchecked in a non-negotiable.
//!
//! The cases below are the ones that differ, not a list of queries that happen
//! to be handy:
//!
//! - **The commonest term.** The worst case for a posting-list read: a
//!   substantial fraction of the corpus to decode.
//! - **A rare term.** The common case. Most queries are mostly rare words.
//! - **Two common terms.** The intersection walk, where the AND happens.
//! - **A common term with a rare one.** What most real queries look like, and
//!   the case a good engine should settle from the rare term's list alone.
//! - **A phrase.** The only query that needs positions, which are the largest
//!   part of the index.
//! - **An exclusion**, which reads a second list for nothing.
//! - **`site:`**, which can skip whole segments before reading anything.
//!
//! ```sh
//! cargo run --release --example query_bench -p uruk-query -- 200000
//! ```
//!
//! The corpus is generated the way `index_size` generates its own: a
//! Zipf-distributed vocabulary of pronounceable nonsense, so the posting lists
//! have the shape real ones do. Timings on synthetic data are still real
//! timings, because the decode work is identical — but they are timings of one
//! machine, and the absolute numbers will not transfer.

use std::time::{Duration, Instant};

use uruk_crawl::store::{CrawlSummary, QualitySignals, Record, StoreWriter};
use uruk_index::build::{IndexConfig, build};
use uruk_index::index::Index;
use uruk_query::parse;
use uruk_query::search::{SearchOptions, search};

const VOCABULARY: usize = 60_000;
const WORDS_PER_DOC: usize = 800;
/// Queries per case. Enough that the slowest is not one unlucky page fault.
const REPEATS: usize = 20;
/// The promise in principle 4.
const BUDGET_MS: f64 = 200.0;

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

fn zipf_index(rng: &mut Rng, size: usize) -> usize {
    let u = rng.unit().max(1e-9);
    let scaled = (size as f64).powf(u).clamp(0.0, size as f64 - 1.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..size-1 above"
    )]
    let index = scaled as usize;
    index.min(size - 1)
}

/// Build the corpus, returning the vocabulary so queries can be drawn from it
/// by rank rather than by hope.
fn generate(crawl_dir: &std::path::Path, documents: usize) -> Vec<String> {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let words = vocabulary(&mut rng);

    eprintln!("generating {documents} documents...");
    let mut writer = StoreWriter::create(crawl_dir).expect("create store");
    for doc in 0..documents {
        let pick = |rng: &mut Rng, count: usize| {
            (0..count)
                .map(|_| words[zipf_index(rng, VOCABULARY)].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let body = pick(&mut rng, WORDS_PER_DOC);
        let title = pick(&mut rng, 8);
        let heading = pick(&mut rng, 6);
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
                lang: Some(String::from("en")),
                links: Vec::new(),
                quality: QualitySignals {
                    text_ratio: 0.55,
                    link_density: 0.08,
                    scripts: 2,
                    words: WORDS_PER_DOC,
                },
                snippet_allowed: true,
                max_snippet: None,
            })
            .expect("write record");
    }
    writer.finish(&CrawlSummary::default()).expect("finish");
    words
}

struct Timing {
    name: &'static str,
    query: String,
    median: Duration,
    worst: Duration,
    matched: usize,
    lists_read: usize,
}

fn time(index: &mut Index, name: &'static str, raw: &str) -> Timing {
    let query = parse::parse(raw);
    let options = SearchOptions::default();

    // One untimed run, so the first query's page faults are not charged to the
    // measurement. A cold cache is a real cost, but it is a different question
    // from how long the work takes.
    let _ = search(index, &query, &options).expect("search");

    let mut samples = Vec::with_capacity(REPEATS);
    let mut matched = 0;
    let mut lists_read = 0;
    for _ in 0..REPEATS {
        let started = Instant::now();
        let results = search(index, &query, &options).expect("search");
        samples.push(started.elapsed());
        matched = results.matched;
        lists_read = results.lists_read;
    }
    samples.sort_unstable();

    Timing {
        name,
        query: raw.to_owned(),
        median: samples[samples.len() / 2],
        worst: samples[samples.len() - 1],
        matched,
        lists_read,
    }
}

/// The rank-`k` word in the Zipf vocabulary.
///
/// Rank 1 is the commonest; the rank-k word appears about 1/k as often. Picking
/// by rank is how "a term in a lot of the corpus" gets built rather than hoped
/// for.
fn term_by_rank(words: &[String], rank: usize) -> &str {
    &words[rank.min(words.len() - 1)]
}

fn report(documents: usize, segments: usize, cases: &[Timing]) {
    println!("\n=== {documents} documents, {segments} segments ===");
    println!(
        "  {:<26} {:>10} {:>10} {:>12} {:>7}",
        "case", "median", "worst", "matched", "lists"
    );
    for case in cases {
        println!(
            "  {:<26} {:>8.1}ms {:>8.1}ms {:>12} {:>7}",
            case.name,
            case.median.as_secs_f64() * 1000.0,
            case.worst.as_secs_f64() * 1000.0,
            case.matched,
            case.lists_read
        );
    }

    println!("\n  queries run:");
    for case in cases {
        println!("    {:<26} {}", case.name, case.query);
    }

    let over: Vec<&Timing> = cases
        .iter()
        .filter(|case| case.worst.as_secs_f64() * 1000.0 > BUDGET_MS)
        .collect();

    println!();
    if over.is_empty() {
        println!("  every case is inside the {BUDGET_MS:.0}ms budget, worst case included.");
    } else {
        println!("  OVER THE {BUDGET_MS:.0}ms BUDGET:");
        for case in over {
            println!(
                "    {:<26} {:.1}ms worst",
                case.name,
                case.worst.as_secs_f64() * 1000.0
            );
        }
        println!(
            "\n  Principle 4 is a promise, not an aspiration. Either this gets\n  \
             faster or the promise gets rewritten, and the first is better."
        );
    }
}

fn main() {
    let documents: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000);

    let dir = std::env::temp_dir().join(format!("uruk-query-bench-{documents}"));
    let crawl = dir.join("crawl");
    let index_dir = dir.join("index");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&crawl).expect("temp dir");

    let words = generate(&crawl, documents);

    eprintln!("indexing...");
    let started = Instant::now();
    let manifest = build(&IndexConfig {
        crawl_dir: crawl.clone(),
        out_dir: index_dir.clone(),
        docs_per_segment: 50_000,
        progress: false,
    })
    .expect("index");
    eprintln!("indexed in {:.1}s", started.elapsed().as_secs_f64());

    let mut index = Index::open(&index_dir).expect("open");

    let very_common = term_by_rank(&words, 1).to_owned();
    let common = term_by_rank(&words, 40).to_owned();
    let rare = term_by_rank(&words, 30_000).to_owned();
    let rarer = term_by_rank(&words, 50_000).to_owned();

    let cases = vec![
        time(&mut index, "commonest term", &very_common),
        time(&mut index, "rare term", &rare),
        time(
            &mut index,
            "two common terms",
            &format!("{very_common} {common}"),
        ),
        time(
            &mut index,
            "common + rare",
            &format!("{very_common} {rare}"),
        ),
        time(&mut index, "two rare terms", &format!("{rare} {rarer}")),
        time(
            &mut index,
            "phrase (needs positions)",
            &format!("\"{very_common} {common}\""),
        ),
        time(
            &mut index,
            "term with exclusion",
            &format!("{very_common} -{common}"),
        ),
        time(
            &mut index,
            "site: filter",
            &format!("site:host7.test {very_common}"),
        ),
        time(&mut index, "no match", "qqzzxxnotaword"),
    ];

    report(documents, manifest.segments.len(), &cases);
    let _ = std::fs::remove_dir_all(&dir);
}
