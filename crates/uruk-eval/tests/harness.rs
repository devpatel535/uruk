//! The harness against a real crawl, index and ranker.
//!
//! The unit tests check the arithmetic. These check the thing the arithmetic
//! is for: that a judged query set run against a real index detects a real
//! ranking change, and does not detect one that is not there.
//!
//! The corpus is built to contain the disagreement Phase 6 measured — a link
//! farm that wins on keyword repetition and loses on host authority — because
//! a harness that cannot see that difference cannot see any difference.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use uruk_crawl::store::{CrawlSummary, OutLink, QualitySignals, Record, StoreReader, StoreWriter};
use uruk_eval::compare::{DEFAULT_TRIALS, compare};
use uruk_eval::judgments::Judgments;
use uruk_eval::run::{Configuration, Signal, evaluate};
use uruk_index::build::{IndexConfig, build};
use uruk_index::index::Index;
use uruk_link::graph::HostGraph;
use uruk_link::{Authority, Method};

const BODY: &str = "Clay tablets from Uruk record barley rations in cuneiform. The scribes \
    pressed a reed into wet clay to make a promise outlive the person who made it, and the \
    tablets that survive were baked by accident when a storehouse burned.";

/// Keyword-stuffed text: says the query words more often and means less.
const STUFFED: &str = "Clay tablets clay tablets clay tablets cuneiform barley Uruk clay \
    tablets clay tablets cuneiform barley clay tablets clay tablets best clay tablets page \
    clay tablets cuneiform barley Uruk clay tablets clay tablets.";

fn record(url: &str, title: &str, text: &str, links: &[&str]) -> Record {
    Record {
        url: url.to_string(),
        final_url: url.to_string(),
        fetched_at: 0,
        status: 200,
        depth: 0,
        fingerprint: 0,
        title: title.to_string(),
        text: text.to_string(),
        headings: vec![title.to_string()],
        lang: Some(String::from("en")),
        links: links
            .iter()
            .map(|&url| OutLink {
                url: url.to_string(),
                anchor: String::from("a link"),
                nofollow: false,
            })
            .collect(),
        quality: QualitySignals {
            text_ratio: 0.5,
            link_density: 0.05,
            scripts: 0,
            words: text.split_whitespace().count(),
        },
        snippet_allowed: true,
        max_snippet: None,
    }
}

struct Corpus {
    dir: PathBuf,
    index: Index,
    store: StoreReader,
    authority: Authority,
}

/// A corpus where honest pages are linked and a farm is not.
fn corpus(name: &str) -> Corpus {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    let crawl = dir.join("crawl");
    let index_dir = dir.join("index");
    std::fs::create_dir_all(&crawl).expect("crawl dir");

    let mut pages = vec![
        // A hub that vouches for the two archives.
        record(
            "https://hub.test/",
            "Cuneiform studies",
            BODY,
            &["https://archive-a.test/", "https://archive-b.test/"],
        ),
        record(
            "https://archive-a.test/",
            "Tablet archive A",
            BODY,
            &["https://archive-b.test/"],
        ),
        record(
            "https://archive-b.test/",
            "Tablet archive B",
            BODY,
            &["https://archive-a.test/"],
        ),
        // The farm's target: stuffed, and linked only from its own site.
        record(
            "https://farm.test/target",
            "The best clay tablets page",
            STUFFED,
            &[],
        ),
    ];
    for i in 0..12 {
        pages.push(record(
            &format!("https://farm.test/p{i}"),
            &format!("Clay tablets resource {i}"),
            STUFFED,
            &["https://farm.test/target", "https://hub.test/"],
        ));
    }

    let mut writer = StoreWriter::create(&crawl).expect("store");
    for page in &pages {
        writer.push(page).expect("push");
    }
    writer.finish(&CrawlSummary::default()).expect("finish");

    build(&IndexConfig {
        crawl_dir: crawl.clone(),
        out_dir: index_dir.clone(),
        docs_per_segment: 100,
        progress: false,
    })
    .expect("index");

    let mut reader = StoreReader::open(&crawl).expect("reopen");
    let graph = HostGraph::build(&mut reader, &[String::from("https://hub.test/")]).expect("graph");
    let authority = Authority::compute(&graph, Method::InDegree, 0.85);

    Corpus {
        dir,
        index: Index::open(&index_dir).expect("open index"),
        store: StoreReader::open(&crawl).expect("store reader"),
        authority,
    }
}

