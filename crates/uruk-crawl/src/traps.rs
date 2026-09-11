//! Crawler trap detection.
//!
//! A trap is any part of a site that generates unbounded URLs. They are almost
//! never malicious — usually it is a calendar with a "next month" link, or a
//! shop where every combination of filters is its own address (`RESEARCH.md`
//! §3.2). Left alone, one trap eats an entire crawl: the frontier grows faster
//! than it drains and every fetch returns the same page.
//!
//! None of these checks is clever. All of them are necessary, and they are
//! cheap enough to run on every URL before it enters the frontier.
//!
//! The last line of defence is not here: it is content-level near-duplicate
//! detection in [`crate::simhash`], which catches whatever slips past the URL
//! rules by noticing that we have read this text before.

use url::Url;

/// Limits that define what counts as a trap. Every one of these is a guess
/// that should be revisited once we have crawled something real.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Links away from a seed. The strongest single protection: a trap can
    /// only generate URLs one hop at a time.
    pub max_depth: u32,
    /// Pages taken from any one host, however large it is.
    pub max_pages_per_host: usize,
    /// Slashes in the path. Deep paths are usually generated, not authored.
    pub max_path_segments: usize,
    /// Distinct query parameters. Faceted navigation explodes here.
    pub max_query_params: usize,
    /// How many times one path segment may repeat before we call it a loop.
    pub max_segment_repeats: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 4,
            max_pages_per_host: 5_000,
            max_path_segments: 12,
            max_query_params: 4,
            max_segment_repeats: 3,
        }
    }
}

/// Why a URL was refused entry to the frontier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// Further from a seed than we are willing to go.
    TooDeep,
    /// This host has given us enough.
    HostQuotaReached,
    /// Path has more segments than any authored URL plausibly needs.
    PathTooDeep,
    /// More query parameters than a real page uses; faceted navigation.
    TooManyParameters,
    /// The same path segment repeats: `/a/b/a/b/a/b`.
    RepeatingPath,
    /// A date-like segment far outside any plausible archive.
    ImplausibleDate,
    /// An extension we have no use for even though the URL looks fine.
    UninterestingType,
}

/// File extensions worth skipping before we spend a request on them.
///
/// Deliberately short. The `Content-Type` check after fetching is the real
/// filter; this only avoids obviously pointless requests.
const SKIP_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "avif", "svg", "ico", "bmp", "tiff", "mp3", "mp4",
    "m4a", "m4v", "avi", "mov", "wmv", "flv", "webm", "ogg", "wav", "zip", "gz", "bz2", "xz",
    "7z", "rar", "tar", "exe", "dmg", "iso", "msi", "deb", "rpm", "apk", "woff", "woff2",
    "ttf", "otf", "eot", "css", "js", "mjs", "json", "xml", "rss", "atom", "doc", "docx",
    "xls", "xlsx", "ppt", "pptx", "psd", "ai", "eps", "dwg", "bin", "dat", "swf",
];

/// Years outside this range in a path segment suggest a generated calendar
/// rather than an archive. The infinite calendar is the classic trap: a "next
/// month" link that keeps working until the heat death of the universe.
const EARLIEST_PLAUSIBLE_YEAR: u32 = 1990;
const LATEST_PLAUSIBLE_YEAR: u32 = 2100;

/// Check a URL against every trap rule.
///
/// `depth` is hops from a seed; `host_count` is how many pages this host has
/// already contributed.
pub fn check(url: &Url, depth: u32, host_count: usize, limits: &Limits) -> Result<(), Trap> {
    if depth > limits.max_depth {
        return Err(Trap::TooDeep);
    }
    if host_count >= limits.max_pages_per_host {
        return Err(Trap::HostQuotaReached);
    }

    let segments: Vec<&str> =
        url.path().split('/').filter(|segment| !segment.is_empty()).collect();

    if segments.len() > limits.max_path_segments {
        return Err(Trap::PathTooDeep);
    }
    if url.query_pairs().count() > limits.max_query_params {
        return Err(Trap::TooManyParameters);
    }
    if let Some(last) = segments.last()
        && let Some((_, extension)) = last.rsplit_once('.')
        && SKIP_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
    {
        return Err(Trap::UninterestingType);
    }
    if has_repeating_segments(&segments, limits.max_segment_repeats) {
        return Err(Trap::RepeatingPath);
    }
    if has_implausible_year(&segments) {
        return Err(Trap::ImplausibleDate);
    }
    Ok(())
}

