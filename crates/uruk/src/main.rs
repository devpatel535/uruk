//! `uruk` — the command-line entry point.
//!
//! One subcommand per component: `crawl`, `index`, `search` and `serve`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use url::Url;

use uruk_crawl::crawler::{self, CrawlConfig, DEFAULT_USER_AGENT};
use uruk_crawl::store::{CrawlSummary, StoreReader};
use uruk_crawl::traps::Limits;
use uruk_eval::compare::{self, Comparison};
use uruk_eval::judgments::Judgments;
use uruk_eval::metrics::{self, Summary};
use uruk_eval::run::{Configuration, Signal, evaluate};
use uruk_index::build::{self, IndexConfig};
use uruk_index::index::Index;
use uruk_index::merge;
use uruk_link::authority::{self, Authority, Method, rank_correlation};
use uruk_link::graph::HostGraph;
use uruk_query::parse;
use uruk_query::search::{self, SearchOptions};
use uruk_query::snippet::{self, SnippetPolicy};
use uruk_serve::server::{self, ServeConfig};

const TAGLINE: &str = "A search engine that returns links, not answers.";

#[derive(Debug, Parser)]
#[command(name = "uruk", version, about = TAGLINE, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Fetch pages politely, extract their text, and store it compressed.
    Crawl(CrawlArgs),
    /// Turn a crawl into a searchable index.
    Index(IndexArgs),
    /// Combine an index's segments, so a query reads fewer of them.
    Merge(MergeArgs),
    /// Build the host link graph and score every host's authority.
    Link(LinkArgs),
    /// Score a ranking configuration against a judged query set.
    Eval(EvalArgs),
    /// Search an index.
    Search(SearchArgs),
    /// Serve the web front end.
    Serve(ServeArgs),
}

#[derive(Debug, clap::Args)]
struct MergeArgs {
    /// Directory holding the index to merge.
    #[arg(short, long, value_name = "DIR", default_value = "data/index")]
    index: PathBuf,

    /// Where to write the merged index.
    ///
    /// A separate directory on purpose: merging in place would leave the index
    /// unreadable if it failed half-way, and an index costs too much to
    /// rebuild for that to be a reasonable risk. Swap the directories
    /// afterwards, as DEPLOYING.md describes for a refresh.
    #[arg(short, long, value_name = "DIR")]
    out: PathBuf,

    /// Most documents in an output segment. Bounds how much is held in memory.
    #[arg(long, default_value_t = 1_000_000)]
    docs_per_segment: usize,

    /// Suppress progress output.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Debug, clap::Args)]
struct EvalArgs {
    /// The judged query set. See `crates/uruk-eval/src/judgments.rs` for the
    /// format; it is plain text and meant to be edited by hand.
    #[arg(short, long, value_name = "FILE")]
    judgments: PathBuf,

    /// Directory holding the index.
    #[arg(short, long, value_name = "DIR", default_value = "data/index")]
    index: PathBuf,

    /// Directory holding the crawl store. Judgments are matched by URL, which
    /// lives in the crawl store rather than the index.
    #[arg(short, long, value_name = "DIR", default_value = "data/crawl")]
    crawl: PathBuf,

    /// Results to score per query. Ten, because that is the product: a
    /// brilliant result at position eleven did not help anybody.
    #[arg(short = 'n', long, default_value_t = metrics::DEFAULT_DEPTH)]
    depth: usize,

    /// Also run the engine with this signal switched off, and report whether
    /// the difference is detectable. Repeatable.
    #[arg(long = "without", value_enum)]
    without: Vec<SignalArg>,

    /// Also compare the two authority methods against each other.
    #[arg(long)]
    compare_authority: bool,

    /// Show the per-query scores, worst first. For finding the queries a
    /// change actually broke.
    #[arg(short, long)]
    per_query: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SignalArg {
    Authority,
    Proximity,
    Quality,
}

impl From<SignalArg> for Signal {
    fn from(value: SignalArg) -> Self {
        match value {
            SignalArg::Authority => Self::Authority,
            SignalArg::Proximity => Self::Proximity,
            SignalArg::Quality => Self::Quality,
        }
    }
}

#[derive(Debug, clap::Args)]
struct LinkArgs {
    /// Directory holding a crawl store.
    #[arg(short, long, value_name = "DIR", default_value = "data/crawl")]
    crawl: PathBuf,

    /// The crawl's seed file. Its hosts become the trust root for trustrank;
    /// without it there is nothing to propagate from and the result degrades
    /// to plain pagerank, which the report says plainly rather than hiding.
    #[arg(short, long, value_name = "FILE")]
    seeds: Option<PathBuf>,

