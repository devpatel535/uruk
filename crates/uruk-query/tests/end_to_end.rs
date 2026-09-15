//! Crawl store to index to ranked results.
//!
//! The unit tests check each stage against its own inputs. This checks the
//! stages agree: that a document written by the crawler, tokenised by the
//! indexer, encoded into a segment and read back through a posting list comes
//! out of a search in the right place, for the right reason.
//!
//! The corpus is small and built so each assertion has exactly one correct
//! answer — a ranking test against a corpus where two answers are defensible
//! tests nothing.

use std::path::PathBuf;

use uruk_crawl::store::{CrawlSummary, QualitySignals, Record, StoreReader, StoreWriter};
use uruk_index::build::{IndexConfig, build};
use uruk_index::index::Index;
use uruk_query::parse;
use uruk_query::search::{Results, SearchOptions, search};
use uruk_query::snippet::{self, SnippetPolicy};

struct Page {
    url: &'static str,
    title: &'static str,
    headings: &'static [&'static str],
    body: &'static str,
    quality: QualitySignals,
}

const GOOD: QualitySignals = QualitySignals {
    text_ratio: 0.45,
    link_density: 0.05,
    scripts: 1,
    words: 0,
};
const CHROME: QualitySignals = QualitySignals {
    text_ratio: 0.04,
    link_density: 0.92,
    scripts: 45,
    words: 0,
};

fn corpus() -> Vec<Page> {
    vec![
        // 0: "tablets" in the title, body and URL. The best answer for it.
        Page {
            url: "https://scribes.test/clay-tablets",
            title: "Clay tablets and the invention of the receipt",
            headings: &["The first ledgers"],
            body: "The scribes of Uruk pressed a cut reed into wet clay because dragging \
                   tears the surface while pressing does not. They were counting sheep and \
                   barley, not writing poetry, and what they invented was a way of making a \
                   promise outlive the person who made it.",
            quality: GOOD,
        },
        // 1: mentions tablets once, in the body only.
        Page {
            url: "https://harbours.test/sediment",
            title: "Deep harbours and the argument with sediment",
            headings: &["Dredging"],
            body: "A harbour is mostly a long argument with sediment. Rivers drop silt where \
                   the current slows. Nothing here concerns tablets at all, though the word \
                   appears once for the sake of the test.",
            quality: GOOD,
        },
        // 2: the phrase "clay tablets" does NOT occur; the words do, far apart.
        Page {
            url: "https://kilns.test/firing",
            title: "Firing and glazing",
            headings: &[],
            body: "Clay behaves differently at different temperatures, and a kiln is the \
                   instrument for exploring that. Much later in this paragraph, and quite \
                   separately from the earlier mention, we discuss tablets of a different \
                   kind entirely, the pharmaceutical sort.",
            quality: GOOD,
        },
        // 3: a link farm that mentions everything and means none of it.
        Page {
            url: "https://cheap.test/deals",
            title: "Cheap clay tablets deals discount offers",
            headings: &["Buy clay tablets now"],
            body: "Clay tablets clay tablets buy clay tablets cheap clay tablets discount \
                   clay tablets offers clay tablets deals clay tablets here clay tablets now.",
            quality: CHROME,
        },
        // 4: on the same host as 0, for a site: filter that must not be trivial.
        Page {
            url: "https://scribes.test/cuneiform",
            title: "Cuneiform marks",
            headings: &[],
            body: "The wedge shape was decided by the reed, not by anyone's taste. Barley \
                   accounts make up most of what survives from the period.",
            quality: GOOD,
        },
    ]
}

fn record(page: &Page) -> Record {
    Record {
        url: page.url.to_owned(),
        final_url: page.url.to_owned(),
        fetched_at: 1_700_000_000,
        status: 200,
        depth: 1,
        fingerprint: 1,
        title: page.title.to_owned(),
        text: page.body.to_owned(),
        headings: page.headings.iter().map(|h| (*h).to_owned()).collect(),
        lang: Some("en".into()),
        links: vec![],
        quality: QualitySignals {
            words: page.body.split_whitespace().count(),
            ..page.quality
        },
        snippet_allowed: true,
        max_snippet: None,
    }
}

