//! End-to-end crawl against a local server.
//!
//! The unit tests check each part in isolation; this checks that the parts
//! agree with each other, against a site built to contain every hazard the
//! crawler claims to handle: a `robots.txt` with a forbidden directory, a
//! mirrored article, an infinite calendar, a binary file, a redirect, a 404
//! and a page asking not to be indexed.
//!
//! The most important assertions here are the ones about restraint — the URLs
//! the crawler must **never request** — and the one about timing, which is the
//! only real evidence that the politeness delay is honoured.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use uruk_crawl::crawler::{self, CrawlConfig};
use uruk_crawl::store::StoreReader;
use uruk_crawl::traps::Limits;

/// Every request the server received, with its arrival time.
type RequestLog = Arc<Mutex<Vec<(String, Instant)>>>;

/// Article bodies, each around 250 words and each about a genuinely different
/// subject. They have to be genuinely different: if two fixtures shared a body
/// the deduplication assertions would pass for the wrong reason.
const TABLETS: &str = "The scribes of Uruk were not writing poetry when they first pressed a reed \
    into wet clay. They were counting sheep, and they needed the count to survive the walk from \
    the pen to the temple storehouse. What they invented, without meaning to, was a way of making \
    a promise outlive the person who made it. A tablet recorded that a quantity of barley had \
    changed hands, in a form that could be checked later by someone who had not been present. \
    That is the whole idea, and every ledger written since is a footnote to it. The marks \
    themselves began as pictures and became wedges, because a wedge is what a cut reed leaves in \
    clay when you press rather than drag it. Dragging tears the surface; pressing does not. So \
    the shape of the writing was decided by the material, as the shape of writing usually is. \
    The tablets were not fired on purpose. Most of what survives was baked by accident when a \
    building burned down, which means the archive we have is a record of disasters rather than \
    of importance. The ordinary tablets, the ones nobody thought worth keeping, dried in the sun \
    and dissolved in the next rain. We read the fires and not the libraries. None of this was \
    literature and none of it was meant to last. It lasted anyway, which is the argument for \
    writing things down in a form that does not depend on anyone remembering to keep them.";

const HARBOURS: &str = "A deep water harbour is mostly an argument with sediment. Rivers carry \
    silt downstream and drop it the moment the current slows, which is precisely where a port \
    wants to be, so every sheltered basin is in the slow process of filling itself in. Dredging \
    is not maintenance in the sense that painting a bridge is maintenance. It is a permanent \
    subsidy paid to keep a piece of geography in a state it does not want to occupy. The \
    engineers of the nineteenth century understood this and built training walls, narrow \
    parallel structures reaching out from the river mouth, to force the current to keep running \
    fast enough to carry its own load out past the bar. Where the walls were built well the \
    channel scoured itself and the dredgers could rest. Where they were built badly the river \
    simply deposited its silt somewhere else inconvenient, and the problem moved rather than \
    resolved. Tidal range complicates all of it. A port with a large range needs either locks or \
    a willingness to strand vessels twice a day, and locks are expensive to build and slow to \
    pass. The great advantage of a natural deep water inlet is that none of this applies, which \
    is why such places became wealthy far out of proportion to anything else about them. \
    Geography decided the argument before anyone arrived to have it.";

const PRESSES: &str = "Movable type did not make books cheap immediately. The first printed \
    volumes were priced against manuscripts and the early printers went bankrupt with dependable \
    regularity, because the capital cost was enormous and the market for expensive books was \
    already served. What changed the economics was paper, which arrived from the east by a long \
    and interrupted route and which cost a fraction of prepared skin. A press with no cheap \
    substrate is a machine for producing the same luxury goods slightly faster. A press with \
    paper is a different industry altogether. The second thing that changed was standardisation \
    of the typeface, which sounds like a matter of taste and was in fact a matter of inventory. \
    Every distinct sort had to be cast, stored, sorted and redistributed after each forme was \
    broken up, and a workshop that could do this quickly could turn its capital over several \
    times a year instead of once. The famous names in early printing are mostly names of people \
    who solved logistics rather than people who solved mechanics. The mechanics had been \
    available in one form or another for a long time, in olive presses and coin dies and textile \
    stamps. What had not been available was a reason to combine them, and the reason turned out \
    to be an abundance of rags.";

