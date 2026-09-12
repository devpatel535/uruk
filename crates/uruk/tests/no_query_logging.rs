//! The server must not write what people search for.
//!
//! `/privacy` says a search is not stored against an IP address. The code
//! backs that up by logging nothing per request — but "we did not write a
//! logging call" is an absence, and absences rot. Somebody adds a `tracing`
//! line to debug a slow query, it prints the query, and the promise is broken
//! by a change that looked like an improvement.
//!
//! So this runs the real binary, searches for a word that appears nowhere else
//! in the world, and fails if that word turns up in anything the process
//! wrote. It is the only kind of test that can catch the change nobody meant
//! to make.
//!
//! `DEPLOYING.md` covers the other half, which this cannot test: a reverse
//! proxy's default access log records the request URI, and for this server the
//! request URI contains the query.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use uruk_crawl::store::{CrawlSummary, QualitySignals, Record, StoreWriter};
use uruk_index::build::{IndexConfig, build};

/// A word that cannot plausibly arrive from anywhere else in the output.
const CANARY: &str = "zzqx-canary-searchterm-7741";

fn record(url: &str, title: &str, text: &str) -> Record {
    Record {
        url: url.to_string(),
        final_url: url.to_string(),
        fetched_at: 0,
        status: 200,
        depth: 0,
        fingerprint: 0,
        title: title.to_string(),
        text: text.to_string(),
        headings: Vec::new(),
        lang: Some(String::from("en")),
        links: Vec::new(),
        quality: QualitySignals {
            text_ratio: 0.5,
            link_density: 0.05,
            scripts: 0,
            words: text.split_whitespace().count(),
        },
        snippet_allowed: true,
        max_snippet: None,
    }
}

/// A free port. Bound and released, which races with anything else on the
/// machine doing the same — acceptable in a test, and the failure is a clean
/// "could not bind" rather than a wrong answer.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

fn get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

#[test]
fn the_server_never_writes_what_was_searched_for() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-query-logging");
    let _ = std::fs::remove_dir_all(&dir);
    let crawl = dir.join("crawl");
    let index = dir.join("index");
    std::fs::create_dir_all(&crawl).expect("crawl dir");

    let mut writer = StoreWriter::create(&crawl).expect("store");
    writer
        .push(&record(
            "https://a.test/page",
            "A page about clay tablets",
            "The scribes of Uruk pressed reeds into wet clay to record barley rations.",
        ))
        .expect("push");
    writer.finish(&CrawlSummary::default()).expect("finish");

    build(&IndexConfig {
        crawl_dir: crawl.clone(),
        out_dir: index.clone(),
        docs_per_segment: 100,
        progress: false,
    })
    .expect("index");

    let port = free_port();
    let mut server = Command::new(env!("CARGO_BIN_EXE_uruk"))
        .arg("serve")
        .args(["--index", index.to_str().expect("path")])
        .args(["--crawl", crawl.to_str().expect("path")])
        .args(["--address", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the server");

    // Wait for it to accept connections rather than guessing at a sleep.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ready = false;
    while Instant::now() < deadline {
        if get(port, "/").is_some() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "the server never started listening on {port}");

    // A search, a search that matches nothing, and the canary in a place that
    // would be reflected into an error path if one existed.
    let found = get(port, &format!("/search?q=clay+tablets+{CANARY}"))
        .expect("the search request should be answered");
    assert!(found.starts_with("HTTP/1.1 200"), "unexpected: {found:.60}");
    let _ = get(port, &format!("/search?q={CANARY}"));
    let _ = get(port, &format!("/{CANARY}"));

    server.kill().expect("stop the server");
    let output = server.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stdout.contains(CANARY),
        "the query was written to stdout:\n{stdout}"
    );
    assert!(
        !stderr.contains(CANARY),
        "the query was written to stderr:\n{stderr}"
    );
    // The start-up lines are allowed and useful; anything per-request is not.
    assert!(
        stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            <= 2,
        "the server logged more than its two start-up lines:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
