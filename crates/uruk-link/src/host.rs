//! Which links count as a vote, and which are a site voting for itself.
//!
//! The link graph is built over **hosts**, not pages, and `RESEARCH.md` §5.3
//! explains why: on a topical crawl a page-level graph is so truncated that
//! `PageRank` over it mostly measures our own crawl order. A host-level graph
//! survives truncation much better, because most hosts worth ranking are
//! linked from several places inside the crawl.
//!
//! That leaves one question, and it is the one that decides whether the whole
//! signal is spammable: **when are two hosts the same site?** A site that can
//! vote for itself by adding subdomains has a free authority generator.

/// A host, normalised for use as a graph node.
///
/// Lowercased, with a leading `www.` and any trailing dot removed. Ports and
/// userinfo are already gone by the time a URL reaches the crawl store, but
/// this strips them anyway rather than trusting that.
pub fn node(url: &str) -> Option<String> {
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

/// The last two labels of a host: an approximation of "the same site".
///
/// `blog.example.com` and `shop.example.com` both reduce to `example.com`, so
/// a link between them is recognised as a site linking to itself and does not
/// pass authority.
///
/// # This is an approximation, and it is wrong in a known direction
///
/// The correct tool is the Public Suffix List, which knows that `example.co.uk`
/// is a site while `co.uk` is not, and that every `*.github.io` is a *different*
/// site despite sharing two labels.
///
/// Without it this function makes two mistakes:
///
/// - `a.example.co.uk` and `b.other.co.uk` both reduce to `co.uk`, so links
///   between unrelated British sites are discarded as self-links.
/// - `alice.github.io` and `bob.github.io` reduce to `github.io`, so one
///   genuinely cannot vote for the other.
///
/// Both mistakes **discard votes that should have counted**. None of them
/// *creates* a vote. That asymmetry is deliberate: an authority signal that
/// undercounts is a weaker signal, while one that overcounts is an attack
/// surface, and this is the signal spam attacks hardest (`RESEARCH.md` §5.3).
///
/// Adopting a real public suffix list is the fix, and it is a data file plus a
/// lookup rather than a redesign. It is not done here because the list has to
/// be shipped, kept current, and licensed, which is a decision rather than a
/// detail.
pub fn site(host: &str) -> &str {
    let mut labels = host.rsplitn(3, '.');
    let Some(last) = labels.next() else {
        return host;
    };
    let Some(second) = labels.next() else {
        return host;
    };
    // `second.last` is the tail; find where it starts in the original.
    let start = host.len() - (second.len() + 1 + last.len());
    &host[start..]
}

/// Whether a link from `from` to `to` is a site linking to itself.
pub fn same_site(from: &str, to: &str) -> bool {
    site(from) == site(to)
}

#[cfg(test)]
mod tests {
    use super::{node, same_site, site};

    #[test]
    fn hosts_are_lowercased_and_stripped_of_www() {
        assert_eq!(
            node("https://WWW.Example.COM/a/b?c").as_deref(),
            Some("example.com")
        );
        assert_eq!(node("http://example.com").as_deref(), Some("example.com"));
    }

    #[test]
    fn ports_userinfo_and_trailing_dots_do_not_make_a_second_host() {
        // All four of these are the same machine, and a graph that treats them
        // as four nodes has four times too little evidence about each.
        for url in [
            "https://example.com/",
            "https://example.com:443/",
            "https://user@example.com/",
            "https://example.com./",
        ] {
            assert_eq!(node(url).as_deref(), Some("example.com"), "{url}");
        }
    }

    #[test]
    fn a_url_without_a_host_is_not_a_node() {
        assert_eq!(node(""), None);
        assert_eq!(node("https:///path"), None);
    }

    #[test]
    fn subdomains_of_one_domain_are_one_site() {
        assert!(same_site("blog.example.com", "shop.example.com"));
        assert!(same_site("example.com", "deep.nested.example.com"));
    }

    #[test]
    fn unrelated_domains_are_different_sites() {
        assert!(!same_site("example.com", "example.org"));
        assert!(!same_site("example.com", "notexample.com"));
    }

    #[test]
    fn a_bare_label_is_its_own_site() {
        assert_eq!(site("localhost"), "localhost");
        assert_eq!(site("example.com"), "example.com");
    }

    #[test]
    fn the_known_approximation_errs_towards_discarding_votes() {
        // Documented in `site`: without a public suffix list these look like
        // one site. The test exists so the limitation is visible rather than
        // discovered, and so that adopting a real list has a failing test to
        // flip.
        assert!(same_site("alice.github.io", "bob.github.io"));
        assert!(same_site("a.example.co.uk", "b.other.co.uk"));
    }
}
