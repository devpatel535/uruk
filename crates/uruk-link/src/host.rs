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

/// The registrable domain of a host: the unit of "the same site".
///
/// `blog.example.com` and `shop.example.com` both reduce to `example.com`, so
/// a link between them is a site linking to itself and passes no authority.
///
/// # This used to be an approximation, and it was wrong in a known direction
///
/// It was "the last two labels", which is wrong twice over: it made two
/// unrelated British sites (`a.example.co.uk`, `b.other.co.uk`) look like one,
/// and two unrelated projects on a hosting service (`alice.github.io`,
/// `bob.github.io`) look like one. Both errors discarded votes that should
/// have counted — never inventing one — which was the safe direction but still
/// a weaker signal than the evidence supported.
///
/// It now consults the Public Suffix List, which is the only way to know that
/// `co.uk` is a registry and `github.io` is one too. See [`crate::suffix`] for
/// how the rules work and `data/README.md` for where the list comes from and
/// what licence it carries.
pub fn site(host: &str) -> &str {
    crate::suffix::registrable_domain(host)
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
    fn addresses_are_each_their_own_site() {
        // Two machines on a subnet are not one site, and the fixture crawls in
        // this repository depend on that being true.
        assert!(!same_site("127.0.0.2", "127.0.0.3"));
        assert!(same_site("127.0.0.2", "127.0.0.2"));
    }

    #[test]
    fn the_cases_the_old_approximation_got_wrong_are_now_right() {
        // This test used to assert the opposite, as a record of what the
        // two-label approximation could not do. The Public Suffix List is
        // what flipped it: two projects on a hosting service can now vouch
        // for each other, and two unrelated British sites are no longer one.
        assert!(!same_site("alice.github.io", "bob.github.io"));
        assert!(!same_site("a.example.co.uk", "b.other.co.uk"));

        // And the thing it was protecting against still holds.
        assert!(same_site("a.example.co.uk", "b.example.co.uk"));
        assert!(same_site("docs.alice.github.io", "blog.alice.github.io"));
    }
}
