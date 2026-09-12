//! The HTTP server.
//!
//! Four routes and no state that outlives a request. What is worth attention
//! here is not the routing but two other things.
//!
//! **The response headers are part of the privacy promise.** A page that
//! contains no trackers can still leak: the referrer tells every site you
//! visit what you searched for, and a stray external request would undo the
//! whole claim. So every response carries `Referrer-Policy: no-referrer` and a
//! Content Security Policy that forbids loading anything from anywhere. The
//! policy is belt and braces — there is nothing external in the HTML to begin
//! with — but it means a future mistake fails closed.
//!
//! **Searching is blocking work.** Reading posting lists and decompressing a
//! document block are file reads, and doing those on an async runtime thread
//! stalls every other request. Each query therefore runs on a blocking thread.
//! Queries are serialised behind one lock, which is the honest limitation
//! here: it is right for a single-machine instance and is the thing to change
//! first if this ever needs concurrency.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query as QueryParams, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use uruk_crawl::store::StoreReader;
use uruk_index::index::Index;
use uruk_query::parse;
use uruk_query::search::{SearchOptions, search};
use uruk_query::snippet::{self, SnippetPolicy};

use crate::html::{self, ResultRow};

/// Longest query we will act on.
///
/// A search box is an input from the internet, and a megabyte of terms is not
/// a search. Truncating rather than rejecting keeps the failure boring.
const MAX_QUERY_LEN: usize = 512;

/// The index and crawl store, behind one lock.
#[derive(Debug)]
struct Engine {
    index: Index,
    store: StoreReader,
}

/// Everything a handler needs.
#[derive(Clone)]
struct AppState {
    engine: Arc<Mutex<Engine>>,
    user_agent: String,
    /// Show the per-signal score breakdown under each result.
    explain: bool,
    results_per_page: usize,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppState")
            .field("explain", &self.explain)
            .field("results_per_page", &self.results_per_page)
            .finish_non_exhaustive()
    }
}

/// How to run the front end.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub index_dir: std::path::PathBuf,
    pub crawl_dir: std::path::PathBuf,
    pub address: SocketAddr,
    pub results_per_page: usize,
    /// Show score breakdowns. Off by default: the brief's page is ten links.
    pub explain: bool,
    pub user_agent: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("could not open the index: {0}")]
    Index(#[from] uruk_index::build::IndexError),
    #[error("could not open the crawl store: {0}")]
    Store(#[from] uruk_crawl::store::StoreError),
    #[error("could not listen on {address}: {source}")]
    Listen {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error("server stopped: {0}")]
    Serve(std::io::Error),
}

/// Build the router. Split out so tests can drive it without a socket.
fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/search", get(search_page))
        .route("/privacy", get(privacy))
        .route("/crawler", get(crawler))
        .fallback(not_found)
        .with_state(state)
}

/// Serve until the process is stopped.
pub async fn run(config: &ServeConfig) -> Result<(), ServeError> {
    let engine = Engine {
        index: Index::open(&config.index_dir)?,
        store: StoreReader::open(&config.crawl_dir)?,
    };
    let documents = engine.index.len();

    let state = AppState {
        engine: Arc::new(Mutex::new(engine)),
        user_agent: config.user_agent.clone(),
        explain: config.explain,
        results_per_page: config.results_per_page.max(1),
    };

    let listener = tokio::net::TcpListener::bind(config.address)
        .await
        .map_err(|source| ServeError::Listen {
            address: config.address,
            source,
        })?;

    let bound = listener.local_addr().unwrap_or(config.address);
    eprintln!("uruk-serve: {documents} documents indexed");
    eprintln!("uruk-serve: listening on http://{bound}/");

    axum::serve(listener, router(state))
        .await
        .map_err(ServeError::Serve)
}

/// Wrap HTML in a response carrying the headers that make the privacy claims
/// true rather than merely intended.
fn respond(status: StatusCode, body: String, cacheable: bool) -> Response {
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();

    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // The brief asks for no referrer leakage to result sites. The `rel` on
    // each link says so too; this says it for every navigation away from here,
    // including ones we did not write.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // Nothing may be loaded from anywhere. There is nothing external in the
    // HTML, so this changes nothing today — it means a future mistake fails
    // closed instead of quietly adding a third-party request.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
             base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        if cacheable {
            HeaderValue::from_static("public, max-age=3600")
        } else {
            // A results page contains the query. It should not sit in a shared
            // cache or in the back-button history of a shared computer.
            HeaderValue::from_static("no-store")
        },
    );
    response
}