    /// Where to write the authority table. Defaults to `authority.json` inside
    /// the crawl directory, which is where `search` and `serve` look for it.
    #[arg(short, long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Which signal to store as the one the ranker should use.
    ///
    /// Both are always computed. In-degree is the default because
    /// RESEARCH.md 5.3 puts the burden of proof on the expensive method, and
    /// the judged query set that would settle it does not exist yet.
    #[arg(short, long, value_enum, default_value_t = MethodArg::InDegree)]
    method: MethodArg,

    /// Random-surfer damping for trustrank.
    #[arg(long, default_value_t = authority::DEFAULT_DAMPING)]
    damping: f64,

    /// Hosts to list in the report.
    #[arg(short = 'n', long, default_value_t = 20)]
    top: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum MethodArg {
    /// Distinct hosts linking here. The baseline.
    InDegree,
    /// Trust propagated from the crawl's seeds.
    TrustRank,
}

impl From<MethodArg> for Method {
    fn from(value: MethodArg) -> Self {
        match value {
            MethodArg::InDegree => Self::InDegree,
            MethodArg::TrustRank => Self::TrustRank,
        }
    }
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Directory holding the index.
    #[arg(short, long, value_name = "DIR", default_value = "data/index")]
    index: PathBuf,

    /// Directory holding the crawl store, for titles and snippets.
    #[arg(short, long, value_name = "DIR", default_value = "data/crawl")]
    crawl: PathBuf,

    /// Address to listen on. Loopback by default: putting a search engine on
    /// a public address should be a decision, not an accident.
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    address: String,

    /// Results per page.
    #[arg(short = 'n', long, default_value_t = 10)]
    limit: usize,

    /// Show each result's score breakdown on the page. For tuning; off by
    /// default, because the product is ten links and nothing else.
    #[arg(short, long)]
    explain: bool,

    /// Searches to answer at once before turning requests away with a 503.
    ///
    /// Counts requests, not requesters: a per-visitor limit would need a table
    /// of who is asking, which is the thing the privacy page says does not
    /// exist. One heavy user can therefore use the whole allowance; that
    /// trade is deliberate and DEPLOYING.md explains it.
    #[arg(long, default_value_t = server::DEFAULT_MAX_CONCURRENT_SEARCHES)]
    max_concurrent_searches: usize,
}

#[derive(Debug, clap::Args)]
struct IndexArgs {
    /// Directory holding a crawl store.
    #[arg(short, long, value_name = "DIR", default_value = "data/crawl")]
    crawl: PathBuf,

    /// Directory to write index segments into.
    #[arg(short, long, value_name = "DIR", default_value = "data/index")]
    out: PathBuf,

    /// Documents per segment. Bounds peak memory while indexing.
    #[arg(long, default_value_t = build::DEFAULT_DOCS_PER_SEGMENT)]
    docs_per_segment: usize,

    /// Suppress progress output.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Debug, clap::Args)]
struct SearchArgs {
    /// The query. Supports "quoted phrases", -exclusion and site:host.
    ///
    /// `allow_hyphen_values` is what makes `-exclusion` work: without it the
    /// argument parser takes `-barley` for an unknown flag and refuses the
    /// query, which would quietly break one of the four operators the brief
    /// asks for.
    #[arg(value_name = "QUERY", num_args = 1.., required = true, allow_hyphen_values = true)]
    query: Vec<String>,

    /// Directory holding the index.
    #[arg(short, long, value_name = "DIR", default_value = "data/index")]
    index: PathBuf,

    /// Directory holding the crawl store, for titles and snippets.
    #[arg(short, long, value_name = "DIR", default_value = "data/crawl")]
    crawl: PathBuf,

    /// Results to show. Ten, because that is the product.
    #[arg(short = 'n', long, default_value_t = 10)]
    limit: usize,

    /// Show the full per-signal score breakdown for every result.
    #[arg(short, long)]
    explain: bool,
}

#[derive(Debug, clap::Args)]
struct CrawlArgs {
    /// File of seed URLs, one per line. Blank lines and `#` comments ignored.
    #[arg(short, long, value_name = "FILE")]
    seeds: PathBuf,

    /// Directory to write the crawl store into.
    #[arg(short, long, value_name = "DIR", default_value = "data/crawl")]
    out: PathBuf,

    /// Stop once this many pages have been stored.
    #[arg(short = 'n', long, default_value_t = 1_000)]
    max_pages: usize,

