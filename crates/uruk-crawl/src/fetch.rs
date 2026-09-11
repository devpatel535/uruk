//! The HTTP client.
//!
//! Deliberately boring, with four properties that matter more than speed.
//!
//! **It says who it is.** The user-agent names the project and carries a URL
//! where an operator can read what we do and how to stop us. `RESEARCH.md`
//! §5.1 argues this is not cosmetic: in 2026 an unidentified crawler reads as
//! an AI scraper, because most of them are.
//!
//! **It does not follow redirects itself.** A redirect can cross hosts, and a
//! followed one would fetch a page whose own `robots.txt` we never consulted.
//! So a 3xx comes back as [`Outcome::Redirect`] and goes through the frontier
//! like any other URL — robots check included.
//!
//! **It cannot be made to read forever.** The body is streamed with a hard
//! byte cap, so one enormous file cannot stall the crawl or exhaust memory.
//!
//! **It backs off when asked.** `429` and `503` carry `Retry-After`, and a
//! crawler that ignores that is a crawler that gets blocked.

use std::time::Duration;

use url::Url;

/// Bytes we will read from one response before giving up on it. Article pages
/// are a few hundred kilobytes at most; past this it is not prose.
pub const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Whole-request budget, connection included.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Content types worth parsing. Anything else is a wasted fetch we stop paying
/// for as soon as the headers arrive.
fn is_parseable(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "text/html" | "application/xhtml+xml" | "text/plain" | ""
    )
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("request timed out")]
    Timeout,
    #[error("response exceeded {limit} bytes")]
    TooLarge { limit: usize },
    #[error("content type {content_type} is not text we can parse")]
    NotParseable { content_type: String },
    #[error("server returned {status}")]
    Status {
        status: u16,
        retry_after: Option<Duration>,
    },
    #[error("redirected without a usable Location header")]
    BadRedirect,
    #[error("transport error: {0}")]
    Transport(String),
}

impl FetchError {
    /// A short, stable label for counting failures by category.
    ///
    /// `RESEARCH.md` §5.1 wants refusal reasons from the first run: the point
    /// of the crawl is partly to discover how much of the web will talk to us.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::TooLarge { .. } => "too_large",
            Self::NotParseable { .. } => "not_parseable",
            Self::BadRedirect => "bad_redirect",
            Self::Transport(_) => "transport",
            Self::Status { status, .. } => match status {
                401 | 403 => "forbidden",
                404 | 410 => "not_found",
                429 => "rate_limited",
                500..=599 => "server_error",
                _ => "other_status",
            },
        }
    }

    /// Is another attempt later worth making?
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Timeout | Self::Transport(_) => true,
            Self::Status { status, .. } => matches!(status, 429 | 500..=599),
            _ => false,
        }
    }
}

/// A successful fetch.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub url: Url,
    pub status: u16,
    pub body: String,
    /// Whatever `X-Robots-Tag` said, to be merged with the page's meta tags.
    pub x_robots: Option<String>,
}

/// What came back.
#[derive(Debug, Clone)]
pub enum Outcome {
    Page(Box<Fetched>),
    /// A 3xx. The target is returned rather than followed, so that it passes
    /// through the frontier and its host's `robots.txt` like anything else.
    Redirect(Url),
}

/// The HTTP client.
#[derive(Debug, Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    max_bytes: usize,
}

impl Fetcher {
    /// Build a client identifying itself as `user_agent`.
    pub fn new(user_agent: &str) -> Result<Self, FetchError> {
        Self::builder(user_agent, DEFAULT_TIMEOUT, DEFAULT_MAX_BYTES)
    }