fn article(topic: &str) -> String {
    let body = match topic {
        "Tablets" => TABLETS,
        "Harbours" => HARBOURS,
        "Presses" => PRESSES,
        other => panic!("no fixture body for {other}"),
    };
    format!("<h1>{topic}</h1><p>{body}</p>")
}

fn page(title: &str, inner: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><title>{title}</title></head>\
         <body><nav><a href=\"/\">home</a></nav><article>{inner}</article>\
         <footer>copyright</footer></body></html>"
    )
}

struct Response {
    status: &'static str,
    headers: Vec<(String, String)>,
    body: String,
}

fn text(status: &'static str, content_type: &str, body: String) -> Response {
    Response {
        status,
        headers: vec![("Content-Type".into(), content_type.into())],
        body,
    }
}

fn route(path: &str) -> Response {
    match path {
        "/robots.txt" => text(
            "200 OK",
            "text/plain",
            // Crawl-delay is deliberately absent: the test asserts our own
            // default is applied, which is the case that matters.
            "User-agent: *\nDisallow: /private/\n".to_owned(),
        ),

        "/" => text(
            "200 OK",
            "text/html",
            page(
                "Index",
                "<p>An index of writing about early record keeping.</p>\
                 <ul>\
                 <li><a href=\"/articles/tablets\">Tablets</a></li>\
                 <li><a href=\"/articles/mirror\">A mirror of the same piece</a></li>\
                 <li><a href=\"/articles/other\">Something else</a></li>\
                 <li><a href=\"/private/secret\">Private area</a></li>\
                 <li><a href=\"/calendar/2387/01\">Next month</a></li>\
                 <li><a href=\"/logo.png\">Logo</a></li>\
                 <li><a href=\"/old-address\">Moved page</a></li>\
                 <li><a href=\"/missing\">Broken link</a></li>\
                 <li><a href=\"/draft\">Draft</a></li>\
                 <li><a href=\"/articles/tablets?utm_source=newsletter\">Tablets again</a></li>\
                 </ul>",
            ),
        ),

        "/articles/tablets" => text("200 OK", "text/html", page("Tablets", &article("Tablets"))),

        // Same article, one word changed: a near-duplicate, not an exact one.
        "/articles/mirror" => text(
            "200 OK",
            "text/html",
            page(
                "Tablets (mirror)",
                &article("Tablets").replace("sheep", "goats"),
            ),
        ),

        "/articles/other" => text(
            "200 OK",
            "text/html",
            page("Harbours", &article("Harbours")),
        ),

        // Must never be requested: robots.txt forbids it.
        "/private/secret" => text(
            "200 OK",
            "text/html",
            page("Secret", "<p>should be unreachable</p>"),
        ),

        // Must never be requested: an implausible year is an infinite calendar.
        "/calendar/2387/01" => text("200 OK", "text/html", page("Calendar", "<p>next month</p>")),

        // Must never be requested: not a document type we index.
        "/logo.png" => text("200 OK", "image/png", "not really a png".to_owned()),

        "/old-address" => Response {
            status: "301 Moved Permanently",
            headers: vec![("Location".into(), "/articles/moved".into())],
            body: String::new(),
        },

        "/articles/moved" => text("200 OK", "text/html", page("Moved", &article("Presses"))),

        "/draft" => text(
            "200 OK",
            "text/html",
            format!(
                "<!doctype html><html><head><title>Draft</title>\
                 <meta name=\"robots\" content=\"noindex\"></head>\
                 <body><article>{}</article></body></html>",
                article("Harbours").replace("harbour", "anchorage")
            ),
        ),

        _ => text(
            "404 Not Found",
            "text/html",
            "<html><body>not found</body></html>".to_owned(),
        ),
    }
}

