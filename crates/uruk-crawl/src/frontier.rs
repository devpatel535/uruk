//! The frontier: the queue of URLs waiting to be fetched.
//!
//! Two jobs, and the second is the one that matters.
//!
//! The obvious job is remembering what to fetch next and not fetching anything
//! twice. The important job is **politeness**: URLs are grouped per host, and
//! a host that was just visited is not eligible again until its delay has
//! passed. Work proceeds across many hosts at once and never more than one
//! request at a time to any single one.
//!
//! That structure is borrowed from Nutch's `CrawlDb` and its per-host fetch
//! queues (`RESEARCH.md` §3.4). What is deliberately not borrowed is Nutch's
//! batch architecture: this is one process you can start, stop and resume
//! without ceremony.
//!
//! Everything here is in memory, which is right for the thousands-to-millions
//! of URLs the first crawls will hold and wrong beyond that. The structure is
//! deliberately narrow so the move to an on-disk `CrawlDb` is a change behind
//! this interface rather than through the whole crawler.

use std::collections::{HashMap, HashSet};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use url::Url;

use crate::traps::{self, Limits, Trap};
use crate::url as urlnorm;

/// How long to wait between requests to one host when it has not asked for
/// anything different.
///
/// Deliberately slow. The brief asks for one request per host every few
/// seconds, and the cost of being wrong in this direction is a crawl that
/// takes longer; the cost of being wrong in the other is being blocked, and
/// rightly so.
pub const DEFAULT_HOST_DELAY: Duration = Duration::from_secs(3);

/// A URL waiting to be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: Url,
    /// Hops from a seed.
    pub depth: u32,
}

/// What the frontier has for us right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Fetch this, then call [`Frontier::note_fetched`].
    Ready(Candidate),
    /// Nothing is eligible yet; every host is inside its delay. Sleep this long.
    Wait(Duration),
    /// Nothing queued anywhere. The crawl is finished.
    Exhausted,
}

/// Why URLs were turned away, counted by reason.
///
/// `RESEARCH.md` §5.1 argues this should exist from the first run rather than
/// being added once something looks wrong: we need to learn what fraction of
/// the web will actually talk to us, and that means counting refusals by
/// category from the beginning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refusals {
    pub already_seen: usize,
    pub too_deep: usize,
    pub host_quota: usize,
    pub path_too_deep: usize,
    pub too_many_parameters: usize,
    pub repeating_path: usize,
    pub implausible_date: usize,
    pub uninteresting_type: usize,
    pub not_a_web_url: usize,
    pub off_limits: usize,
}

impl Refusals {
    pub fn total(&self) -> usize {
        self.already_seen
            + self.too_deep
            + self.host_quota
            + self.path_too_deep
            + self.too_many_parameters
            + self.repeating_path
            + self.implausible_date
            + self.uninteresting_type
            + self.not_a_web_url
            + self.off_limits
    }

    fn note(&mut self, trap: Trap) {
        match trap {
            Trap::TooDeep => self.too_deep += 1,
            Trap::HostQuotaReached => self.host_quota += 1,
            Trap::PathTooDeep => self.path_too_deep += 1,
            Trap::TooManyParameters => self.too_many_parameters += 1,
            Trap::RepeatingPath => self.repeating_path += 1,
            Trap::ImplausibleDate => self.implausible_date += 1,
            Trap::UninterestingType => self.uninteresting_type += 1,
        }
    }
}

#[derive(Debug)]
struct HostQueue {
    queue: VecDeque<Candidate>,
    /// Earliest time we may touch this host again.
    ready_at: Instant,
    /// Pages actually taken from this host.
    fetched: usize,
    delay: Duration,
    /// Set once `robots.txt` says the whole host is off limits, so that its
    /// remaining queue can be dropped rather than fetched and discarded.
    blocked: bool,
}

/// The queue of URLs waiting to be fetched.
#[derive(Debug)]
pub struct Frontier {
    hosts: HashMap<String, HostQueue>,
    /// Dedupe keys of every URL ever admitted — see [`urlnorm::dedupe_key`].
    seen: HashSet<String>,
    limits: Limits,
    default_delay: Duration,
    refusals: Refusals,
    queued: usize,
}

