//! What the link graph is worth is decided by which links it refuses to count.
//!
//! These tests are built around a crawl store written on the fly, because the
//! interesting cases are all about *shapes of linking* — a farm, a footer, a
//! comment section — and those are easier to state as a site than as a fixture
//! file.

use std::path::PathBuf;

use uruk_crawl::store::{OutLink, QualitySignals, Record, StoreReader, StoreWriter};
use uruk_link::authority::{Authority, DEFAULT_DAMPING, Method, rank_correlation};
use uruk_link::graph::HostGraph;

fn page(url: &str, links: &[(&str, bool)]) -> Record {
    Record {
        url: url.to_string(),
        final_url: url.to_string(),
        fetched_at: 0,
        status: 200,
        depth: 0,
        fingerprint: 0,
        title: String::from("a page"),
        text: String::from("some words that do not matter to the link graph"),
        headings: Vec::new(),
        lang: None,
        links: links
            .iter()
            .map(|&(url, nofollow)| OutLink {
                url: url.to_string(),
                anchor: String::from("link"),
                nofollow,
            })
            .collect(),
        quality: QualitySignals {
            text_ratio: 0.5,
            link_density: 0.1,
            scripts: 0,
            words: 9,
        },
        snippet_allowed: true,
        max_snippet: None,
    }
}

/// Write `pages` to a fresh store and build a graph over it.
fn graph_of(name: &str, pages: Vec<Record>, seeds: &[&str]) -> HostGraph {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let mut writer = StoreWriter::create(&dir).expect("store");
    for page in pages {
        writer.push(&page).expect("push");
    }
    writer
        .finish(&uruk_crawl::store::CrawlSummary::default())
        .expect("finish");

    let mut reader = StoreReader::open(&dir).expect("reopen");
    let seeds: Vec<String> = seeds.iter().map(|&s| s.to_string()).collect();
    HostGraph::build(&mut reader, &seeds).expect("graph")
}

#[test]
fn a_thousand_links_from_one_host_are_still_one_vote() {
    // The cheapest attack on any link-counting signal: generate pages. If
    // in-degree counted links rather than distinct hosts, a farm of 50 pages
    // would outrank a site linked by three independent people.
    let mut pages = Vec::new();
    for i in 0..50 {
        pages.push(page(
            &format!("https://farm.example/p{i}.html"),
            &[("https://target.example/", false)],
        ));
    }
    // Three independent hosts, one link each.
    for host in ["one.test", "two.test", "three.test"] {
        pages.push(page(
            &format!("https://{host}/index.html"),
            &[("https://honest.example/", false)],
        ));
    }

    let graph = graph_of("one_vote", pages, &[]);
    let target = graph.id("target.example").expect("target host");
    let honest = graph.id("honest.example").expect("honest host");

    assert_eq!(
        graph.node(target).in_degree(),
        1,
        "the farm bought 50 votes"
    );
    assert_eq!(graph.node(honest).in_degree(), 3);

    let authority = Authority::compute(&graph, Method::InDegree, DEFAULT_DAMPING);
    assert!(
        authority.score("honest.example") > authority.score("target.example"),
        "three independent hosts should beat one host with fifty pages"
    );
}

#[test]
fn a_site_cannot_vote_for_itself_through_subdomains() {
    let pages = vec![
        page(
            "https://blog.example.com/",
            &[("https://shop.example.com/", false)],
        ),
        page(
            "https://shop.example.com/",
            &[("https://blog.example.com/", false)],
        ),
        page(
            "https://www.example.com/",
            &[("https://shop.example.com/", false)],
        ),
    ];
    let graph = graph_of("self_vote", pages, &[]);
    let shop = graph.id("shop.example.com").expect("shop host");
    assert_eq!(
        graph.node(shop).in_degree(),
        0,
        "a subdomain voted for its own site"
    );
    assert!(graph.dropped.same_site >= 3);
}