    pub fn builder(
        user_agent: &str,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(timeout)
            .connect_timeout(timeout / 2)
            // Redirects are the frontier's business, not the client's.
            .redirect(reqwest::redirect::Policy::none())
            // A crawler holds connections to thousands of hosts; letting them
            // accumulate is how it runs out of file descriptors overnight.
            .pool_idle_timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|error| FetchError::Transport(error.to_string()))?;
        Ok(Self { client, max_bytes })
    }

    /// Fetch `url` as text.
    pub async fn get(&self, url: &Url) -> Result<Outcome, FetchError> {
        let mut response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| classify(&error))?;
        let status = response.status();

        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or(FetchError::BadRedirect)?;
            // Relative Location headers are legal and common.
            let target = url.join(location).map_err(|_| FetchError::BadRedirect)?;
            return Ok(Outcome::Redirect(target));
        }

        if !status.is_success() {
            return Err(FetchError::Status {
                status: status.as_u16(),
                retry_after: retry_after(response.headers()),
            });
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if !is_parseable(&content_type) {
            return Err(FetchError::NotParseable { content_type });
        }

        let x_robots = response
            .headers()
            .get("x-robots-tag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        // Refuse before reading when the server is honest about the size.
        if let Some(length) = response.content_length()
            && length > self.max_bytes as u64
        {
            return Err(FetchError::TooLarge {
                limit: self.max_bytes,
            });
        }

        let body = self.read_capped(&mut response).await?;
        let body = decode(&body, &content_type);

        Ok(Outcome::Page(Box::new(Fetched {
            url: url.clone(),
            status: status.as_u16(),
            body,
            x_robots,
        })))
    }

    /// Read the body, stopping the moment it exceeds the cap.
    ///
    /// `Content-Length` is a claim, not a promise, so the limit is also
    /// enforced against the bytes that actually arrive.
    async fn read_capped(&self, response: &mut reqwest::Response) -> Result<Vec<u8>, FetchError> {
        let mut body = Vec::with_capacity(64 * 1024);
        while let Some(chunk) = response.chunk().await.map_err(|error| classify(&error))? {
            if body.len() + chunk.len() > self.max_bytes {
                return Err(FetchError::TooLarge {
                    limit: self.max_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn classify(error: &reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Transport(error.to_string())
    }
}

/// `Retry-After` as a duration. The header may be seconds or an HTTP date; we
/// understand the seconds form and treat a date as "wait a while".
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    raw.trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or(Some(Duration::from_secs(60)))
        // Never let a server park us for longer than an hour.
        .map(|delay| delay.min(Duration::from_secs(3600)))
}

/// Decode a response body to text.
///
/// A meaningful slice of the web is still Windows-1252 or ISO-8859-1, and
/// decoding those as UTF-8 produces replacement characters that become junk
/// terms in the index (`RESEARCH.md` §3.3). The charset comes from the header
/// when given, from a `<meta charset>` in the first chunk otherwise, and
/// UTF-8 is the fallback.
fn decode(body: &[u8], content_type: &str) -> String {
    let declared = content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"'))
    });

    let label = declared.map_or_else(|| sniff_meta_charset(body), str::to_owned);
    let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(body);
    text.into_owned()
}

/// Look for `<meta charset=...>` in the head, where most pages declare it.
fn sniff_meta_charset(body: &[u8]) -> String {
    const WINDOW: usize = 2048;
    let head = &body[..body.len().min(WINDOW)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();

    if let Some(at) = head.find("charset") {
        let rest = &head[at + "charset".len()..];
        let rest = rest.trim_start_matches(['=', ' ', '"', '\'']);
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        if end > 0 {
            return rest[..end].to_owned();
        }
    }
    "utf-8".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{FetchError, decode, is_parseable, retry_after, sniff_meta_charset};
    use std::time::Duration;

    #[test]
    fn parseable_types_are_recognised() {
        assert!(is_parseable("text/html"));
        assert!(is_parseable("text/html; charset=utf-8"));
        assert!(is_parseable("TEXT/HTML"));
        assert!(is_parseable("application/xhtml+xml"));
        // A server that sends no type at all gets the benefit of the doubt.
        assert!(is_parseable(""));
    }

    #[test]
    fn binary_types_are_refused() {
        for kind in [
            "image/png",
            "application/pdf",
            "application/zip",
            "video/mp4",
        ] {
            assert!(!is_parseable(kind), "{kind} should not be parseable");
        }
    }

    #[test]
    fn failure_categories_are_stable() {
        assert_eq!(FetchError::Timeout.category(), "timeout");
        assert_eq!(
            FetchError::Status {
                status: 403,
                retry_after: None
            }
            .category(),
            "forbidden"
        );
        assert_eq!(
            FetchError::Status {
                status: 404,
                retry_after: None
            }
            .category(),
            "not_found"
        );
        assert_eq!(
            FetchError::Status {
                status: 429,
                retry_after: None
            }
            .category(),
            "rate_limited"
        );
        assert_eq!(
            FetchError::Status {
                status: 503,
                retry_after: None
            }
            .category(),
            "server_error"
        );
    }

    #[test]
    fn only_some_failures_are_worth_retrying() {
        assert!(FetchError::Timeout.is_transient());
        assert!(
            FetchError::Status {
                status: 503,
                retry_after: None
            }
            .is_transient()
        );
        assert!(
            !FetchError::Status {
                status: 404,
                retry_after: None
            }
            .is_transient()
        );
        assert!(!FetchError::TooLarge { limit: 1 }.is_transient());
    }

    fn headers(name: &str, value: &str) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        map.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        map
    }

    #[test]
    fn retry_after_seconds_are_read() {
        assert_eq!(
            retry_after(&headers("retry-after", "120")),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn retry_after_is_capped() {
        // A server cannot park the crawler for a week.
        assert_eq!(
            retry_after(&headers("retry-after", "999999")),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn an_http_date_retry_after_becomes_a_sane_wait() {
        assert_eq!(
            retry_after(&headers("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn utf8_bodies_decode_unchanged() {
        assert_eq!(
            decode("Uruk — clay".as_bytes(), "text/html; charset=utf-8"),
            "Uruk — clay"
        );
    }

    #[test]
    fn a_declared_legacy_charset_is_honoured() {
        // 0xE9 is é in Windows-1252 and invalid UTF-8. Decoding it as UTF-8
        // would put a replacement character into the index.
        let body = b"caf\xe9 society";
        assert_eq!(
            decode(body, "text/html; charset=windows-1252"),
            "café society"
        );
        assert!(!decode(body, "text/html; charset=windows-1252").contains('\u{fffd}'));
    }

    #[test]
    fn a_meta_charset_is_used_when_the_header_is_silent() {
        let body = b"<html><head><meta charset=\"windows-1252\"></head><body>caf\xe9</body></html>";
        assert!(decode(body, "text/html").contains("café"));
    }

    #[test]
    fn charset_sniffing_falls_back_to_utf8() {
        assert_eq!(
            sniff_meta_charset(b"<html><head><title>no charset</title>"),
            "utf-8"
        );
    }

    #[test]
    fn an_unknown_charset_label_falls_back_rather_than_failing() {
        assert_eq!(
            decode("plain".as_bytes(), "text/html; charset=not-a-real-encoding"),
            "plain"
        );
    }
}