impl Frontier {
    pub fn new(limits: Limits) -> Self {
        Self {
            hosts: HashMap::new(),
            seen: HashSet::new(),
            limits,
            default_delay: DEFAULT_HOST_DELAY,
            refusals: Refusals::default(),
            queued: 0,
        }
    }

    /// Override the delay used for hosts that do not declare a `Crawl-delay`.
    #[must_use]
    pub fn with_default_delay(mut self, delay: Duration) -> Self {
        self.default_delay = delay;
        self
    }

    /// Offer a URL to the frontier.
    ///
    /// Returns `true` if it was queued. Everything else — already seen, a
    /// trap, a host we have been told to leave alone — is counted and dropped.
    pub fn push(&mut self, url: &Url, depth: u32, now: Instant) -> bool {
        let Ok(normalized) = urlnorm::normalize(url) else {
            self.refusals.not_a_web_url += 1;
            return false;
        };
        let Some(host) = urlnorm::host_of(&normalized) else {
            self.refusals.not_a_web_url += 1;
            return false;
        };

        let key = urlnorm::dedupe_key(&normalized);
        if self.seen.contains(&key) {
            self.refusals.already_seen += 1;
            return false;
        }

        let fetched = self.hosts.get(&host).map_or(0, |queue| queue.fetched);
        if self.hosts.get(&host).is_some_and(|queue| queue.blocked) {
            self.refusals.off_limits += 1;
            return false;
        }
        if let Err(trap) = traps::check(&normalized, depth, fetched, &self.limits) {
            self.refusals.note(trap);
            return false;
        }

        // Only record the URL as seen once it is actually admitted, so that a
        // URL refused for a transient reason (a host quota that a later run
        // raises) is not permanently poisoned.
        self.seen.insert(key);
        let default_delay = self.default_delay;
        self.hosts
            .entry(host)
            .or_insert_with(|| HostQueue {
                queue: VecDeque::new(),
                // A host we have never touched is immediately eligible.
                ready_at: now,
                fetched: 0,
                delay: default_delay,
                blocked: false,
            })
            .queue
            .push_back(Candidate { url: normalized, depth });
        self.queued += 1;
        true
    }

    /// The next URL that may be fetched without breaking a host's delay.
    ///
    /// Which of several eligible hosts is chosen is unspecified.
    pub fn next(&mut self, now: Instant) -> Next {
        let mut earliest: Option<Instant> = None;

        for queue in self.hosts.values_mut() {
            if queue.queue.is_empty() || queue.blocked {
                continue;
            }
            if queue.ready_at <= now {
                if let Some(candidate) = queue.queue.pop_front() {
                    self.queued -= 1;
                    return Next::Ready(candidate);
                }
            } else if earliest.is_none_or(|current| queue.ready_at < current) {
                earliest = Some(queue.ready_at);
            }
        }

        match earliest {
            // saturating: a host could have become ready between the scan and here.
            Some(when) => Next::Wait(when.saturating_duration_since(now)),
            None => Next::Exhausted,
        }
    }

    /// Record that a host was just contacted, starting its cooldown.
    ///
    /// Call this after **every** request to a host, including failures and
    /// `robots.txt` fetches. A request that errored still cost the server
    /// work, and backing off only on success is how a crawler hammers a site
    /// that is already struggling.
    pub fn note_fetched(&mut self, host: &str, now: Instant) {
        if let Some(queue) = self.hosts.get_mut(host) {
            queue.fetched += 1;
            queue.ready_at = now + queue.delay;
        }
    }

    /// Apply a host's declared `Crawl-delay`.
    ///
    /// Only ever lengthens the delay. A site asking to be crawled faster than
    /// our default does not override our own politeness budget.
    pub fn set_host_delay(&mut self, host: &str, delay: Duration) {
        if let Some(queue) = self.hosts.get_mut(host) {
            queue.delay = queue.delay.max(delay);
        }
    }

    /// Mark a host as off limits and discard everything queued for it.
    pub fn block_host(&mut self, host: &str) {
        if let Some(queue) = self.hosts.get_mut(host) {
            self.queued -= queue.queue.len();
            self.refusals.off_limits += queue.queue.len();
            queue.queue.clear();
            queue.blocked = true;
        }
    }