#[test]
fn nofollow_links_do_not_pass_authority() {
    // A comment section linking out is not the host vouching for the target.
    let pages = vec![
        page("https://forum.test/thread", &[("https://spam.test/", true)]),
        page("https://blog.test/post", &[("https://good.test/", false)]),
    ];
    let graph = graph_of("nofollow", pages, &[]);
    // The target is not a node with zero in-links; it is not a node at all.
    // A nofollow link is the only thing that pointed at it, and that is not
    // evidence the host exists as far as authority is concerned.
    assert_eq!(graph.id("spam.test"), None);
    assert_eq!(
        graph.id("good.test").map(|id| graph.node(id).in_degree()),
        Some(1)
    );
    assert_eq!(graph.dropped.nofollow, 1);
}

#[test]
fn a_sitewide_footer_link_counts_once_per_source_host() {
    // Every page of a site links to its host's favourite charity. That is one
    // host vouching, not two hundred.
    let pages: Vec<Record> = (0..20)
        .map(|i| {
            page(
                &format!("https://sitewide.test/page{i}"),
                &[
                    ("https://charity.test/", false),
                    ("https://charity.test/about", false),
                ],
            )
        })
        .collect();
    let graph = graph_of("footer", pages, &[]);
    let charity = graph.id("charity.test").expect("charity host");
    assert_eq!(graph.node(charity).in_degree(), 1);
    // The edge remembers that 20 pages made the link, for a human reading the
    // graph — but the algorithms never look at it.
    let source = graph.id("sitewide.test").expect("source host");
    assert_eq!(graph.node(charity).incoming.get(&source), Some(&20));
    assert_eq!(graph.dropped.repeat, 20, "the second link on each page");
}

#[test]
fn trust_flows_from_the_seeds_and_not_backwards_for_free() {
    // A chain: seed -> middle -> far, plus an unconnected host that nobody
    // links to. The unconnected host must not end up with seed-level trust.
    let pages = vec![
        page("https://seed.test/", &[("https://middle.test/", false)]),
        page("https://middle.test/", &[("https://far.test/", false)]),
        page("https://far.test/", &[]),
        page("https://orphan.test/", &[]),
    ];
    let graph = graph_of("trust_chain", pages, &["https://seed.test/"]);
    let authority = Authority::compute(&graph, Method::TrustRank, DEFAULT_DAMPING);

    assert!(
        authority.convergence.converged,
        "power iteration did not converge in {} steps",
        authority.convergence.iterations
    );
    assert!(!authority.convergence.trust_root_was_empty);

    let seed = authority.hosts["seed.test"].trust;
    let middle = authority.hosts["middle.test"].trust;
    let far = authority.hosts["far.test"].trust;
    let orphan = authority.hosts["orphan.test"].trust;

    assert!(seed > middle, "seed {seed} should outrank middle {middle}");
    assert!(middle > far, "middle {middle} should outrank far {far}");
    assert!(far > orphan, "far {far} should outrank orphan {orphan}");
}

#[test]
fn the_iteration_cap_is_high_enough_for_the_damping_it_is_used_with() {
    // This is the bug this file caught: a cap of 100 iterations against a
    // requirement of 128 meant `converged` could never be true, and the scores
    // were close enough that nothing else would have noticed.
    for damping in [0.5, 0.85, 0.9, 0.95] {
        let pages = vec![
            page("https://a.test/", &[("https://b.test/", false)]),
            page("https://b.test/", &[("https://c.test/", false)]),
            page("https://c.test/", &[("https://a.test/", false)]),
        ];
        let name = format!("cap{}", damping.to_string().replace('.', "_"));
        let graph = graph_of(&name, pages, &["https://a.test/"]);
        let authority = Authority::compute(&graph, Method::TrustRank, damping);
        assert!(
            authority.convergence.converged,
            "damping {damping} needed more than {} iterations",
            authority.convergence.iterations
        );
    }
}