    /// Seconds between requests to any one host.
    ///
    /// Lowering this is the single easiest way to get the crawler blocked.
    #[arg(long, value_name = "SECONDS", default_value_t = 3.0)]
    delay: f64,

    /// Requests in flight at once, across *different* hosts. One host is never
    /// asked for two things at the same time regardless of this value.
    #[arg(short, long, default_value_t = 8)]
    concurrency: usize,

    /// How many links from a seed to follow.
    #[arg(long, default_value_t = 4)]
    max_depth: u32,

    /// Most pages to take from any single host.
    #[arg(long, default_value_t = 5_000)]
    max_per_host: usize,

    /// Override the user-agent. Keep a contact URL in it.
    #[arg(long, default_value = DEFAULT_USER_AGENT)]
    user_agent: String,

    /// Suppress progress output.
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Crawl(args) => finish(run_crawl(args)),
        Command::Index(args) => finish(run_index(&args)),
        Command::Merge(args) => finish(run_merge(&args)),
        Command::Link(args) => finish(run_link(&args)),
        Command::Eval(args) => finish(run_eval(&args)),
        Command::Search(args) => finish(run_search(&args)),
        Command::Serve(args) => finish(run_serve(&args)),
    }
}

fn run_serve(args: &ServeArgs) -> Result<(), String> {
    let address = args
        .address
        .parse()
        .map_err(|error| format!("{:?} is not an address to listen on: {error}", args.address))?;

    let config = ServeConfig {
        index_dir: args.index.clone(),
        crawl_dir: args.crawl.clone(),
        address,
        results_per_page: args.limit.max(1),
        explain: args.explain,
        user_agent: DEFAULT_USER_AGENT.to_owned(),
        max_concurrent_searches: args.max_concurrent_searches.max(1),
    };

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start the async runtime: {error}"))?;
    runtime
        .block_on(server::run(&config))
        .map_err(|error| format!("server failed: {error}"))
}

fn finish(outcome: Result<(), String>) -> ExitCode {
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("uruk: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run_index(args: &IndexArgs) -> Result<(), String> {
    let manifest = build::build(&IndexConfig {
        crawl_dir: args.crawl.clone(),
        out_dir: args.out.clone(),
        docs_per_segment: args.docs_per_segment.max(1),
        progress: !args.quiet,
    })
    .map_err(|error| format!("indexing failed: {error}"))?;

    println!("\nindex built");
    println!("  documents          {}", manifest.documents);
    println!("  segments           {}", manifest.segments.len());
    println!("  distinct terms     {}", manifest.terms);
    println!("  postings           {}", manifest.postings);
    if manifest.skipped_empty > 0 {
        println!("  skipped (no text)  {}", manifest.skipped_empty);
    }

    println!(
        "\n  text indexed       {}",
        human_bytes(manifest.text_bytes)
    );
    println!("  index on disk      {}", human_bytes(manifest.bytes_total));
    println!(
        "    postings         {}",
        human_bytes(manifest.bytes_postings)
    );
    println!(
        "    dictionary       {}",
        human_bytes(manifest.bytes_dictionary)
    );
    println!("    hosts            {}", human_bytes(manifest.bytes_hosts));
    println!(
        "    doc table        {}",
        human_bytes(manifest.bytes_doc_table)
    );

    // The number RESEARCH.md section 6 argues about, measured rather than
    // estimated. The brief hoped for 15-25% of the text.
    if manifest.text_bytes > 0 {
        println!(
            "\n  index is {:.0}% of the text it describes",
            manifest.size_ratio() * 100.0
        );
        if manifest.documents > 0 {
            println!(
                "  {} per indexed page",
                human_bytes(manifest.bytes_total / u64::from(manifest.documents))
            );
        }
    }
    println!("\nwritten to {}", args.out.display());
    Ok(())
}

/// Score a ranking configuration against a judged query set.
///
/// The point of this command is that it makes a ranking argument settleable.
/// Every weight in the scorer is currently a starting value chosen by reading,
/// not by measurement, and `RESEARCH.md` §5.4 says plainly that tuning them by
/// eye is guessing. This is how the guessing stops.
/// Combine an index's segments.
///
/// A query reads every segment's dictionary and, for each term, one posting
/// list per segment. Fewer segments is straightforwardly less work per query,
/// and the only reason an index has several is that building one bounds
/// memory.
fn run_merge(args: &MergeArgs) -> Result<(), String> {
    if args.out == args.index {
        return Err(String::from(
            "--out must differ from --index: merging in place would destroy the \
             index if it failed part-way through",
        ));
    }

    let before = build::read_manifest(&args.index).map_err(|error| {
        format!(
            "could not read the index at {}: {error}",
            args.index.display()
        )
    })?;

    let merged = merge::merge(&merge::MergeConfig {
        index_dir: args.index.clone(),
        out_dir: args.out.clone(),
        docs_per_segment: args.docs_per_segment.max(1),
        progress: !args.quiet,
    })
    .map_err(|error| format!("merge failed: {error}"))?;

    println!();
    println!(
        "  segments           {} -> {}",
        before.segments.len(),
        merged.segments.len()
    );
    println!("  documents          {}", merged.documents);
    println!("  postings           {}", merged.postings);
    println!(
        "  index on disk      {} -> {}",
        human_bytes(before.bytes_total),
        human_bytes(merged.bytes_total)
    );
    println!(
        "    dictionary       {} -> {}",
        human_bytes(before.bytes_dictionary),
        human_bytes(merged.bytes_dictionary)
    );

    // The count is what a query actually pays: a three-word search reads three
    // posting lists per segment.
    println!(
        "\n  a three-word query now reads up to {} posting lists, not {}",
        3 * merged.segments.len(),
        3 * before.segments.len()
    );
    println!("\nwritten to {}", args.out.display());
    println!(
        "the original is untouched at {}; swap the directories once you have \
         checked it",
        args.index.display()
    );
    Ok(())
}

fn run_eval(args: &EvalArgs) -> Result<(), String> {
    let judgments = Judgments::load(&args.judgments)
        .map_err(|error| format!("could not read the judged query set: {error}"))?;
    if judgments.is_empty() {
        return Err(format!("{} contains no queries", args.judgments.display()));
    }

    let mut index = Index::open(&args.index).map_err(|error| {
        format!(
            "could not open the index at {}: {error}",
            args.index.display()
        )
    })?;
    let mut store = StoreReader::open(&args.crawl).map_err(|error| {
        format!(
            "could not open the crawl store at {}: {error}",
            args.crawl.display()
        )
    })?;
    let authority = Authority::beside_crawl(&args.crawl)
        .map_err(|error| format!("could not read the authority table: {error}"))?;

    let mut baseline = Configuration::baseline(authority.clone());
    baseline.depth = args.depth.max(1);

    let run = evaluate(&mut index, &mut store, &judgments, &baseline)
        .map_err(|error| format!("evaluation failed: {error}"))?;

    println!();
    println!("  judged queries     {}", judgments.len());
    println!("  graded documents   {}", judgments.graded());
    println!("  evaluation depth   {}", baseline.depth);
    report_summary("baseline", &run.summary);

    if authority.is_none() {
        println!(
            "\n  note: no authority table beside the crawl, so this is text-only ranking.\n\
             \x20       run `uruk link` first to include host authority."
        );
    }

    if args.per_query {
        report_per_query(&judgments, &run.scores);
    }

    // Each variant is the same engine with one thing changed, which is the
    // only kind of comparison that can attribute a difference to a cause.
    for signal in &args.without {
        let variant = baseline.without((*signal).into());
        let other = evaluate(&mut index, &mut store, &judgments, &variant)
            .map_err(|error| format!("evaluation failed: {error}"))?;
        report_comparison(&variant.name, &run.scores, &other.scores);
    }

    if args.compare_authority {
        if authority.is_none() {
            println!("\n  cannot compare authority methods: no authority table");
        } else {
            let degree = baseline.using(Method::InDegree);
            let trust = baseline.using(Method::TrustRank);
            let a = evaluate(&mut index, &mut store, &judgments, &degree)
                .map_err(|error| format!("evaluation failed: {error}"))?;
            let b = evaluate(&mut index, &mut store, &judgments, &trust)
                .map_err(|error| format!("evaluation failed: {error}"))?;
            report_comparison("trustrank instead of in-degree", &a.scores, &b.scores);
        }
    }

    Ok(())
}

fn report_summary(name: &str, summary: &Summary) {
    println!("\n  {name}");
    println!("    nDCG@10          {:.4}", summary.ndcg);
    println!("    precision        {:.4}", summary.precision);
    println!("    MRR              {:.4}", summary.mean_reciprocal_rank);
    println!("    recall           {:.4}", summary.recall);
    println!("    judged coverage  {:.4}", summary.coverage);
    if summary.empty > 0 {
        println!(
            "    {} queries returned nothing: the corpus, not the ranking, is what is short",
            summary.empty
        );
    }
    // Coverage is the number that decides whether any of the above is
    // evidence. Below half, most of what the engine returned was scored as
    // irrelevant only because nobody had looked at it.
    if summary.coverage < 0.5 {
        println!(
            "    WARNING: under half of the returned results were judged at all.\n\
             \x20            These scores mostly measure how much of the corpus the\n\
             \x20            judge has seen, not how good the ranking is."
        );
    }
}

fn report_per_query(judgments: &Judgments, scores: &[uruk_eval::Scored]) {
    let mut rows: Vec<(&str, &uruk_eval::Scored)> = judgments
        .queries
        .iter()
        .map(|judged| judged.query.as_str())
        .zip(scores)
        .collect();
    rows.sort_by(|a, b| a.1.ndcg.total_cmp(&b.1.ndcg));

    println!("\n  per query, worst first");
    for (query, scored) in rows {
        println!(
            "    {:.4}  cov {:.2}  {}",
            scored.ndcg, scored.coverage, query
        );
    }
}

fn report_comparison(name: &str, baseline: &[uruk_eval::Scored], variant: &[uruk_eval::Scored]) {
    let result: Comparison = compare::compare(baseline, variant, compare::DEFAULT_TRIALS);
    println!("\n  {name}");
    println!(
        "    nDCG {:.4} -> {:.4}  ({:+.4})",
        result.baseline, result.variant, result.difference
    );
    println!("    {}", result.verdict());
}

/// Build the host link graph and write the authority table.
///
/// The report is deliberately noisy about what it *discarded*. A link graph
/// that looks healthy while silently dropping every edge is the failure mode
/// here, and the only way to notice is to print the counts.
fn run_link(args: &LinkArgs) -> Result<(), String> {
    let mut store = StoreReader::open(&args.crawl).map_err(|error| {
        format!(
            "could not open the crawl store at {}: {error}",
            args.crawl.display()
        )
    })?;

    // `read_seeds` validates and returns parsed URLs; the graph only wants
    // their hosts, so hand it the strings back.
    let seeds: Vec<String> = match &args.seeds {
        Some(path) => read_seeds(path)?
            .into_iter()
            .map(|url| url.to_string())
            .collect(),
        None => Vec::new(),
    };

    let graph = HostGraph::build(&mut store, &seeds)
        .map_err(|error| format!("could not read the crawl: {error}"))?;

    if graph.is_empty() {
        return Err(format!(
            "the crawl at {} has no pages, so there is no graph to build",
            args.crawl.display()
        ));
    }

    let method: Method = args.method.into();
    let table = Authority::compute(&graph, method, args.damping);

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| args.crawl.join("authority.json"));
    table
        .write(&out)
        .map_err(|error| format!("could not write the authority table: {error}"))?;

    report_graph(&graph, &table, args.top);
    println!("\nwritten to {}", out.display());
    Ok(())
}

fn report_graph(graph: &HostGraph, table: &Authority, top: usize) {
    println!();
    println!("  hosts              {}", graph.len());
    println!("  host-to-host edges {}", graph.edges());
    println!("  seeds (trust root) {}", graph.seeds().count());

    let dropped = graph.dropped;
    println!("\n  links that did not become edges");
    println!("    nofollow           {:>9}", dropped.nofollow);
    println!("    same site          {:>9}", dropped.same_site);
    println!("    repeat on a page   {:>9}", dropped.repeat);
    println!("    unparseable        {:>9}", dropped.unparseable);

    let convergence = table.convergence;
    println!("\n  trustrank");
    if convergence.trust_root_was_empty {
        println!("    no seed hosts: this is PageRank, not TrustRank");
    }
    if convergence.converged {
        println!(
            "    converged in {} iterations (residual {:.2e})",
            convergence.iterations, convergence.residual
        );
    } else {
        println!(
            "    DID NOT CONVERGE in {} iterations (residual {:.2e})",
            convergence.iterations, convergence.residual
        );
    }

    // How much the two methods disagree. If they agree almost perfectly the
    // expensive one is not earning its iterations, whatever a judged query set
    // later says about which is better.
    let by_degree = Authority {
        method: Method::InDegree,
        ..table.clone()
    };
    let by_trust = Authority {
        method: Method::TrustRank,
        ..table.clone()
    };
    let degree_ranking = by_degree.ranking();
    let trust_ranking = by_trust.ranking();
    if let Some(correlation) = rank_correlation(&degree_ranking, &trust_ranking) {
        println!("\n  in-degree vs trustrank: rank correlation {correlation:.3}");
        if correlation > 0.99 {
            println!("    they agree; trustrank is not earning its iterations here");
        }
    }

    let ranking = table.ranking();
    let shown = top.min(ranking.len());
    println!(
        "\n  top {shown} hosts by {}",
        match table.method {
            Method::InDegree => "in-degree",
            Method::TrustRank => "trustrank",
        }
    );
    for (rank, (host, score)) in ranking.iter().take(shown).enumerate() {
        let entry = table.hosts.get(*host).copied().unwrap_or_default();
        println!(
            "  {:>3}. {:<40} {:.3}   in-degree {:<5} pages {:<5} trust {:.6}",
            rank + 1,
            host,
            score,
            entry.in_degree,
            entry.pages,
            entry.trust
        );
    }
}

fn run_search(args: &SearchArgs) -> Result<(), String> {
    let raw = args.query.join(" ");
    let query = parse::parse(&raw);
    if query.is_empty() {
        return Err(format!("nothing to search for in {raw:?}"));
    }

    let mut index = Index::open(&args.index).map_err(|error| {
        format!(
            "could not open the index at {}: {error}",
            args.index.display()
        )
    })?;
    // Host authority if `uruk link` has been run, text-only ranking if not.
    // Absent is not an error: an index is searchable the moment it is built.
    let authority = Authority::beside_crawl(&args.crawl)
        .map_err(|error| format!("could not read the authority table: {error}"))?;
    let options = SearchOptions {
        limit: args.limit.max(1),
        authority: authority.as_ref(),
        ..SearchOptions::default()
    };
    let results = search::search(&mut index, &query, &options)
        .map_err(|error| format!("search failed: {error}"))?;

    let mut store = StoreReader::open(&args.crawl).map_err(|error| {
        format!(
            "could not open the crawl store at {}: {error}",
            args.crawl.display()
        )
    })?;

    if results.hits.is_empty() {
        println!("no results for {raw:?}");
        report_cost(&results, index.len());
        return Ok(());
    }

    let terms = query.distinct_terms();
    for (rank, hit) in results.hits.iter().enumerate() {
        let record = store
            .get(hit.crawl_doc)
            .map_err(|error| format!("could not read document {}: {error}", hit.crawl_doc))?;

        let title = if record.title.trim().is_empty() {
            &record.url
        } else {
            &record.title
        };
        println!("\n{:>2}. {title}", rank + 1);
        println!("    {}", record.url);

        let extract = snippet::snippet(
            &record.text,
            &terms,
            SnippetPolicy {
                allowed: record.snippet_allowed,
                max_chars: record.max_snippet,
            },
            snippet::DEFAULT_LENGTH,
        );
        if !extract.is_empty() {
            println!("    {}", extract.text.replace('\n', " "));
        }

        // Every result can say why it ranked where it did. Not a debug mode:
        // tuning ranking from feedback is guesswork without it.
        let parts: Vec<String> = hit
            .explanation
            .signals()
            .iter()
            .map(|(name, value)| format!("{name} {value:.3}"))
            .collect();
        println!("    score {:.3}  ({})", hit.score, parts.join(" + "));

        if args.explain {
            explain(hit);
        }
    }

    report_cost(&results, index.len());
    Ok(())
}

/// The full per-term breakdown, for tuning.
fn explain(hit: &search::Hit) {
    for term in &hit.explanation.terms {
        let fields: Vec<String> = uruk_index::fields::Field::ALL
            .iter()
            .filter(|field| term.counts.get(**field) > 0)
            .map(|field| format!("{}={}", field.name(), term.counts.get(*field)))
            .collect();
        println!(
            "      {:<16} idf {:.3}  tf' {:.3}  ->  {:.3}   [{}]  in {} docs",
            term.term,
            term.idf,
            term.pseudo_frequency,
            term.contribution,
            fields.join(" "),
            term.doc_frequency,
        );
    }
    if let Some(span) = hit.explanation.closest_span {
        println!("      {:<16} terms came within {span} tokens", "proximity");
    }
    let authority = &hit.explanation.authority;
    if authority.known {
        println!(
            "      {:<16} host standing {:.3}  ->  {:.3}",
            "authority", authority.factor, authority.contribution
        );
    } else {
        // Saying nothing here would look like a host nobody links to. It is
        // not: it is a signal that was never computed, and `uruk link` is how
        // it gets computed.
        println!(
            "      {:<16} not measured (run `uruk link` to build the graph)",
            "authority"
        );
    }
    let quality = &hit.explanation.quality;
    println!(
        "      {:<16} text {:.2}  links {:.2}  scripts {}  ->  factor {:.2}",
        "quality", quality.text_ratio, quality.link_density, quality.scripts, quality.factor
    );
}

fn report_cost(results: &search::Results, corpus: u32) {
    println!(
        "\n{} of {corpus} documents matched; {} posting lists read in {:.1}ms",
        results.matched,
        results.lists_read,
        results.elapsed.as_secs_f64() * 1000.0
    );
}

/// Read a seed file: one URL per line, `#` comments and blank lines ignored.
fn read_seeds(path: &PathBuf) -> Result<Vec<Url>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;