    /// Has this host been marked off limits?
    pub fn is_blocked(&self, host: &str) -> bool {
        self.hosts.get(host).is_some_and(|queue| queue.blocked)
    }

    /// URLs waiting to be fetched.
    pub fn queued(&self) -> usize {
        self.queued
    }

    /// Distinct hosts the frontier knows about.
    pub fn hosts(&self) -> usize {
        self.hosts.len()
    }

    /// Distinct URLs admitted over the life of the crawl.
    pub fn seen(&self) -> usize {
        self.seen.len()
    }

    pub fn refusals(&self) -> Refusals {
        self.refusals
    }
}

#[cfg(test)]
mod tests {
    use super::{Frontier, Next};
    use crate::traps::Limits;
    use std::time::{Duration, Instant};
    use url::Url;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    fn frontier() -> (Frontier, Instant) {
        (Frontier::new(Limits::default()), Instant::now())
    }

    #[test]
    fn a_seed_is_immediately_ready() {
        let (mut f, now) = frontier();
        assert!(f.push(&url("https://a.test/"), 0, now));
        assert_eq!(f.next(now), Next::Ready(super::Candidate { url: url("https://a.test/"), depth: 0 }));
    }

    #[test]
    fn an_empty_frontier_is_exhausted() {
        let (mut f, now) = frontier();
        assert_eq!(f.next(now), Next::Exhausted);
    }

    #[test]
    fn the_same_url_is_never_queued_twice() {
        let (mut f, now) = frontier();
        assert!(f.push(&url("https://a.test/p"), 0, now));
        assert!(!f.push(&url("https://a.test/p"), 0, now));
        assert_eq!(f.queued(), 1);
        assert_eq!(f.refusals().already_seen, 1);
    }

    #[test]
    fn urls_differing_only_by_tracking_parameters_are_one_url() {
        let (mut f, now) = frontier();
        assert!(f.push(&url("https://a.test/p"), 0, now));
        assert!(!f.push(&url("https://a.test/p?utm_source=twitter"), 0, now));
        assert!(!f.push(&url("https://a.test/p#section"), 0, now));
        assert_eq!(f.queued(), 1);
    }

    #[test]
    fn a_host_must_wait_between_requests() {
        let (mut f, now) = frontier();
        f.push(&url("https://a.test/one"), 0, now);
        f.push(&url("https://a.test/two"), 0, now);

        assert!(matches!(f.next(now), Next::Ready(_)));
        f.note_fetched("a.test", now);

        // Second URL exists but the host is cooling down.
        match f.next(now) {
            Next::Wait(delay) => assert!(delay > Duration::ZERO),
            other => panic!("expected a wait, got {other:?}"),
        }
        // After the delay it becomes available.
        assert!(matches!(f.next(now + super::DEFAULT_HOST_DELAY), Next::Ready(_)));
    }

    #[test]
    fn one_slow_host_does_not_block_another() {
        // The whole point of per-host queues: work continues elsewhere.
        // Which host is served first is unspecified, so the invariant is
        // stated without assuming it.
        let (mut f, now) = frontier();
        f.push(&url("https://a.test/one"), 0, now);
        f.push(&url("https://a.test/two"), 0, now);
        f.push(&url("https://b.test/one"), 0, now);

        let Next::Ready(first) = f.next(now) else { panic!("expected a ready URL") };
        let busy = first.url.host_str().unwrap().to_owned();
        f.note_fetched(&busy, now);

        // Whatever we just fetched, the frontier must move to the other host
        // rather than stalling on this one's cooldown.
        match f.next(now) {
            Next::Ready(second) => assert_ne!(second.url.host_str().unwrap(), busy),
            other => panic!("expected the other host to be ready, got {other:?}"),
        }
    }