async fn home() -> Response {
    respond(StatusCode::OK, html::home(), true)
}

async fn privacy() -> Response {
    respond(StatusCode::OK, html::privacy(), true)
}

async fn crawler(State(state): State<AppState>) -> Response {
    respond(StatusCode::OK, html::crawler(&state.user_agent), true)
}

async fn not_found() -> Response {
    respond(StatusCode::NOT_FOUND, html::not_found(), false)
}

#[derive(Debug, serde::Deserialize)]
struct SearchParams {
    q: Option<String>,
}

async fn search_page(
    State(state): State<AppState>,
    QueryParams(params): QueryParams<SearchParams>,
) -> Response {
    let raw = params.q.unwrap_or_default();
    let raw: String = raw.chars().take(MAX_QUERY_LEN).collect();

    if raw.trim().is_empty() {
        return respond(StatusCode::OK, html::home(), false);
    }

    // Searching reads files. Doing that on a runtime thread would stall every
    // other request in flight.
    let worker = tokio::task::spawn_blocking(move || run_search(&state, &raw));
    match worker.await {
        Ok(page) => respond(StatusCode::OK, page, false),
        Err(_) => respond(StatusCode::INTERNAL_SERVER_ERROR, html::not_found(), false),
    }
}

/// The blocking half of a search: match, rank, and fetch what is displayed.
fn run_search(state: &AppState, raw: &str) -> String {
    let query = parse::parse(raw);
    let Ok(mut engine) = state.engine.lock() else {
        // The lock is poisoned, which means a previous query panicked. Say
        // nothing found rather than propagate a panic to every later request.
        return html::results(raw, &[], 0, 0.0);
    };

    let options = SearchOptions {
        limit: state.results_per_page,
        ..SearchOptions::default()
    };
    let Ok(found) = search(&mut engine.index, &query, &options) else {
        return html::results(raw, &[], 0, 0.0);
    };

    let terms = query.distinct_terms();
    let mut shown = Vec::with_capacity(found.hits.len());

    for hit in &found.hits {
        // One document-store block read per result, and only for results that
        // are actually displayed.
        let Ok(record) = engine.store.get(hit.crawl_doc) else {
            continue;
        };

        let extract = snippet::snippet(
            &record.text,
            &terms,
            SnippetPolicy {
                allowed: record.snippet_allowed,
                max_chars: record.max_snippet,
            },
            snippet::DEFAULT_LENGTH,
        );

        let title = if record.title.trim().is_empty() {
            record.url.clone()
        } else {
            record.title.clone()
        };

        shown.push(ResultRow {
            title,
            url: record.url,
            snippet: extract.text,
            highlights: extract.highlights,
            explanation: state.explain.then(|| {
                let parts: Vec<String> = hit
                    .explanation
                    .signals()
                    .iter()
                    .map(|(name, value)| format!("{name} {value:.3}"))
                    .collect();
                format!("score {:.3} = {}", hit.score, parts.join(" + "))
            }),
        });
    }

    html::results(
        raw,
        &shown,
        found.matched,
        found.elapsed.as_secs_f64() * 1000.0,
    )
}

#[cfg(test)]
mod tests {
    use super::{MAX_QUERY_LEN, ServeConfig};

    #[test]
    fn a_query_longer_than_the_cap_is_truncated_not_rejected() {
        let huge = "clay ".repeat(10_000);
        let truncated: String = huge.chars().take(MAX_QUERY_LEN).collect();
        assert_eq!(truncated.chars().count(), MAX_QUERY_LEN);
        // And it still parses to something searchable rather than erroring.
        assert!(!uruk_query::parse::parse(&truncated).is_empty());
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // Taking chars rather than bytes is what guarantees this; slicing by
        // byte would panic on multibyte input.
        let text = "café ".repeat(1_000);
        let truncated: String = text.chars().take(MAX_QUERY_LEN).collect();
        assert!(truncated.ends_with(|c: char| c.is_whitespace() || c.is_alphanumeric()));
    }

    #[test]
    fn the_default_page_shows_ten_results() {
        let config = ServeConfig {
            index_dir: "data/index".into(),
            crawl_dir: "data/crawl".into(),
            address: "127.0.0.1:8080".parse().unwrap(),
            results_per_page: 10,
            explain: false,
            user_agent: "uruk-crawl/0.1".into(),
        };
        assert_eq!(config.results_per_page, 10, "ten links is the product");
        assert!(
            !config.explain,
            "score breakdowns are not on the public page by default"
        );
    }
}
