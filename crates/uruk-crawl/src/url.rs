//! URL normalisation.
//!
//! Two different jobs get confused with each other constantly, so they are two
//! functions here:
//!
//! - [`normalize`] produces the URL we will actually **fetch**. It only removes
//!   things that cannot change what a server returns: the fragment, a default
//!   port, dot segments, and query parameters that exist purely for tracking.
//! - [`dedupe_key`] produces a string used only to decide **"have we seen this
//!   page already?"**. It may reorder query parameters, which `normalize` must
//!   not do, because a minority of servers genuinely care about parameter
//!   order and we would rather fetch a duplicate than fetch a 404.
//!
//! Getting this wrong is one of the classic ways a crawl explodes: session IDs
//! and tracking parameters mint a fresh URL for every visit, so the frontier
//! grows without bound while fetching the same page over and over
//! (`RESEARCH.md` §3.2).

use url::Url;

/// Query parameters that exist to track a human and never change the response.
///
/// Conservative on purpose. A parameter is only listed if removing it is safe
/// on essentially every site; anything ambiguous (`ref`, `source`, `id`) is
/// left alone, because dropping a meaningful parameter turns a good URL into a
/// 404 and we would never notice.
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_id",
    "utm_source_platform",
    "gclid",
    "gclsrc",
    "dclid",
    "fbclid",
    "msclkid",
    "yclid",
    "twclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gl",
    "hsa_cam",
    "hsa_grp",
    "vero_id",
    "wickedid",
    "mkt_tok",
];

/// Parameters that carry a per-visitor session. These are the worst offenders:
/// every visit produces a "new" URL for identical content.
const SESSION_PARAMS: &[&str] = &[
    "phpsessid",
    "jsessionid",
    "aspsessionid",
    "asp.net_sessionid",
    "sessionid",
    "session_id",
    "sid",
    "sessid",
    "cfid",
    "cftoken",
    "zenid",
    "oscsid",
];

/// Reasons a URL is not worth putting in the frontier at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// Not `http` or `https`. We do not crawl `mailto:`, `javascript:`, `ftp:`.
    NotWebScheme,
    /// No host to be polite to.
    NoHost,
    /// Long enough that it is almost certainly machine-generated.
    TooLong,
}

/// The longest URL we will consider. Anything past this is a generated trap or
/// a data URI someone pasted into an `href`.
const MAX_URL_LEN: usize = 2048;

fn is_droppable(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    TRACKING_PARAMS.contains(&lower.as_str())
        || SESSION_PARAMS.contains(&lower.as_str())
        // Any `utm_*` we have not enumerated.
        || lower.starts_with("utm_")
}

/// Normalise a URL for fetching.
///
/// Removes the fragment (never sent to a server anyway), the port when it is
/// the scheme default, and tracking and session parameters. Host case and dot
/// segments are handled by the `url` crate's WHATWG parsing. Parameter order
/// is preserved.
pub fn normalize(raw: &Url) -> Result<Url, Rejected> {
    if !matches!(raw.scheme(), "http" | "https") {
        return Err(Rejected::NotWebScheme);
    }
    if raw.host_str().is_none() {
        return Err(Rejected::NoHost);
    }

    let mut url = raw.clone();
    url.set_fragment(None);

    // `Url::port` already returns None for the scheme default, so this only
    // strips an explicitly written `:80` / `:443`.
    if url.port().is_none() {
        let _ = url.set_port(None);
    }

    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| !is_droppable(name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();

    if kept.is_empty() {
        // Distinguishes `?` and `?utm_source=x` from no query at all.
        url.set_query(None);
    } else {
        let mut serializer = url.query_pairs_mut();
        serializer.clear();
        for (name, value) in &kept {
            serializer.append_pair(name, value);
        }
        drop(serializer);
    }

    if url.as_str().len() > MAX_URL_LEN {
        return Err(Rejected::TooLong);
    }
    Ok(url)
}

/// Resolve a possibly-relative `href` against the page it was found on, then
/// normalise it.
pub fn resolve(base: &Url, href: &str) -> Result<Url, Rejected> {
    let href = href.trim();
    // Cheap rejects before paying for a parse.
    if href.is_empty() || href.starts_with('#') {
        return Err(Rejected::NotWebScheme);
    }
    let joined = base.join(href).map_err(|_| Rejected::NotWebScheme)?;
    normalize(&joined)
}

/// A canonical string for "is this the same page?" comparisons.
///
/// Additionally lowercases the host, drops a trailing slash on the path, and
/// **sorts query parameters** — all things [`normalize`] refuses to do because
/// they can change what a server returns. Never fetch this; only compare it.
pub fn dedupe_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let path = url.path();
    let path = path.strip_suffix('/').filter(|p| !p.is_empty()).unwrap_or(path);

    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();

    let mut key = String::with_capacity(url.as_str().len());
    // Scheme is deliberately excluded: http and https of the same path are the
    // same document for our purposes, and treating them separately is a
    // reliable way to index everything twice.
    key.push_str(&host);
    key.push_str(path);
    if !pairs.is_empty() {
        key.push('?');
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                key.push('&');
            }
            key.push_str(k);
            key.push('=');
            key.push_str(v);
        }
    }
    key
}

