//! The crawl loop.
//!
//! Take a URL the frontier says is polite to fetch, get it, extract the text
//! and the links, store the text, queue the links. Repeat until we have enough
//! pages or run out of things to visit.
//!
//! Concurrency is across hosts and never within one. The frontier hands out at
//! most one URL per host at a time, so several requests can be in flight while
//! every individual site still sees one request every few seconds. That is the
//! only arrangement that is both fast and polite, and it is why the per-host
//! queue exists rather than a single global queue.
//!
//! Each task is responsible for the whole of one URL, including fetching that
//! host's `robots.txt` the first time we meet it. Robots is therefore never
//! raced: the host is held in flight for the duration, so two tasks cannot
//! both decide it is missing and both go and get it.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::task::JoinSet;
use url::Url;

use crate::fetch::{FetchError, Fetched, Fetcher, Outcome};
use crate::frontier::{Frontier, Next};
use crate::parse::{self, Directives};
use crate::robots::{self, Robots};
use crate::simhash::{self, SeenFingerprints};
use crate::store::{CrawlSummary, OutLink, QualitySignals, Record, StoreError, StoreWriter};
use crate::traps::Limits;
use crate::url as urlnorm;

/// The token a `robots.txt` uses to name us specifically.
pub const PRODUCT_TOKEN: &str = "uruk-crawl";

/// Default user-agent.
///
/// The contact URL is not decoration. `RESEARCH.md` §5.1: a crawler with no
/// stated identity reads as an AI scraper in 2026, and the page behind this
/// URL is where an operator finds out who we are and how to block us.
pub const DEFAULT_USER_AGENT: &str =
    "uruk-crawl/0.1 (+https://github.com/devpatel535/uruk; independent search index)";

#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    #[error("could not open the crawl store: {0}")]
    Store(#[from] StoreError),
    #[error("could not build the HTTP client: {0}")]
    Fetch(#[from] FetchError),
    #[error("no usable seed URLs")]
    NoSeeds,
}

/// How to run a crawl.
#[derive(Debug, Clone)]
pub struct CrawlConfig {
    pub seeds: Vec<Url>,
    /// Where the crawl store is written.
    pub out_dir: PathBuf,
    /// Stop once this many pages have been stored.
    pub max_pages: usize,
    pub limits: Limits,
    /// Minimum gap between requests to one host.
    pub host_delay: Duration,
    /// Requests in flight at once, across different hosts.
    pub concurrency: usize,
    pub user_agent: String,
    /// Print progress to stderr. A crawler you cannot watch is the mistake
    /// `RESEARCH.md` §3.4 attributes to the open-source crawlers of the era.
    pub progress: bool,
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            seeds: Vec::new(),
            out_dir: PathBuf::from("data/crawl"),
            max_pages: 1_000,
            limits: Limits::default(),
            host_delay: crate::frontier::DEFAULT_HOST_DELAY,
            concurrency: 8,
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            progress: true,
        }
    }
}

/// Shared, so that every task sees a host's `robots.txt` once one task has
/// fetched it.
type RobotsCache = Arc<Mutex<HashMap<String, Arc<Robots>>>>;

/// What one task did with one URL.
#[derive(Debug)]
enum TaskOutcome {
    Fetched(Box<Outcome>),
    /// `robots.txt` allows us on this host but not on this path.
    PathDisallowed,
    /// `robots.txt` refuses the whole host.
    HostDisallowed,
    Failed(FetchError),
}

#[derive(Debug)]
struct Completed {
    url: Url,
    host: String,
    depth: u32,
    outcome: TaskOutcome,
    /// Set when this task was the one that fetched the host's `robots.txt`.
    crawl_delay: Option<Duration>,
    /// True when this task made no request at all, so no cooldown is owed.
    skipped: bool,
}

/// Everything the crawl loop mutates as results come back.
///
/// Grouped rather than passed around as six arguments, so that absorbing a
/// completed task is one call and `run` stays readable.
#[derive(Debug)]
struct CrawlState {
    frontier: Frontier,
    seen_text: SeenFingerprints,
    writer: StoreWriter,
    summary: CrawlSummary,
    failures: BTreeMap<String, usize>,
    stored: usize,
}