    #[test]
    fn a_declared_crawl_delay_is_honoured() {
        let (mut f, now) = frontier();
        f.push(&url("https://a.test/one"), 0, now);
        f.push(&url("https://a.test/two"), 0, now);
        f.set_host_delay("a.test", Duration::from_secs(30));

        f.next(now);
        f.note_fetched("a.test", now);

        // Our default would have released it by now; the site asked for longer.
        assert!(matches!(f.next(now + super::DEFAULT_HOST_DELAY), Next::Wait(_)));
        assert!(matches!(f.next(now + Duration::from_secs(30)), Next::Ready(_)));
    }

    #[test]
    fn a_site_cannot_ask_us_to_go_faster_than_our_default() {
        let (mut f, now) = frontier();
        f.push(&url("https://a.test/one"), 0, now);
        f.push(&url("https://a.test/two"), 0, now);
        f.set_host_delay("a.test", Duration::from_millis(1));

        f.next(now);
        f.note_fetched("a.test", now);
        assert!(matches!(f.next(now + Duration::from_millis(10)), Next::Wait(_)));
    }

    #[test]
    fn a_failed_request_still_starts_the_cooldown() {
        // note_fetched is called on failures too; a struggling server must not
        // be hammered simply because it is returning errors.
        let (mut f, now) = frontier();
        f.push(&url("https://a.test/one"), 0, now);
        f.push(&url("https://a.test/two"), 0, now);
        f.next(now);
        f.note_fetched("a.test", now);
        assert!(matches!(f.next(now), Next::Wait(_)));
    }

    #[test]
    fn traps_are_refused_and_counted() {
        let (mut f, now) = frontier();
        assert!(!f.push(&url("https://a.test/img/photo.png"), 0, now));
        assert!(!f.push(&url("https://a.test/p"), 99, now));
        assert_eq!(f.refusals().uninteresting_type, 1);
        assert_eq!(f.refusals().too_deep, 1);
        assert_eq!(f.refusals().total(), 2);
    }

    #[test]
    fn non_web_urls_are_refused() {
        let (mut f, now) = frontier();
        assert!(!f.push(&url("mailto:x@a.test"), 0, now));
        assert_eq!(f.refusals().not_a_web_url, 1);
    }

    #[test]
    fn a_host_quota_stops_admitting_more() {
        let limits = Limits { max_pages_per_host: 2, ..Limits::default() };
        let mut f = Frontier::new(limits);
        let now = Instant::now();

        for i in 0..5 {
            f.push(&url(&format!("https://a.test/p{i}")), 0, now);
        }
        // The quota counts pages actually taken, so queueing is still open...
        assert_eq!(f.queued(), 5);

        // ...until two have been fetched.
        f.note_fetched("a.test", now);
        f.note_fetched("a.test", now);
        assert!(!f.push(&url("https://a.test/later"), 0, now));
        assert_eq!(f.refusals().host_quota, 1);
    }

    #[test]
    fn blocking_a_host_discards_its_queue() {
        let (mut f, now) = frontier();
        f.push(&url("https://a.test/one"), 0, now);
        f.push(&url("https://a.test/two"), 0, now);
        f.push(&url("https://b.test/one"), 0, now);
        assert_eq!(f.queued(), 3);

        f.block_host("a.test");
        assert!(f.is_blocked("a.test"));
        assert_eq!(f.queued(), 1);

        // Only b.test is left, and nothing new for a.test is accepted.
        assert!(!f.push(&url("https://a.test/three"), 0, now));
        match f.next(now) {
            Next::Ready(candidate) => assert_eq!(candidate.url.host_str(), Some("b.test")),
            other => panic!("expected b.test, got {other:?}"),
        }
        assert_eq!(f.next(now), Next::Exhausted);
    }

    #[test]
    fn depth_is_carried_through() {
        let (mut f, now) = frontier();
        f.push(&url("https://a.test/deep"), 3, now);
        match f.next(now) {
            Next::Ready(candidate) => assert_eq!(candidate.depth, 3),
            other => panic!("expected ready, got {other:?}"),
        }
    }

    #[test]
    fn counters_track_the_crawl() {
        let (mut f, now) = frontier();
        f.push(&url("https://a.test/one"), 0, now);
        f.push(&url("https://b.test/one"), 0, now);
        assert_eq!(f.hosts(), 2);
        assert_eq!(f.seen(), 2);
        assert_eq!(f.queued(), 2);
    }
}
