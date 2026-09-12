//! `uruk` — the command-line entry point.
//!
//! One subcommand per component, added as each phase is built. `crawl`,
//! `index` and `search` are here; `serve` follows.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use url::Url;

use uruk_crawl::crawler::{self, CrawlConfig, DEFAULT_USER_AGENT};
use uruk_crawl::store::{CrawlSummary, StoreReader};
use uruk_crawl::traps::Limits;
use uruk_index::build::{self, IndexConfig};
use uruk_index::index::Index;
use uruk_query::parse;
use uruk_query::search::{self, SearchOptions};
use uruk_query::snippet::{self, SnippetPolicy};

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
    /// Search an index.
    Search(SearchArgs),
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
        Command::Search(args) => finish(run_search(&args)),
    }
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
    let options = SearchOptions {
        limit: args.limit.max(1),
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