#[test]
fn trust_is_a_distribution_and_stays_one() {
    let pages = vec![
        page(
            "https://a.test/",
            &[("https://b.test/", false), ("https://c.test/", false)],
        ),
        page("https://b.test/", &[("https://c.test/", false)]),
        page("https://c.test/", &[("https://a.test/", false)]),
    ];
    let graph = graph_of("distribution", pages, &["https://a.test/"]);
    let authority = Authority::compute(&graph, Method::TrustRank, DEFAULT_DAMPING);
    let total: f64 = authority.hosts.values().map(|host| host.trust).sum();
    assert!(
        (total - 1.0).abs() < 1e-9,
        "trust summed to {total}, so mass is leaking"
    );
}

#[test]
fn a_crawl_with_no_seeds_says_so_rather_than_pretending() {
    // TrustRank without a trust root is PageRank. The result is not wrong, but
    // calling it TrustRank would be.
    let pages = vec![page("https://a.test/", &[("https://b.test/", false)])];
    let graph = graph_of("no_seeds", pages, &[]);
    let authority = Authority::compute(&graph, Method::TrustRank, DEFAULT_DAMPING);
    assert!(authority.convergence.trust_root_was_empty);
}

#[test]
fn a_www_host_from_the_index_finds_its_normalised_entry() {
    // The index keeps hosts as the crawler saw them; the graph collapsed
    // `www.` away when it was built. If `score` did not normalise, every www
    // host in the corpus would silently lose this signal.
    let pages = vec![
        page(
            "https://someone.test/",
            &[("https://www.target.test/", false)],
        ),
        page("https://www.target.test/", &[]),
    ];
    let graph = graph_of("www_lookup", pages, &[]);
    let authority = Authority::compute(&graph, Method::InDegree, DEFAULT_DAMPING);
    assert!(authority.hosts.contains_key("target.test"));
    assert!(!authority.hosts.contains_key("www.target.test"));
    assert!(
        authority.score("www.target.test") > 0.0,
        "a www host looked up as the index stores it scored zero"
    );
    assert!(
        (authority.score("www.target.test") - authority.score("target.test")).abs() < f64::EPSILON
    );
}

#[test]
fn an_unknown_host_scores_zero_rather_than_average() {
    let pages = vec![page("https://a.test/", &[("https://b.test/", false)])];
    let graph = graph_of("unknown", pages, &[]);
    let authority = Authority::compute(&graph, Method::InDegree, DEFAULT_DAMPING);
    assert!(authority.score("never-seen.test").abs() < f64::EPSILON);
}

#[test]
fn the_two_methods_can_be_compared_at_all() {
    // §5.3 says in-degree is the baseline to beat. "Beat" needs the two
    // rankings to be comparable, which is what this asserts: same hosts, a
    // defined correlation, and a real disagreement on a graph built to produce
    // one.
    let mut pages = vec![
        page("https://seed.test/", &[("https://trusted.test/", false)]),
        page("https://trusted.test/", &[]),
    ];
    // A host linked by many low-value hosts that the seed cannot reach.
    for i in 0..6 {
        pages.push(page(
            &format!("https://random{i}.test/"),
            &[("https://popular.test/", false)],
        ));
    }
    pages.push(page("https://popular.test/", &[]));

    let graph = graph_of("compare", pages, &["https://seed.test/"]);
    let by_degree = Authority::compute(&graph, Method::InDegree, DEFAULT_DAMPING);
    let by_trust = Authority::compute(&graph, Method::TrustRank, DEFAULT_DAMPING);

    let degree_ranking = by_degree.ranking();
    let trust_ranking = by_trust.ranking();
    let correlation = rank_correlation(&degree_ranking, &trust_ranking).expect("comparable");
    assert!(
        (-1.0..=1.0).contains(&correlation),
        "correlation {correlation} is not a correlation"
    );

    // In-degree prefers the popular host; trust prefers the one the seed
    // reaches. That disagreement is the whole reason both are computed.
    assert!(by_degree.score("popular.test") > by_degree.score("trusted.test"));
    assert!(by_trust.score("trusted.test") > by_trust.score("popular.test"));
}