/// A minimal HTTP/1.1 server. Enough to answer a crawler, and no more.
async fn serve(listener: TcpListener, log: RequestLog) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let log = Arc::clone(&log);
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 8192];
            let Ok(read) = socket.read(&mut buffer).await else {
                return;
            };
            let request = String::from_utf8_lossy(&buffer[..read]);
            let target = request.split_whitespace().nth(1).unwrap_or("/").to_owned();

            if let Ok(mut entries) = log.lock() {
                entries.push((target.clone(), Instant::now()));
            }

            // Strip the query string before routing; it only matters for the
            // URL-normalisation assertion.
            let path = target.split('?').next().unwrap_or("/");
            let response = route(path);

            let mut head = format!("HTTP/1.1 {}\r\n", response.status);
            for (name, value) in &response.headers {
                let _ = write!(head, "{name}: {value}\r\n");
            }
            let _ = write!(head, "Content-Length: {}\r\n", response.body.len());
            // Close each connection so the client never waits on keep-alive.
            head.push_str("Connection: close\r\n\r\n");

            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(response.body.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}

struct Crawled {
    summary: uruk_crawl::store::CrawlSummary,
    stored: Vec<uruk_crawl::store::Record>,
    requests: Vec<(String, Instant)>,
    delay: Duration,
}

impl Crawled {
    fn paths(&self) -> Vec<&str> {
        self.requests
            .iter()
            .map(|(path, _)| path.as_str())
            .collect()
    }

    fn requested(&self, path: &str) -> bool {
        self.paths().contains(&path)
    }

    fn stored_paths(&self) -> Vec<String> {
        self.stored
            .iter()
            .map(|record| Url::parse(&record.url).unwrap().path().to_owned())
            .collect()
    }
}

