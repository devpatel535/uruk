//! `uruk` — the command-line entry point.
//!
//! One subcommand per component, added as each phase is built. `crawl` is
//! here; `index`, `query` and `serve` follow.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use url::Url;

use uruk_crawl::crawler::{self, CrawlConfig, DEFAULT_USER_AGENT};
use uruk_crawl::store::CrawlSummary;
use uruk_crawl::traps::Limits;

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
        Command::Crawl(args) => match run_crawl(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("uruk: {message}");
                ExitCode::FAILURE
            }
        },
    }
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
        let super::Command::Crawl(args) = cli.command;
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