/// The host we rate-limit against. Politeness is per-host, not per-URL.
pub fn host_of(url: &Url) -> Option<String> {
    url.host_str().map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::{Rejected, dedupe_key, host_of, normalize, resolve};
    use url::Url;

    fn norm(raw: &str) -> String {
        normalize(&Url::parse(raw).unwrap()).unwrap().to_string()
    }

    #[test]
    fn strips_fragment() {
        assert_eq!(norm("https://a.test/p#section"), "https://a.test/p");
    }

    #[test]
    fn strips_tracking_parameters_but_keeps_real_ones() {
        assert_eq!(
            norm("https://a.test/p?utm_source=x&id=7&fbclid=zz"),
            "https://a.test/p?id=7"
        );
    }

    #[test]
    fn strips_unenumerated_utm_parameters() {
        assert_eq!(norm("https://a.test/p?utm_anything=1"), "https://a.test/p");
    }

    #[test]
    fn strips_session_identifiers_case_insensitively() {
        assert_eq!(norm("https://a.test/p?PHPSESSID=abc"), "https://a.test/p");
    }

    #[test]
    fn a_query_that_is_entirely_tracking_becomes_no_query() {
        // Not "https://a.test/p?" — that would be a second distinct URL.
        assert_eq!(norm("https://a.test/p?utm_source=x"), "https://a.test/p");
    }

    #[test]
    fn keeps_ambiguous_parameters_we_are_not_sure_about() {
        // `ref` is meaningful on plenty of sites; dropping it would 404.
        assert_eq!(norm("https://a.test/p?ref=nav"), "https://a.test/p?ref=nav");
    }

    #[test]
    fn preserves_parameter_order_for_fetching() {
        assert_eq!(norm("https://a.test/p?b=2&a=1"), "https://a.test/p?b=2&a=1");
    }

    #[test]
    fn rejects_non_web_schemes() {
        let mailto = Url::parse("mailto:someone@a.test").unwrap();
        assert_eq!(normalize(&mailto), Err(Rejected::NotWebScheme));
    }

    #[test]
    fn rejects_absurdly_long_urls() {
        let long = format!("https://a.test/{}", "x".repeat(4000));
        assert_eq!(normalize(&Url::parse(&long).unwrap()), Err(Rejected::TooLong));
    }

    #[test]
    fn resolves_relative_links() {
        let base = Url::parse("https://a.test/dir/page.html").unwrap();
        assert_eq!(resolve(&base, "../other.html").unwrap().as_str(), "https://a.test/other.html");
        assert_eq!(resolve(&base, "/abs").unwrap().as_str(), "https://a.test/abs");
    }

    #[test]
    fn refuses_bare_fragment_links() {
        let base = Url::parse("https://a.test/p").unwrap();
        assert_eq!(resolve(&base, "#top"), Err(Rejected::NotWebScheme));
        assert_eq!(resolve(&base, "   "), Err(Rejected::NotWebScheme));
    }

    #[test]
    fn dedupe_key_ignores_scheme_and_parameter_order() {
        let a = Url::parse("https://A.test/p?b=2&a=1").unwrap();
        let b = Url::parse("http://a.test/p?a=1&b=2").unwrap();
        assert_eq!(dedupe_key(&a), dedupe_key(&b));
    }

    #[test]
    fn dedupe_key_ignores_a_trailing_slash() {
        let a = Url::parse("https://a.test/dir/").unwrap();
        let b = Url::parse("https://a.test/dir").unwrap();
        assert_eq!(dedupe_key(&a), dedupe_key(&b));
    }

    #[test]
    fn dedupe_key_keeps_the_root_path() {
        let root = Url::parse("https://a.test/").unwrap();
        assert_eq!(dedupe_key(&root), "a.test/");
    }

    #[test]
    fn dedupe_key_distinguishes_different_pages() {
        let a = Url::parse("https://a.test/one").unwrap();
        let b = Url::parse("https://a.test/two").unwrap();
        assert_ne!(dedupe_key(&a), dedupe_key(&b));
    }

    #[test]
    fn host_is_lowercased() {
        assert_eq!(host_of(&Url::parse("https://EXAMPLE.test/p").unwrap()).unwrap(), "example.test");
    }
}