impl CrawlState {
    /// Fold one finished task into the crawl.
    fn absorb(
        &mut self,
        completed: Completed,
        progress: bool,
        now: Instant,
    ) -> Result<(), CrawlError> {
        // A task that never contacted the host owes it no cooldown.
        if completed.skipped {
            self.frontier.note_skipped(&completed.host);
        } else {
            self.frontier.note_fetched(&completed.host, now);
            self.summary.pages_fetched += 1;
        }
        if let Some(delay) = completed.crawl_delay {
            self.frontier.set_host_delay(&completed.host, delay);
        }

        match completed.outcome {
            TaskOutcome::Fetched(outcome) => match *outcome {
                Outcome::Redirect(target) => {
                    // Requeued rather than followed, so the target's own host
                    // and robots.txt are checked like any other URL.
                    self.frontier.push(&target, completed.depth, now);
                }
                Outcome::Page(page) => {
                    if self.keep(&page, completed.depth, now)? {
                        self.stored += 1;
                        if progress && self.stored.is_multiple_of(25) {
                            eprintln!(
                                "uruk-crawl: {} stored, {} queued, {} hosts",
                                self.stored,
                                self.frontier.queued(),
                                self.frontier.hosts()
                            );
                        }
                    }
                }
            },
            TaskOutcome::PathDisallowed => self.summary.robots_disallowed += 1,
            TaskOutcome::HostDisallowed => {
                self.summary.robots_disallowed += 1;
                self.frontier.block_host(&completed.host);
            }
            TaskOutcome::Failed(error) => {
                self.summary.fetch_failures += 1;
                *self
                    .failures
                    .entry(error.category().to_owned())
                    .or_default() += 1;
                if progress {
                    eprintln!("uruk-crawl: {} failed: {error}", completed.url);
                }
            }
        }
        Ok(())
    }

    /// Parse a fetched page, queue its links, and store it if it is worth
    /// storing. Returns whether it was stored.
    fn keep(&mut self, fetched: &Fetched, depth: u32, now: Instant) -> Result<bool, CrawlError> {
        let mut header_directives = Directives::default();
        if let Some(header) = &fetched.x_robots {
            header_directives.apply(header);
        }
        let page = parse::parse(&fetched.body, &fetched.url, header_directives);

        // Links are queued even from a page we will not store: a hub with no
        // prose of its own is still how we reach the pages that have some.
        // A page asking us not to follow its links is obeyed.
        if page.directives.follow {
            for link in &page.links {
                if !link.nofollow {
                    self.frontier.push(&link.url, depth + 1, now);
                }
            }
        }

        // `noindex` means do not show it; `noarchive` means do not keep a copy.
        // A search index of pages we may neither store nor quote is not worth
        // the disk, so both are treated as "do not store".
        if !page.directives.index || !page.directives.archive {
            self.summary.noindex += 1;
            return Ok(false);
        }
        if page.text.trim().is_empty() {
            return Ok(false);
        }

        let fingerprint = simhash::fingerprint(&page.text);
        if self
            .seen_text
            .insert_unless_duplicate(fingerprint)
            .is_some()
        {
            self.summary.near_duplicates += 1;
            return Ok(false);
        }

        // A canonical URL is the page naming its own preferred address, which
        // is how mirrors and print views converge on one record.
        let final_url = page.canonical.as_ref().unwrap_or(&fetched.url);

        self.writer.push(&Record {
            url: fetched.url.to_string(),
            final_url: final_url.to_string(),
            fetched_at: unix_now(),
            status: fetched.status,
            depth,
            fingerprint,
            title: page.title,
            text: page.text,
            headings: page.headings,
            lang: page.lang,
            links: page
                .links
                .into_iter()
                .map(|link| OutLink {
                    url: link.url.to_string(),
                    anchor: link.anchor,
                    nofollow: link.nofollow,
                })
                .collect(),
            quality: QualitySignals {
                text_ratio: page.quality.text_ratio,
                link_density: page.quality.link_density,
                scripts: page.quality.scripts,
                words: page.quality.words,
            },
            snippet_allowed: page.directives.snippet,
            max_snippet: page.directives.max_snippet,
        })?;
        Ok(true)
    }
}

