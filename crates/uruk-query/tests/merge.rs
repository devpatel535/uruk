//! A merged index must answer exactly as the unmerged one did.
//!
//! Merging rewrites document ids, renumbers hosts and re-encodes every posting
//! list. Any of those going wrong produces an index that opens, searches, and
//! returns confidently wrong answers — the failure mode this repository has
//! hit before and now tests for directly.
//!
//! So the assertion is not about files. It is that the same queries against
//! the same corpus return the same URLs, in the same order, with the same
//! scores, before and after.

use std::path::{Path, PathBuf};

use uruk_crawl::store::{CrawlSummary, QualitySignals, Record, StoreReader, StoreWriter};
use uruk_index::build::{IndexConfig, build, read_manifest};
use uruk_index::index::Index;
use uruk_index::merge::{MergeConfig, merge};
use uruk_query::parse;
use uruk_query::search::{SearchOptions, search};

/// Bodies distinct enough that ranking between them has one right answer.
const TOPICS: [(&str, &str); 6] = [
    (
        "Clay tablets and the reed stylus",
        "The scribes of Uruk pressed a reed into wet clay to record barley rations. \
         A tablet outlived the person who made it, which is the whole idea.",
    ),
    (
        "Barley rations in the temple storehouse",
        "Accounts of grain moving from the threshing floor to the storehouse, counted \
         in measures nobody now uses, recorded in cuneiform on clay.",
    ),
    (
        "Cuneiform and how the wedges got there",
        "Pressing rather than dragging a reed leaves a wedge. The shape of the writing \
         was decided by the material, as the shape of writing usually is.",
    ),
    (
        "Seal impressions and who vouched for what",
        "A cylinder seal rolled across wet clay left a signature. Administration in \
         Uruk depended on somebody being willing to be named.",
    ),
    (
        "The storehouse fire that saved the archive",
        "Most tablets that survive were baked by accident when a building burned. \
         Destruction is why the record exists at all.",
    ),
    (
        "Counting sheep before counting words",
        "The first tablets are not literature. They are inventories of animals, and \
         writing is what inventories turned into.",
    ),
];

fn corpus(crawl: &Path, pages: usize) {
    std::fs::create_dir_all(crawl).expect("crawl dir");
    let mut writer = StoreWriter::create(crawl).expect("store");
    for i in 0..pages {
        let (title, body) = TOPICS[i % TOPICS.len()];
        // Different hosts, so host renumbering across segments is exercised
        // rather than assumed.
        let host = format!("host{}.test", i % 7);
        writer
            .push(&Record {
                url: format!("https://{host}/article/{i}"),
                final_url: format!("https://{host}/article/{i}"),
                fetched_at: 1_700_000_000,
                status: 200,
                depth: 1,
                fingerprint: i as u64,
                title: format!("{title} ({i})"),
                text: format!("{body} Entry number {i}."),
                headings: vec![title.to_string()],
                lang: Some(String::from("en")),
                links: Vec::new(),
                quality: QualitySignals {
                    text_ratio: 0.4 + (i % 5) as f64 / 50.0,
                    link_density: 0.05,
                    scripts: i % 4,
                    words: body.split_whitespace().count(),
                },
                snippet_allowed: true,
                max_snippet: None,
            })
            .expect("push");
    }
    writer.finish(&CrawlSummary::default()).expect("finish");
}

struct Both {
    root: PathBuf,
    before: Index,
    after: Index,
    crawl: PathBuf,
}