    let mut seeds = Vec::new();
    let mut rejected = Vec::new();
    for (number, line) in raw.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        match Url::parse(line) {
            Ok(url) => seeds.push(url),
            // A typo in a seed list should be named, not silently skipped:
            // a seed that never loads is a whole branch of the crawl missing.
            Err(error) => rejected.push(format!("  line {}: {line} ({error})", number + 1)),
        }
    }

    if !rejected.is_empty() {
        return Err(format!("unusable seed URLs:\n{}", rejected.join("\n")));
    }
    if seeds.is_empty() {
        return Err(format!("{} contains no seed URLs", path.display()));
    }
    Ok(seeds)
}

fn run_crawl(args: CrawlArgs) -> Result<(), String> {
    let seeds = read_seeds(&args.seeds)?;

    if args.delay < 1.0 {
        // Not refused — the local test server in this repo's own tests needs a
        // shorter one — but never let it pass unremarked.
        eprintln!(
            "uruk: warning: a {:.1}s delay is faster than one request per second per host. \
             Do not point this at someone else's site.",
            args.delay
        );
    }

    let config = CrawlConfig {
        seeds,
        out_dir: args.out.clone(),
        max_pages: args.max_pages,
        limits: Limits {
            max_depth: args.max_depth,
            max_pages_per_host: args.max_per_host,
            ..Limits::default()
        },
        host_delay: Duration::from_secs_f64(args.delay.max(0.0)),
        concurrency: args.concurrency.max(1),
        user_agent: args.user_agent,
        progress: !args.quiet,
    };

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("could not start the async runtime: {error}"))?;
    let summary = runtime
        .block_on(crawler::run(config))
        .map_err(|error| format!("crawl failed: {error}"))?;