struct Searchable {
    index: Index,
    crawl_dir: PathBuf,
}

impl Searchable {
    fn run(&mut self, raw: &str) -> Results {
        let query = parse::parse(raw);
        search(&mut self.index, &query, &SearchOptions::default()).expect("search should succeed")
    }

    /// The same query with proximity scored for every candidate.
    fn run_exact(&mut self, raw: &str) -> Results {
        let query = parse::parse(raw);
        let options = SearchOptions {
            rescore_depth: None,
            ..SearchOptions::default()
        };
        search(&mut self.index, &query, &options).expect("search should succeed")
    }

    /// URLs of the hits, in rank order.
    fn urls(&mut self, raw: &str) -> Vec<String> {
        let results = self.run(raw);
        let mut store = StoreReader::open(&self.crawl_dir).unwrap();
        results
            .hits
            .iter()
            .map(|hit| store.get(hit.crawl_doc).unwrap().url)
            .collect()
    }
}

fn prepare(name: &str, docs_per_segment: usize) -> Searchable {
    let root = std::env::temp_dir().join(format!("uruk-e2e-query-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let crawl_dir = root.join("crawl");
    let index_dir = root.join("index");

    let mut writer = StoreWriter::create(&crawl_dir).unwrap();
    for page in &corpus() {
        writer.push(&record(page)).unwrap();
    }
    writer.finish(&CrawlSummary::default()).unwrap();

    build(&IndexConfig {
        crawl_dir: crawl_dir.clone(),
        out_dir: index_dir.clone(),
        docs_per_segment,
        progress: false,
    })
    .unwrap();

    Searchable {
        index: Index::open(&index_dir).unwrap(),
        crawl_dir,
    }
}

#[test]
fn a_single_term_finds_every_page_that_has_it() {
    let mut engine = prepare("single", 100);
    let urls = engine.urls("barley");
    assert_eq!(urls.len(), 2, "got {urls:?}");
    assert!(urls.iter().all(|url| url.contains("scribes.test")));
}

#[test]
fn two_terms_are_an_and_not_an_or() {
    // The default that the brief insists on. "sediment" is on one page only,
    // so anything mentioning both it and clay is nothing.
    let mut engine = prepare("and", 100);
    assert!(engine.urls("sediment clay").is_empty());
    assert_eq!(engine.urls("sediment harbour").len(), 1);
}

#[test]
fn an_absent_term_returns_nothing_quickly() {
    let mut engine = prepare("absent", 100);
    let results = engine.run("babylon");
    assert!(results.hits.is_empty());
    // The dictionary answered it; no posting list was read.
    assert_eq!(
        results.lists_read, 0,
        "an absent term should not cost a read"
    );
}

#[test]
fn a_title_match_outranks_a_passing_mention() {
    // Page 0 has "tablets" in title, headings, URL and body. Page 1 mentions
    // it once in passing. There is one right answer here.
    let mut engine = prepare("title-rank", 100);
    let urls = engine.urls("tablets");
    assert!(urls.len() >= 2, "got {urls:?}");
    assert!(urls[0].contains("clay-tablets"), "ranked {urls:?}");
}

#[test]
fn a_phrase_matches_only_when_the_words_are_adjacent() {
    let mut engine = prepare("phrase", 100);

    // Both words appear on the kilns page, far apart; the phrase does not.
    let loose = engine.urls("clay tablets");
    let strict = engine.urls("\"clay tablets\"");

    assert!(
        loose.iter().any(|url| url.contains("kilns.test")),
        "loose AND: {loose:?}"
    );
    assert!(
        !strict.iter().any(|url| url.contains("kilns.test")),
        "the phrase should not match scattered words: {strict:?}"
    );
    assert!(!strict.is_empty(), "the phrase does occur somewhere");
}

#[test]
fn exclusion_removes_pages() {
    let mut engine = prepare("exclude", 100);
    let all = engine.urls("clay");
    let without = engine.urls("clay -kiln");

    assert!(all.iter().any(|url| url.contains("kilns.test")));
    assert!(
        !without.iter().any(|url| url.contains("kilns.test")),
        "got {without:?}"
    );
    assert!(without.len() < all.len());
}

#[test]
fn a_phrase_can_be_excluded() {
    let mut engine = prepare("exclude-phrase", 100);
    let without = engine.urls("clay -\"clay tablets\"");
    assert!(
        !without.iter().any(|url| url.contains("cheap.test")),
        "the deals page is nothing but that phrase: {without:?}"
    );
}

#[test]
fn site_restricts_to_one_host() {
    let mut engine = prepare("site", 100);
    let urls = engine.urls("the site:scribes.test");
    assert!(!urls.is_empty());
    assert!(
        urls.iter().all(|url| url.contains("scribes.test")),
        "got {urls:?}"
    );

    // A host nothing came from returns nothing rather than everything.
    assert!(engine.urls("the site:nowhere.test").is_empty());
}

#[test]
fn a_site_filter_skips_segments_without_that_host() {
    // With one document per segment, the filter must skip four of five
    // segments before reading any posting list.
    let mut engine = prepare("site-skip", 1);
    let broad = engine.run("the").lists_read;
    let narrow = engine.run("the site:harbours.test").lists_read;
    assert!(
        narrow < broad,
        "site: read {narrow} lists, unfiltered read {broad}"
    );
}

#[test]
fn results_are_the_same_however_the_index_is_segmented() {
    // Segment boundaries are an implementation detail; if they changed
    // ranking, IDF or average length would be being computed per segment.
    let mut one = prepare("seg-one", 100);
    let mut many = prepare("seg-many", 1);
    assert_eq!(one.index.segment_count(), 1);
    assert_eq!(many.index.segment_count(), 5);

    for query in ["clay", "clay tablets", "barley", "\"clay tablets\""] {
        assert_eq!(
            one.urls(query),
            many.urls(query),
            "query {query:?} ranked differently"
        );
    }
}

#[test]
fn a_link_farm_ranks_below_an_article_that_says_less() {
    // The deals page repeats "clay tablets" nine times and has it in the
    // title. It should still lose: saturation caps what repetition buys, and
    // the content-quality proxies count against a page that is mostly chrome.
    let mut engine = prepare("linkfarm", 100);
    let urls = engine.urls("\"clay tablets\"");
    let farm = urls.iter().position(|url| url.contains("cheap.test"));
    let article = urls.iter().position(|url| url.contains("scribes.test"));

    assert!(article.is_some(), "the real article should match: {urls:?}");
    if let (Some(farm), Some(article)) = (farm, article) {
        assert!(
            article < farm,
            "the link farm outranked the article: {urls:?}"
        );
    }
}

#[test]
fn every_result_explains_itself() {
    let mut engine = prepare("explain", 100);
    let results = engine.run("clay tablets");
    assert!(!results.hits.is_empty());

    for hit in &results.hits {
        let explanation = &hit.explanation;
        // The breakdown must be the real thing, not a summary of it.
        let summed: f64 = explanation.signals().iter().map(|(_, value)| value).sum();
        assert!(
            (hit.score - summed).abs() < 1e-12,
            "signals do not sum to the score"
        );

        // Both query terms are accounted for, with their own numbers.
        assert_eq!(explanation.terms.len(), 2);
        for term in &explanation.terms {
            assert!(term.idf > 0.0, "{} has no IDF", term.term);
            assert!(term.doc_frequency > 0);
            assert!(term.contribution >= 0.0);
        }
        assert!(explanation.quality.factor >= 0.0 && explanation.quality.factor <= 1.0);
    }
}

#[test]
fn hits_come_back_in_descending_score_order() {
    let mut engine = prepare("order", 100);
    let results = engine.run("clay");
    let scores: Vec<f64> = results.hits.iter().map(|hit| hit.score).collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "out of order: {scores:?}"
    );
}

#[test]
fn the_limit_is_respected() {
    let mut engine = prepare("limit", 100);
    let query = parse::parse("clay");
    let options = SearchOptions {
        limit: 1,
        ..SearchOptions::default()
    };
    let results = search(&mut engine.index, &query, &options).unwrap();

    assert_eq!(results.hits.len(), 1);
    // But the number that matched is still reported honestly.
    assert!(results.matched > 1, "matched {}", results.matched);
}

#[test]
fn snippets_come_from_the_stored_text_and_show_the_match() {
    let mut engine = prepare("snippet", 100);
    let results = engine.run("barley");
    let hit = results.hits.first().expect("barley should match");

    let mut store = StoreReader::open(&engine.crawl_dir).unwrap();
    let record = store.get(hit.crawl_doc).unwrap();
    let policy = SnippetPolicy {
        allowed: record.snippet_allowed,
        max_chars: record.max_snippet,
    };
    let found = snippet::snippet(&record.text, &["barley"], policy, snippet::DEFAULT_LENGTH);

    assert!(
        found.text.to_lowercase().contains("barley"),
        "got: {}",
        found.text
    );
    assert!(!found.highlights.is_empty());
}

#[test]
fn an_empty_query_searches_for_nothing() {
    let mut engine = prepare("empty", 100);
    assert!(engine.run("").hits.is_empty());
    assert!(engine.run("   ").hits.is_empty());
    // Only a filter is not a search.
    assert!(engine.run("site:scribes.test").hits.is_empty());
}

#[test]
fn stopwords_are_searchable_because_they_were_indexed() {
    // The decision in RESEARCH.md 5.7, end to end: a phrase made entirely of
    // common words has to work, or quoted search is a lie.
    let mut engine = prepare("stopwords", 100);
    let urls = engine.urls("\"a long argument with sediment\"");
    assert_eq!(urls.len(), 1, "got {urls:?}");
    assert!(urls[0].contains("harbours.test"));
}

#[test]
fn the_rescoring_cut_does_not_change_the_results_it_returns() {
    // The engine scores proximity for the best hundred candidates rather than
    // all of them, because doing it for all of them was half the cost of the
    // worst query measured (`RESEARCH.md` §6b). That is an approximation, and
    // an approximation nobody checks is a bug with a good excuse.
    //
    // This corpus is far smaller than the cut, so every candidate is rescored
    // either way and the two must agree exactly — which is the point: it pins
    // the invariant that the cut only ever removes candidates that were going
    // to lose anyway, and it will fail loudly if the cut is ever applied
    // before the other signals have ranked the field.
    let mut engine = prepare("rescore_cut_agrees", 64);

    for query in [
        "clay tablets",
        "\"clay tablets\"",
        "tablets barley",
        "scribes -barley",
        "site:scribes.test tablets",
        "tablets",
        "barley rations temple",
    ] {
        let approximate = engine.run(query);
        let exact = engine.run_exact(query);

        let approximate_docs: Vec<u32> = approximate.hits.iter().map(|h| h.crawl_doc).collect();
        let exact_docs: Vec<u32> = exact.hits.iter().map(|h| h.crawl_doc).collect();
        assert_eq!(
            approximate_docs, exact_docs,
            "the rescoring cut changed the results for {query:?}"
        );
        assert_eq!(
            approximate.matched, exact.matched,
            "the match count changed for {query:?}, so it is counting scored hits \
             rather than matches"
        );

        for (a, b) in approximate.hits.iter().zip(&exact.hits) {
            assert!(
                (a.score - b.score).abs() < 1e-12,
                "the same document scored differently for {query:?}: {} vs {}",
                a.score,
                b.score
            );
        }
    }
}

/// A corpus of identical pages, so every score ties.
///
/// Ties are where a selection and a sort disagree if they do not use the same
/// order, and real corpora produce them constantly: any two pages with the
/// same words in the same proportions score the same.
fn tied_corpus(name: &str, pages: usize, docs_per_segment: usize) -> Searchable {
    let root = std::env::temp_dir().join(format!("uruk-e2e-tied-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let crawl_dir = root.join("crawl");
    let index_dir = root.join("index");

    let mut writer = StoreWriter::create(&crawl_dir).unwrap();
    for i in 0..pages {
        writer
            .push(&Record {
                url: format!("https://tied.test/page/{i}"),
                final_url: format!("https://tied.test/page/{i}"),
                fetched_at: 0,
                status: 200,
                depth: 1,
                fingerprint: i as u64,
                title: String::from("Clay tablets"),
                text: String::from("Clay tablets from Uruk record barley rations in cuneiform."),
                headings: vec![String::from("Clay tablets")],
                lang: Some(String::from("en")),
                links: Vec::new(),
                quality: QualitySignals {
                    text_ratio: 0.45,
                    link_density: 0.05,
                    scripts: 1,
                    words: 9,
                },
                snippet_allowed: true,
                max_snippet: None,
            })
            .unwrap();
    }
    writer.finish(&CrawlSummary::default()).unwrap();

    build(&IndexConfig {
        crawl_dir: crawl_dir.clone(),
        out_dir: index_dir.clone(),
        docs_per_segment,
        progress: false,
    })
    .unwrap();

    Searchable {
        index: Index::open(&index_dir).unwrap(),
        crawl_dir,
    }
}

#[test]
fn tied_scores_rank_by_the_rule_and_not_by_arrival_order() {
    // The ranking rule says: best score first, ties to the lower document id.
    // If the top-k heap admits only on a strictly better score, a hit that
    // ties with the current worst is turned away — so which of several
    // equal-scoring documents survives depends on the order they arrive in,
    // which depends on how the index happens to be segmented.
    //
    // Every page here scores identically, so the answer is entirely decided
    // by the tie-break, and the segmentation is varied to change arrival
    // order without changing the corpus. The result must not move.
    let mut expected: Option<Vec<u32>> = None;
    for docs_per_segment in [500, 64, 32, 7] {
        let mut engine = tied_corpus(&format!("arrival{docs_per_segment}"), 200, docs_per_segment);
        let page: Vec<u32> = engine
            .run("clay tablets")
            .hits
            .iter()
            .map(|hit| hit.crawl_doc)
            .collect();
        assert_eq!(page.len(), 10);
        match &expected {
            None => expected = Some(page),
            Some(first) => assert_eq!(
                &page, first,
                "segmenting at {docs_per_segment} changed which tied documents won"
            ),
        }
    }
    // And the rule itself: with everything tied, the lowest document ids win.
    assert_eq!(expected.unwrap(), (0..10).collect::<Vec<u32>>());
}

#[test]
fn the_rescoring_cut_survives_a_corpus_where_everything_ties() {
    // The case that caught a real bug. When every candidate scores the same,
    // which hundred the cut keeps is decided entirely by the tie-break — and
    // if that differs from the tie-break the final ranking uses, the page
    // changes depending on whether the cut is enabled. It did, for one commit.
    //
    // Two hundred identical pages against a cut of twelve, so the cut bites
    // hard and every decision it makes is a tie.
    let mut engine = tied_corpus("ties", 200, 1000);

    for query in [
        "clay",
        "clay tablets",
        "\"clay tablets\"",
        "tablets -mesopotamia",
    ] {
        let query_parsed = parse::parse(query);
        let cut = SearchOptions {
            limit: 10,
            rescore_depth: Some(12),
            ..SearchOptions::default()
        };
        let exact = SearchOptions {
            limit: 10,
            rescore_depth: None,
            ..SearchOptions::default()
        };

        let with_cut = search(&mut engine.index, &query_parsed, &cut).expect("search");
        let without = search(&mut engine.index, &query_parsed, &exact).expect("search");

        let a: Vec<u32> = with_cut.hits.iter().map(|h| h.crawl_doc).collect();
        let b: Vec<u32> = without.hits.iter().map(|h| h.crawl_doc).collect();
        assert_eq!(
            a, b,
            "the cut changed the page for {query:?} on an all-ties corpus"
        );
        assert_eq!(
            with_cut.matched, without.matched,
            "match count for {query:?}"
        );
    }
}