fn prepare(name: &str, pages: usize, docs_per_segment: usize, merge_to: usize) -> Both {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("merge-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    let crawl = root.join("crawl");
    let index = root.join("index");
    let merged = root.join("merged");

    corpus(&crawl, pages);
    build(&IndexConfig {
        crawl_dir: crawl.clone(),
        out_dir: index.clone(),
        docs_per_segment,
        progress: false,
    })
    .expect("build");

    merge(&MergeConfig {
        index_dir: index.clone(),
        out_dir: merged.clone(),
        docs_per_segment: merge_to,
        progress: false,
    })
    .expect("merge");

    Both {
        before: Index::open(&index).expect("open source"),
        after: Index::open(&merged).expect("open merged"),
        crawl,
        root,
    }
}

impl Drop for Both {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// URLs and scores for a query, which is what "the same answer" means.
fn answer(index: &mut Index, crawl: &Path, raw: &str) -> Vec<(String, f64)> {
    let query = parse::parse(raw);
    let results = search(index, &query, &SearchOptions::default()).expect("search");
    let mut store = StoreReader::open(crawl).expect("store");
    results
        .hits
        .iter()
        .map(|hit| (store.get(hit.crawl_doc).expect("record").url, hit.score))
        .collect()
}

const QUERIES: [&str; 9] = [
    "clay tablets",
    "\"clay tablets\"",
    "barley rations",
    "cuneiform",
    "reed -barley",
    "site:host3.test clay",
    "writing inventories",
    "seal impressions vouched",
    "zzznothinghere",
];

#[test]
fn a_merged_index_answers_identically() {
    // Twelve segments down to one.
    let mut both = prepare("identical", 120, 10, 10_000);

    assert!(both.before.segment_count() > 1, "the source was not split");
    assert_eq!(both.after.segment_count(), 1, "the merge did not combine");
    assert_eq!(both.before.len(), both.after.len(), "documents were lost");

    for query in QUERIES {
        let before = answer(&mut both.before, &both.crawl, query);
        let after = answer(&mut both.after, &both.crawl, query);
        assert_eq!(
            before.iter().map(|(url, _)| url).collect::<Vec<_>>(),
            after.iter().map(|(url, _)| url).collect::<Vec<_>>(),
            "merging changed the results for {query:?}"
        );
        for ((url, a), (_, b)) in before.iter().zip(&after) {
            assert!(
                (a - b).abs() < 1e-9,
                "merging changed the score of {url} for {query:?}: {a} vs {b}"
            );
        }
    }
}

#[test]
fn the_size_bound_is_respected_and_still_answers_identically() {
    // Twelve segments into groups of at most forty documents, so the merge
    // produces several segments rather than one and the bound is exercised.
    let mut both = prepare("bounded", 120, 10, 40);

    assert!(both.after.segment_count() > 1, "the bound was ignored");
    assert!(
        both.after.segment_count() < both.before.segment_count(),
        "the merge combined nothing"
    );
    assert_eq!(both.before.len(), both.after.len());

    for query in QUERIES {
        assert_eq!(
            answer(&mut both.before, &both.crawl, query),
            answer(&mut both.after, &both.crawl, query),
            "merging changed the results for {query:?}"
        );
    }
}

#[test]
fn merging_shrinks_the_index_it_does_not_grow_it() {
    // Every segment repeats the dictionary for terms it shares with the
    // others, and a term in all twelve costs twelve posting-list headers.
    // Merging should give some of that back; if it ever costs more, the
    // trade has stopped being worth making and somebody should know.
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("merge-size");
    let _ = std::fs::remove_dir_all(&root);
    let crawl = root.join("crawl");
    let index = root.join("index");
    let merged = root.join("merged");

    corpus(&crawl, 300);
    let before = build(&IndexConfig {
        crawl_dir: crawl.clone(),
        out_dir: index.clone(),
        docs_per_segment: 25,
        progress: false,
    })
    .expect("build");

    let after = merge(&MergeConfig {
        index_dir: index.clone(),
        out_dir: merged.clone(),
        docs_per_segment: 10_000,
        progress: false,
    })
    .expect("merge");

    assert_eq!(after.documents, before.documents);
    assert_eq!(after.postings, before.postings, "postings were lost");
    assert!(
        after.bytes_dictionary < before.bytes_dictionary,
        "the dictionary did not shrink: {} -> {}",
        before.bytes_dictionary,
        after.bytes_dictionary
    );
    assert!(
        after.bytes_total < before.bytes_total,
        "the merged index is larger: {} -> {}",
        before.bytes_total,
        after.bytes_total
    );

    // And the manifest on disk describes what is actually there.
    let written = read_manifest(&merged).expect("manifest");
    assert_eq!(written.segments.len(), 1);
    assert_eq!(written.documents, before.documents);

    let _ = std::fs::remove_dir_all(&root);
}