/// Does any single path segment repeat more than `allowed` times?
///
/// Catches both `/a/a/a/a` and the interleaved `/a/b/a/b/a/b` that a redirect
/// loop or a badly-built breadcrumb produces.
fn has_repeating_segments(segments: &[&str], allowed: usize) -> bool {
    for (index, segment) in segments.iter().enumerate() {
        // Single characters repeat innocently (`/a/b/c/`), so ignore them.
        if segment.len() < 2 {
            continue;
        }
        let count = segments[index..].iter().filter(|other| *other == segment).count();
        if count > allowed {
            return true;
        }
    }
    false
}

/// Is there a year-like segment well outside any plausible archive?
fn has_implausible_year(segments: &[&str]) -> bool {
    segments.iter().any(|segment| {
        segment.len() == 4
            && segment.bytes().all(|b| b.is_ascii_digit())
            && segment
                .parse::<u32>()
                .is_ok_and(|year| !(EARLIEST_PLAUSIBLE_YEAR..=LATEST_PLAUSIBLE_YEAR).contains(&year))
    })
}

#[cfg(test)]
mod tests {
    use super::{Limits, Trap, check};
    use url::Url;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    fn allow(raw: &str) {
        assert_eq!(check(&url(raw), 0, 0, &Limits::default()), Ok(()), "should have allowed {raw}");
    }

    fn deny(raw: &str, expected: Trap) {
        assert_eq!(check(&url(raw), 0, 0, &Limits::default()), Err(expected), "for {raw}");
    }

    #[test]
    fn ordinary_urls_pass() {
        allow("https://a.test/");
        allow("https://a.test/blog/2024/03/clay-tablets");
        allow("https://a.test/wiki/Uruk");
        allow("https://a.test/search?q=cuneiform&page=2");
    }

    #[test]
    fn depth_is_bounded() {
        let limits = Limits::default();
        assert_eq!(check(&url("https://a.test/p"), limits.max_depth, 0, &limits), Ok(()));
        assert_eq!(
            check(&url("https://a.test/p"), limits.max_depth + 1, 0, &limits),
            Err(Trap::TooDeep)
        );
    }

    #[test]
    fn a_host_quota_stops_one_site_dominating() {
        let limits = Limits::default();
        assert_eq!(
            check(&url("https://a.test/p"), 0, limits.max_pages_per_host, &limits),
            Err(Trap::HostQuotaReached)
        );
    }

    #[test]
    fn very_deep_paths_are_refused() {
        deny("https://a.test/a/b/c/d/e/f/g/h/i/j/k/l/m/n", Trap::PathTooDeep);
    }

    #[test]
    fn faceted_navigation_is_refused() {
        // The shop filter explosion: every combination is its own URL.
        deny(
            "https://a.test/shop?colour=red&size=xl&brand=acme&sort=price&page=3&instock=1",
            Trap::TooManyParameters,
        );
    }

    #[test]
    fn repeating_path_segments_are_refused() {
        deny("https://a.test/dir/dir/dir/dir/page", Trap::RepeatingPath);
        // Interleaved repetition, the redirect-loop shape.
        deny("https://a.test/aa/bb/aa/bb/aa/bb/aa", Trap::RepeatingPath);
    }

    #[test]
    fn innocent_short_segments_do_not_count_as_repetition() {
        allow("https://a.test/a/b/a/b/a/b");
    }

    #[test]
    fn an_infinite_calendar_is_refused() {
        // "Next month" links eventually walk out of plausible time.
        deny("https://a.test/events/2387/04", Trap::ImplausibleDate);
        deny("https://a.test/events/0001/01", Trap::ImplausibleDate);
    }

    #[test]
    fn real_archive_dates_are_fine() {
        allow("https://a.test/blog/1999/12/party");
        allow("https://a.test/blog/2026/01/post");
    }

    #[test]
    fn non_article_file_types_are_skipped() {
        deny("https://a.test/img/photo.JPG", Trap::UninterestingType);
        deny("https://a.test/app.js", Trap::UninterestingType);
        deny("https://a.test/archive.tar.gz", Trap::UninterestingType);
    }

    #[test]
    fn html_extensions_are_not_skipped() {
        allow("https://a.test/page.html");
        allow("https://a.test/page.htm");
        allow("https://a.test/page.php");
        allow("https://a.test/page.aspx");
    }

    #[test]
    fn a_dot_in_the_path_that_is_not_an_extension_is_harmless() {
        allow("https://a.test/docs/v1.2.3/guide");
    }
}