impl Drop for Corpus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Every non-empty subset of the shared vocabulary, up to four terms.
///
/// Thirty genuinely different queries — `clay tablets`, `cuneiform barley`,
/// `uruk clay cuneiform` — rather than one query repeated thirty times. It
/// matters: a paired test over thirty copies of one query is measuring one
/// observation and reporting thirty, which is the exact mistake the test
/// exists to prevent anyone else from making.
///
/// All five words appear in every document, so every query retrieves the whole
/// corpus and the only thing that can separate the results is the ranking.
fn judgments(count: usize) -> Judgments {
    const VOCABULARY: [&str; 5] = ["clay", "tablets", "cuneiform", "barley", "uruk"];

    let mut queries: Vec<String> = Vec::new();
    // Subsets as bit patterns, skipping the empty one and any with all five
    // terms, ordered by size so short queries come first.
    for size in 1..VOCABULARY.len() {
        for mask in 1u32..(1 << VOCABULARY.len()) {
            if mask.count_ones() as usize != size {
                continue;
            }
            let terms: Vec<&str> = VOCABULARY
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, term)| *term)
                .collect();
            queries.push(terms.join(" "));
        }
    }
    assert!(
        queries.len() >= count,
        "asked for {count} queries, the vocabulary yields {}",
        queries.len()
    );

    let mut text = String::new();
    for query in queries.iter().take(count) {
        let _ = write!(
            text,
            "query: {query}\n  3 https://archive-a.test/\n  3 https://archive-b.test/\n  \
             2 https://hub.test/\n  0 https://farm.test/target\n"
        );
    }
    Judgments::parse(&text, std::path::Path::new("t")).expect("parses")
}

#[test]
fn the_harness_detects_the_ranking_change_authority_makes() {
    let mut corpus = corpus("detects");
    let judged = judgments(30);

    let with = Configuration::baseline(Some(corpus.authority.clone()));
    let without = with.without(Signal::Authority);

    let a = evaluate(&mut corpus.index, &mut corpus.store, &judged, &with).expect("with");
    let b = evaluate(&mut corpus.index, &mut corpus.store, &judged, &without).expect("without");

    assert!(
        a.summary.ndcg > b.summary.ndcg,
        "authority did not improve nDCG: {:.4} with, {:.4} without",
        a.summary.ndcg,
        b.summary.ndcg
    );

    let result = compare(&b.scores, &a.scores, DEFAULT_TRIALS);
    assert!(result.difference > 0.0);
    assert!(
        result.significant(),
        "a change this consistent should be detectable: {}",
        result.verdict()
    );
}

#[test]
fn the_harness_does_not_invent_a_difference_that_is_not_there() {
    // The same configuration twice. Anything but "no detectable difference"
    // here would mean the harness blesses noise, which is worse than having no
    // harness: it would launder every change into evidence.
    let mut corpus = corpus("no_difference");
    let judged = judgments(30);
    let configuration = Configuration::baseline(Some(corpus.authority.clone()));

    let a = evaluate(
        &mut corpus.index,
        &mut corpus.store,
        &judged,
        &configuration,
    )
    .expect("a");
    let b = evaluate(
        &mut corpus.index,
        &mut corpus.store,
        &judged,
        &configuration,
    )
    .expect("b");

    let result = compare(&a.scores, &b.scores, DEFAULT_TRIALS);
    assert!(result.difference.abs() < f64::EPSILON);
    assert!(!result.significant(), "{}", result.verdict());
    assert!(result.verdict().contains("no detectable difference"));
}

#[test]
fn coverage_falls_when_the_corpus_outgrows_the_judgments() {
    // The number that says whether to believe the rest. Judging one document
    // and letting the engine return ten means nine were scored as irrelevant
    // because nobody looked.
    let mut corpus = corpus("coverage");
    let thin = Judgments::parse(
        "query: clay tablets\n  3 https://archive-a.test/\n",
        std::path::Path::new("t"),
    )
    .expect("parses");

    let configuration = Configuration::baseline(Some(corpus.authority.clone()));
    let run = evaluate(&mut corpus.index, &mut corpus.store, &thin, &configuration).expect("run");
    assert!(
        run.summary.coverage < 0.3,
        "coverage was {:.2}, so the warning would never fire",
        run.summary.coverage
    );
}

#[test]
fn every_signal_can_be_switched_off_and_measured() {
    // The question a harness answers best is "what is this signal worth?".
    // Each one has to at least run and produce a finite number; whether it
    // helps is for a real judged set to say.
    let mut corpus = corpus("signals");
    let judged = judgments(5);
    let baseline = Configuration::baseline(Some(corpus.authority.clone()));

    let mut by_signal = BTreeMap::new();
    for signal in Signal::ALL {
        let variant = baseline.without(signal);
        let run =
            evaluate(&mut corpus.index, &mut corpus.store, &judged, &variant).expect("variant");
        assert!(run.summary.ndcg.is_finite());
        by_signal.insert(signal.name(), run.summary.ndcg);
    }
    assert_eq!(by_signal.len(), Signal::ALL.len());
}

#[test]
fn both_authority_methods_can_be_evaluated_against_each_other() {
    // The decision RESEARCH.md 5.3 deferred twice. This does not answer it -
    // a four-host fixture cannot - but it proves the question is now
    // answerable by running a command rather than by argument.
    let mut corpus = corpus("methods");
    let judged = judgments(30);
    let baseline = Configuration::baseline(Some(corpus.authority.clone()));

    let degree = baseline.using(Method::InDegree);
    let trust = baseline.using(Method::TrustRank);

    let a = evaluate(&mut corpus.index, &mut corpus.store, &judged, &degree).expect("in-degree");
    let b = evaluate(&mut corpus.index, &mut corpus.store, &judged, &trust).expect("trustrank");

    let result = compare(&a.scores, &b.scores, DEFAULT_TRIALS);
    assert_eq!(result.queries, 30);
    assert!(result.p_value > 0.0 && result.p_value <= 1.0);
}