    report(&summary, &args.out);
    Ok(())
}

/// Print what happened, in the order someone operating a crawl cares about.
fn report(summary: &CrawlSummary, out: &Path) {
    let elapsed = summary.finished_at.saturating_sub(summary.started_at);
    println!("\ncrawl finished in {elapsed}s");
    println!("  pages stored       {}", summary.pages_stored);
    println!("  pages fetched      {}", summary.pages_fetched);
    println!("  hosts seen         {}", summary.hosts);
    println!("  urls seen          {}", summary.urls_seen);
    println!("  near-duplicates    {}", summary.near_duplicates);
    println!("  robots disallowed  {}", summary.robots_disallowed);
    println!("  noindex / noarchive{:>4}", summary.noindex);
    println!("  fetch failures     {}", summary.fetch_failures);

    if !summary.failures.is_empty() {
        println!("\n  why fetches failed");
        for (reason, count) in &summary.failures {
            println!("    {reason:<20} {count}");
        }
    }

    // Refusals are the number RESEARCH.md section 5.1 actually wants: how much
    // of what we found was worth fetching at all.
    let refused: usize = summary.refusals.values().sum();
    if refused > 0 {
        println!("\n  why urls were refused");
        for (reason, count) in summary.refusals.iter().filter(|(_, count)| **count > 0) {
            println!("    {reason:<20} {count}");
        }
    }

    if summary.text_bytes > 0 {
        let ratio = summary.bytes_written as f64 / summary.text_bytes as f64;
        println!("\n  text extracted     {}", human_bytes(summary.text_bytes));
        println!(
            "  store on disk      {}  ({:.0}% of the text, {:.1}x compression)",
            human_bytes(summary.bytes_written),
            ratio * 100.0,
            1.0 / ratio
        );
        if summary.pages_stored > 0 {
            println!(
                "  bytes per page     {:.0}",
                summary.bytes_written as f64 / summary.pages_stored as f64
            );
        }
    }
    println!("\nwritten to {}", out.display());
}