/// Run a crawl to completion.
pub async fn run(config: CrawlConfig) -> Result<CrawlSummary, CrawlError> {
    let fetcher = Fetcher::new(&config.user_agent)?;
    let robots_cache: RobotsCache = Arc::new(Mutex::new(HashMap::new()));

    let mut frontier = Frontier::new(config.limits).with_default_delay(config.host_delay);
    let start = Instant::now();
    if !config
        .seeds
        .iter()
        .any(|seed| frontier.push(seed, 0, start))
    {
        return Err(CrawlError::NoSeeds);
    }

    let mut state = CrawlState {
        frontier,
        seen_text: SeenFingerprints::new(),
        writer: StoreWriter::create(&config.out_dir)?,
        summary: CrawlSummary {
            started_at: unix_now(),
            ..CrawlSummary::default()
        },
        failures: BTreeMap::new(),
        stored: 0,
    };
    let mut tasks: JoinSet<Completed> = JoinSet::new();

    while state.stored < config.max_pages {
        let waiting_for = dispatch(
            &mut state.frontier,
            &mut tasks,
            &config,
            &fetcher,
            &robots_cache,
        );

        if tasks.is_empty() {
            // Nothing running and nothing ready: either wait for a host's
            // delay to expire, or, if nothing is even waiting, we are done.
            let Some(delay) = waiting_for else { break };
            tokio::time::sleep(delay).await;
            continue;
        }

        let Some(joined) = tasks.join_next().await else {
            continue;
        };
        match joined {
            Ok(completed) => state.absorb(completed, config.progress, Instant::now())?,
            // A panicking task must not take the crawl down with it.
            Err(error) => {
                *state.failures.entry("task_panic".into()).or_default() += 1;
                if config.progress {
                    eprintln!("uruk-crawl: task failed: {error}");
                }
            }
        }
    }

    // Stop outstanding work rather than leaving it running past the finish.
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}

    finalise(&mut state.summary, &state.frontier, state.failures);
    // finish() fills in what only the store knows, so its return value is the
    // authoritative summary rather than the one we handed it.
    Ok(state.writer.finish(&state.summary)?)
}

/// Start tasks until the concurrency limit is reached or the frontier has
/// nothing eligible. Returns how long to wait when it ran out for now.
fn dispatch(
    frontier: &mut Frontier,
    tasks: &mut JoinSet<Completed>,
    config: &CrawlConfig,
    fetcher: &Fetcher,
    robots_cache: &RobotsCache,
) -> Option<Duration> {
    while tasks.len() < config.concurrency {
        match frontier.next(Instant::now()) {
            Next::Ready(candidate) => {
                let Some(host) = urlnorm::host_of(&candidate.url) else {
                    continue;
                };
                tasks.spawn(visit(
                    fetcher.clone(),
                    Arc::clone(robots_cache),
                    candidate.url,
                    host,
                    candidate.depth,
                    config.host_delay,
                ));
            }
            Next::Wait(delay) => return Some(delay),
            Next::Exhausted => return None,
        }
    }
    None
}

/// Fold the frontier's counters into the summary that gets written out.
fn finalise(summary: &mut CrawlSummary, frontier: &Frontier, failures: BTreeMap<String, usize>) {
    let refusals = frontier.refusals();
    summary.finished_at = unix_now();
    summary.hosts = frontier.hosts();
    summary.urls_seen = frontier.seen();
    summary.failures = failures;
    summary.refusals = BTreeMap::from([
        ("already_seen".into(), refusals.already_seen),
        ("too_deep".into(), refusals.too_deep),
        ("host_quota".into(), refusals.host_quota),
        ("path_too_deep".into(), refusals.path_too_deep),
        ("too_many_parameters".into(), refusals.too_many_parameters),
        ("repeating_path".into(), refusals.repeating_path),
        ("implausible_date".into(), refusals.implausible_date),
        ("uninteresting_type".into(), refusals.uninteresting_type),
        ("not_a_web_url".into(), refusals.not_a_web_url),
        ("off_limits".into(), refusals.off_limits),
    ]);
}

