//! The host part of a URL, and which hosts are one site.
//!
//! Duplicated from nothing: `uruk-link` answers the same question for the link
//! graph, but it depends on this crate's *sibling* rather than on this crate,
//! and making the indexer depend on the link graph to tokenise a document
//! would invert the layering for one string function.
//!
//! The two must agree, because they are deciding the same thing — whether a
//! link is a site talking about itself — in two places. A test in `uruk-link`
//! asserts they do.

/// The host part of a URL.
///
/// Deliberately a string slice rather than a parse: the crawler already
/// normalised and validated these URLs, so a second full parse per document
/// would be paid for nothing.
pub fn url_host(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = authority
        .split_once(':')
        .map_or(authority, |(host, _)| host);
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

/// The registrable domain: the unit of "the same site".
///
/// Re-exported from `uruk-link`, which owns the Public Suffix List, so that
/// the indexer and the link graph cannot drift apart about what a site is.
pub use uruk_link::suffix::registrable_domain;