/// Bytes at a scale a person can read. A first crawl of a few hundred pages
/// would otherwise report "0.0 MB" for everything it did.
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.2} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, read_seeds};
    use clap::Parser;

    #[test]
    fn the_cli_definition_is_valid() {
        // clap can only catch a malformed command definition at runtime.
        <Cli as clap::CommandFactory>::command().debug_assert();
    }

    #[test]
    fn crawl_parses_with_only_a_seed_file() {
        let cli = Cli::parse_from(["uruk", "crawl", "--seeds", "seeds.txt"]);
        let super::Command::Crawl(args) = cli.command else {
            panic!("expected crawl")
        };
        assert_eq!(args.seeds.to_str(), Some("seeds.txt"));
        assert_eq!(args.max_pages, 1_000);
        assert!(
            args.delay >= 1.0,
            "the default delay must not be aggressive"
        );
    }

    fn write(contents: &str, name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("uruk-seeds-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn seed_files_ignore_comments_and_blank_lines() {
        let path = write(
            "# a comment\n\nhttps://a.test/\n  https://b.test/page  # trailing\n",
            "ok",
        );
        let seeds = read_seeds(&path).unwrap();
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].as_str(), "https://a.test/");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_malformed_seed_is_named_rather_than_skipped() {
        // Silently dropping a seed loses a whole branch of the crawl.
        let path = write("https://a.test/\nnot a url\n", "bad");
        let error = read_seeds(&path).unwrap_err();
        assert!(error.contains("line 2"), "error was: {error}");
        assert!(error.contains("not a url"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_empty_seed_file_is_an_error() {
        let path = write("# nothing but comments\n", "empty");
        assert!(read_seeds(&path).unwrap_err().contains("no seed URLs"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn every_subcommand_parses() {
        for argv in [
            vec!["uruk", "index"],
            vec!["uruk", "index", "--crawl", "c", "--out", "i"],
            vec!["uruk", "search", "clay", "tablets"],
            vec!["uruk", "search", "--explain", "-n", "3", "clay"],
            vec!["uruk", "serve"],
            vec!["uruk", "serve", "--address", "0.0.0.0:9000", "--explain"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_ok(),
                "failed to parse {argv:?}"
            );
        }
        // A search with no query is a usage error, not an empty search.
        assert!(Cli::try_parse_from(["uruk", "search"]).is_err());
    }

    #[test]
    fn an_exclusion_is_a_query_term_not_an_unknown_flag() {
        // Without allow_hyphen_values, clap rejects this outright.
        let cli = Cli::parse_from(["uruk", "search", "clay", "-barley"]);
        let super::Command::Search(args) = cli.command else {
            panic!("expected search")
        };
        assert_eq!(args.query, ["clay", "-barley"]);

        let parsed = super::parse::parse(&args.query.join(" "));
        assert_eq!(parsed.required, ["clay"]);
        assert_eq!(parsed.excluded, ["barley"]);
    }

    #[test]
    fn real_flags_still_work_alongside_a_query() {
        let cli = Cli::parse_from(["uruk", "search", "-n", "3", "--explain", "clay"]);
        let super::Command::Search(args) = cli.command else {
            panic!("expected search")
        };
        assert_eq!(args.limit, 3);
        assert!(args.explain);
        assert_eq!(args.query, ["clay"]);
    }

    #[test]
    fn a_multi_word_search_query_is_joined() {
        let cli = Cli::parse_from(["uruk", "search", "clay", "tablets"]);
        let super::Command::Search(args) = cli.command else {
            panic!("expected search")
        };
        assert_eq!(args.query.join(" "), "clay tablets");
        assert_eq!(args.limit, 10, "ten results is the product");
    }

    #[test]
    fn serve_listens_on_loopback_by_default() {
        // Putting a search engine on a public address should be a decision.
        let cli = Cli::parse_from(["uruk", "serve"]);
        let super::Command::Serve(args) = cli.command else {
            panic!("expected serve")
        };
        assert!(
            args.address.starts_with("127.0.0.1"),
            "default address: {}",
            args.address
        );
        assert!(!args.explain, "score breakdowns are off on the public page");
        assert_eq!(args.limit, 10);
    }

    #[test]
    fn bytes_are_reported_at_a_readable_scale() {
        use super::human_bytes;
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn a_missing_seed_file_is_reported_clearly() {
        let path = std::path::PathBuf::from("/nonexistent/uruk/seeds.txt");
        assert!(read_seeds(&path).unwrap_err().contains("could not read"));
    }
}