/// This host's rules, fetching `robots.txt` if we have not met it before.
///
/// Returns the rules and whether a request was made, since a cached answer
/// costs the host nothing and so owes it no cooldown.
async fn robots_for(
    fetcher: &Fetcher,
    cache: &RobotsCache,
    url: &Url,
    host: &str,
    host_delay: Duration,
) -> (Arc<Robots>, bool) {
    if let Ok(map) = cache.lock()
        && let Some(known) = map.get(host)
    {
        return (Arc::clone(known), false);
    }

    // First contact. The frontier holds this host in flight, so exactly one
    // task can be here at a time and robots.txt is never fetched twice.
    let response = fetch_robots(fetcher, url).await;
    let rules = Arc::new(Robots::from_fetch(&response));
    if let Ok(mut map) = cache.lock() {
        map.entry(host.to_owned())
            .or_insert_with(|| Arc::clone(&rules));
    }
    // We have already spoken to this host once; wait before speaking again.
    tokio::time::sleep(host_delay).await;
    (rules, true)
}

/// Fetch one URL, consulting the host's `robots.txt` first.
async fn visit(
    fetcher: Fetcher,
    cache: RobotsCache,
    url: Url,
    host: String,
    depth: u32,
    host_delay: Duration,
) -> Completed {
    let (rules, made_request) = robots_for(&fetcher, &cache, &url, &host, host_delay).await;
    let crawl_delay = made_request
        .then(|| rules.crawl_delay(PRODUCT_TOKEN))
        .flatten();

    // Path patterns may match the query string, so both are offered.
    let path_and_query = match url.query() {
        Some(query) => format!("{}?{}", url.path(), query),
        None => url.path().to_owned(),
    };

    if !rules.allows(PRODUCT_TOKEN, &path_and_query) {
        // Distinguish "not this page" from "not this site": the second lets
        // the frontier discard everything else queued for the host.
        let outcome = if rules.allows(PRODUCT_TOKEN, "/") {
            TaskOutcome::PathDisallowed
        } else {
            TaskOutcome::HostDisallowed
        };
        return Completed {
            url,
            host,
            depth,
            outcome,
            crawl_delay,
            skipped: !made_request,
        };
    }

    let outcome = match fetcher.get(&url).await {
        Ok(outcome) => TaskOutcome::Fetched(Box::new(outcome)),
        Err(error) => TaskOutcome::Failed(error),
    };
    Completed {
        url,
        host,
        depth,
        outcome,
        crawl_delay,
        skipped: false,
    }
}

/// Fetch and classify `robots.txt`, mapping HTTP status onto RFC 9309's rules.
async fn fetch_robots(fetcher: &Fetcher, from: &Url) -> robots::Fetched {
    let Ok(mut robots_url) = from.join("/robots.txt") else {
        return robots::Fetched::Missing;
    };
    robots_url.set_query(None);
    robots_url.set_fragment(None);

    match fetcher.get(&robots_url).await {
        Ok(Outcome::Page(body)) => robots::Fetched::Body(body.body),
        Err(FetchError::Status {
            status: 401 | 403, ..
        }) => robots::Fetched::AccessDenied,
        Err(FetchError::Status { status, .. }) if (400..500).contains(&status) => {
            robots::Fetched::Missing
        }
        // Everything else leaves us not knowing the rules: a 5xx, a timeout, a
        // transport failure, or a redirect we decline to chase. Not knowing is
        // not permission, so all of it means "unavailable", never "absent".
        Ok(Outcome::Redirect(_)) | Err(_) => robots::Fetched::Unavailable,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::{CrawlConfig, DEFAULT_USER_AGENT, PRODUCT_TOKEN};

    #[test]
    fn the_user_agent_identifies_us_and_says_where_to_complain() {
        assert!(DEFAULT_USER_AGENT.starts_with(PRODUCT_TOKEN));
        assert!(
            DEFAULT_USER_AGENT.contains("https://"),
            "no contact URL in the user-agent"
        );
    }

    #[test]
    fn defaults_are_polite() {
        let config = CrawlConfig::default();
        assert!(
            config.host_delay >= std::time::Duration::from_secs(1),
            "default delay is too aggressive"
        );
        assert!(
            config.limits.max_depth <= 6,
            "default crawl is not bounded tightly enough"
        );
    }
}