async fn crawl(name: &str) -> Crawled {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let log: RequestLog = Arc::new(Mutex::new(Vec::new()));

    let server_log = Arc::clone(&log);
    let server = tokio::spawn(serve(listener, server_log));

    // Short enough to keep the test quick, long enough that a violation of it
    // cannot be mistaken for scheduling jitter.
    let delay = Duration::from_millis(300);
    // Tests in one binary run in parallel threads, so the directory has to be
    // unique per test or they delete each other's stores.
    let dir = std::env::temp_dir().join(format!("uruk-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let summary = crawler::run(CrawlConfig {
        seeds: vec![Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap()],
        out_dir: dir.clone(),
        max_pages: 50,
        limits: Limits::default(),
        host_delay: delay,
        concurrency: 4,
        user_agent: crawler::DEFAULT_USER_AGENT.to_owned(),
        progress: false,
    })
    .await
    .expect("crawl should succeed");

    server.abort();

    let mut reader = StoreReader::open(&dir).expect("store should be readable");
    let stored: Vec<_> = reader
        .records()
        .expect("iterate")
        .map(Result::unwrap)
        .collect();
    let requests = log.lock().unwrap().clone();
    let _ = std::fs::remove_dir_all(&dir);

    Crawled {
        summary,
        stored,
        requests,
        delay,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crawls_a_site_and_respects_every_rule() {
    let result = crawl("crawls_a_site_and_respects_every_rule").await;

    // --- robots.txt is read first, before anything else ---
    assert_eq!(
        result.paths().first().copied(),
        Some("/robots.txt"),
        "robots.txt must be the first request to a host, got {:?}",
        result.paths()
    );

    // --- the forbidden directory is never even requested ---
    assert!(
        !result.requested("/private/secret"),
        "robots.txt forbids /private/ but it was requested: {:?}",
        result.paths()
    );

    // --- traps and binaries are refused before a request is spent ---
    assert!(
        !result.requested("/calendar/2387/01"),
        "an infinite calendar was followed"
    );
    assert!(!result.requested("/logo.png"), "a binary file was fetched");

    // --- real articles are stored ---
    let stored = result.stored_paths();
    assert!(
        stored.contains(&"/articles/tablets".to_owned()),
        "stored: {stored:?}"
    );
    assert!(
        stored.contains(&"/articles/other".to_owned()),
        "stored: {stored:?}"
    );

    // --- the near-duplicate mirror is fetched but not stored twice ---
    assert!(
        !stored.contains(&"/articles/mirror".to_owned()),
        "the mirrored article should have been recognised as a duplicate: {stored:?}"
    );
    assert_eq!(
        result.summary.near_duplicates, 1,
        "expected exactly one near-duplicate"
    );

    // --- redirects are followed, via the frontier ---
    assert!(result.requested("/old-address"));
    assert!(
        stored.contains(&"/articles/moved".to_owned()),
        "redirect target not stored"
    );

    // --- noindex is obeyed ---
    assert!(
        result.requested("/draft"),
        "the draft should still be fetched"
    );
    assert!(
        !stored.contains(&"/draft".to_owned()),
        "a noindex page was stored"
    );
    assert_eq!(result.summary.noindex, 1);

    // --- a 404 is counted, not fatal ---
    assert!(result.requested("/missing"));
    assert_eq!(result.summary.failures.get("not_found"), Some(&1));

    // --- tracking parameters do not mint a second URL ---
    let tablet_requests = result
        .paths()
        .iter()
        .filter(|path| path.starts_with("/articles/tablets"))
        .count();
    assert_eq!(
        tablet_requests, 1,
        "?utm_source= was treated as a different page"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_exceeds_the_politeness_rate() {
    let result = crawl("never_exceeds_the_politeness_rate").await;

    // The claim the whole design rests on: one request to a host at a time,
    // and never closer together than the configured delay.
    let mut previous: Option<Instant> = None;
    let mut gaps = Vec::new();
    for (_, at) in &result.requests {
        if let Some(earlier) = previous {
            gaps.push(at.duration_since(earlier));
        }
        previous = Some(*at);
    }

    assert!(
        gaps.len() >= 4,
        "too few requests to prove anything: {}",
        result.requests.len()
    );

    // A 10% tolerance for timer granularity; a genuine violation is a request
    // arriving at a fraction of the delay, not 5% under it.
    let floor = result.delay.mul_f64(0.9);
    let violations: Vec<_> = gaps.iter().filter(|gap| **gap < floor).collect();
    assert!(
        violations.is_empty(),
        "requests to one host came {violations:?} apart, faster than the {:?} delay",
        result.delay
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extracted_text_is_the_article_and_nothing_else() {
    let result = crawl("extracted_text_is_the_article_and_nothing_else").await;
    let tablets = result
        .stored
        .iter()
        .find(|record| record.url.ends_with("/articles/tablets"))
        .expect("the tablets article should have been stored");

    assert!(
        tablets.text.contains("counting sheep"),
        "article text missing"
    );
    assert!(
        !tablets.text.contains("copyright"),
        "footer leaked into the text"
    );
    assert!(
        !tablets.text.contains("home"),
        "navigation leaked into the text"
    );
    assert_eq!(tablets.title, "Tablets");
    assert_eq!(tablets.lang.as_deref(), Some("en"));
    assert_ne!(tablets.fingerprint, 0);
    assert!(
        tablets.quality.words > 100,
        "words: {}",
        tablets.quality.words
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_summary_adds_up() {
    let result = crawl("the_summary_adds_up").await;

    assert_eq!(result.summary.pages_stored, result.stored.len());
    assert!(result.summary.pages_fetched >= result.summary.pages_stored);
    assert!(result.summary.hosts >= 1);
    assert!(result.summary.text_bytes > 0);

    // Every URL the frontier turned away is accounted for under some reason.
    let refused: usize = result.summary.refusals.values().sum();
    assert!(
        refused > 0,
        "nothing was refused, but the fixture site contains traps"
    );
    assert!(
        result
            .summary
            .refusals
            .get("uninteresting_type")
            .copied()
            .unwrap_or(0)
            >= 1,
        "the .png should have been refused by type: {:?}",
        result.summary.refusals
    );
    assert!(
        result
            .summary
            .refusals
            .get("implausible_date")
            .copied()
            .unwrap_or(0)
            >= 1,
        "the calendar should have been refused as a trap: {:?}",
        result.summary.refusals
    );
}

/// A crawl with no usable seeds should say so rather than sit there.
#[tokio::test]
async fn refuses_to_start_without_seeds() {
    let dir = std::env::temp_dir().join(format!("uruk-e2e-noseed-{}", std::process::id()));
    let error = crawler::run(CrawlConfig {
        seeds: vec![Url::parse("mailto:nobody@example.test").unwrap()],
        out_dir: dir.clone(),
        progress: false,
        ..CrawlConfig::default()
    })
    .await;
    assert!(matches!(error, Err(crawler::CrawlError::NoSeeds)));
    let _ = std::fs::remove_dir_all(&dir);
}
