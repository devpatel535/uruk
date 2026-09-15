//! Anchor text: finding a page by what other people call it.
//!
//! The reason the field exists is that a page often does not contain the words
//! people use to look for it. These tests build exactly that situation — a
//! page that never says the phrase — and check it is findable anyway, and that
//! the obvious ways to abuse it do not work.

use std::path::PathBuf;

use uruk_crawl::store::{CrawlSummary, OutLink, QualitySignals, Record, StoreReader, StoreWriter};
use uruk_index::build::{IndexConfig, build};
use uruk_index::index::Index;
use uruk_query::parse;
use uruk_query::search::{SearchOptions, search};

fn page(url: &str, title: &str, text: &str, links: &[(&str, &str, bool)]) -> Record {
    Record {
        url: url.to_string(),
        final_url: url.to_string(),
        fetched_at: 0,
        status: 200,
        depth: 1,
        fingerprint: url.len() as u64,
        title: title.to_string(),
        text: text.to_string(),
        headings: Vec::new(),
        lang: Some(String::from("en")),
        links: links
            .iter()
            .map(|&(target, anchor, nofollow)| OutLink {
                url: target.to_string(),
                anchor: anchor.to_string(),
                nofollow,
            })
            .collect(),
        quality: QualitySignals {
            text_ratio: 0.45,
            link_density: 0.05,
            scripts: 0,
            words: text.split_whitespace().count(),
        },
        snippet_allowed: true,
        max_snippet: None,
    }
}

struct Corpus {
    root: PathBuf,
    index: Index,
    crawl: PathBuf,
}

impl Drop for Corpus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn prepare(name: &str, pages: &[Record]) -> Corpus {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("anchors-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    let crawl = root.join("crawl");
    let index_dir = root.join("index");
    std::fs::create_dir_all(&crawl).expect("dir");

    let mut writer = StoreWriter::create(&crawl).expect("store");
    for page in pages {
        writer.push(page).expect("push");
    }
    writer.finish(&CrawlSummary::default()).expect("finish");

    build(&IndexConfig {
        crawl_dir: crawl.clone(),
        out_dir: index_dir.clone(),
        docs_per_segment: 1000,
        progress: false,
    })
    .expect("build");

    Corpus {
        index: Index::open(&index_dir).expect("open"),
        crawl,
        root,
    }
}

fn urls(corpus: &mut Corpus, raw: &str) -> Vec<String> {
    let query = parse::parse(raw);
    let results = search(&mut corpus.index, &query, &SearchOptions::default()).expect("search");
    let mut store = StoreReader::open(&corpus.crawl).expect("store");
    results
        .hits
        .iter()
        .map(|hit| store.get(hit.crawl_doc).expect("record").url)
        .collect()
}

/// A page that never uses the words people would search for.
const OPAQUE: &str = "Applications open in the autumn term. Candidates submit a portfolio \
    and two references. Decisions are issued in March.";

#[test]
fn a_page_is_findable_by_what_others_call_it() {
    // The admissions page never says "how to apply". Three other sites do.
    let mut corpus = prepare(
        "findable",
        &[
            page("https://college.test/admissions", "Admissions", OPAQUE, &[]),
            page(
                "https://guide.test/",
                "A guide",
                "Some advice for prospective students.",
                &[("https://college.test/admissions", "how to apply", false)],
            ),
            page(
                "https://forum.test/",
                "A forum",
                "A thread about the process.",
                &[("https://college.test/admissions", "how to apply", false)],
            ),
            page(
                "https://news.test/",
                "News",
                "Coverage of the admissions season.",
                &[("https://college.test/admissions", "apply here", false)],
            ),
        ],
    );

    assert!(
        !OPAQUE.to_lowercase().contains("apply"),
        "the fixture has stopped testing what it claims to"
    );

    let found = urls(&mut corpus, "apply");
    assert!(
        found.contains(&String::from("https://college.test/admissions")),
        "the page was not findable by what others call it: {found:?}"
    );

    // And as a phrase, which only works because anchor text gets positions.
    let phrase = urls(&mut corpus, "\"how to apply\"");
    assert_eq!(
        phrase,
        vec![String::from("https://college.test/admissions")],
        "the phrase did not match inside anchor text"
    );
}

#[test]
fn a_site_cannot_describe_itself() {
    // Navigation is not evidence. Every page on one site links to its own
    // target with the same words, and none of it counts.
    let mut pages = vec![page("https://self.test/target", "Target", OPAQUE, &[])];
    for i in 0..30 {
        pages.push(page(
            &format!("https://self.test/page{i}"),
            "A page",
            "Filler text about the autumn term and the portfolio.",
            &[("https://self.test/target", "how to apply", false)],
        ));
    }

    let mut corpus = prepare("self", &pages);
    let found = urls(&mut corpus, "\"how to apply\"");
    assert!(
        found.is_empty(),
        "a site described itself into the index: {found:?}"
    );
}

#[test]
fn one_host_repeating_itself_is_one_opinion() {
    // A sitewide footer link is one host's view, however many pages carry it.
    // The cap on distinct phrases is what stops a single site filling it, so
    // a second, different describer must still get through.
    let mut pages = vec![
        page("https://target.test/", "Target", OPAQUE, &[]),
        page(
            "https://second.test/",
            "Second",
            "A different site entirely.",
            &[("https://target.test/", "reference portfolio", false)],
        ),
    ];
    // One host, many pages, all shouting the same twenty different things.
    for i in 0..40 {
        let anchors: Vec<(&str, String, bool)> = (0..20)
            .map(|k| ("https://target.test/", format!("spamword{k}"), false))
            .collect();
        let borrowed: Vec<(&str, &str, bool)> = anchors
            .iter()
            .map(|(u, a, n)| (*u, a.as_str(), *n))
            .collect();
        pages.push(page(
            &format!("https://farm.test/p{i}"),
            "Farm",
            "Filler.",
            &borrowed,
        ));
    }

    let mut corpus = prepare("repeats", &pages);

    // The honest describer survives the farm's flooding.
    let honest = urls(&mut corpus, "\"reference portfolio\"");
    assert!(
        honest.contains(&String::from("https://target.test/")),
        "one host's flood crowded out an independent description: {honest:?}"
    );
}

#[test]
fn nofollow_anchors_do_not_describe_anything() {
    let mut corpus = prepare(
        "nofollow",
        &[
            page("https://target.test/", "Target", OPAQUE, &[]),
            page(
                "https://comments.test/",
                "Comments",
                "A thread.",
                &[("https://target.test/", "how to apply", true)],
            ),
        ],
    );
    let found = urls(&mut corpus, "\"how to apply\"");
    assert!(
        found.is_empty(),
        "a nofollow link described its target: {found:?}"
    );
}

#[test]
fn an_over_long_anchor_is_not_a_description() {
    // A paragraph wrapped in a link is not what anybody calls the page, and
    // indexing it as one is wrong in the direction that helps an attacker.
    let essay = "apply ".repeat(60);
    let mut corpus = prepare(
        "long",
        &[
            page("https://target.test/", "Target", OPAQUE, &[]),
            page(
                "https://essay.test/",
                "Essay",
                "A long-winded site.",
                &[("https://target.test/", &essay, false)],
            ),
        ],
    );
    assert!(
        urls(&mut corpus, "apply").is_empty(),
        "an essay counted as a description"
    );
}
