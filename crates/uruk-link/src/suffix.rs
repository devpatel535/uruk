//! The Public Suffix List: which part of a host is a registry.
//!
//! [`crate::host::site`] needs to answer "are these two hosts the same site?",
//! and the answer is not "do they share the last two labels". `example.co.uk`
//! is a site; `co.uk` is a registry nobody owns. `alice.github.io` and
//! `bob.github.io` share two labels and belong to different people.
//!
//! There is no rule that derives this — it is a fact about how each registry
//! chose to organise itself, and the only way to know it is to be told. The
//! Public Suffix List is that telling: ten thousand rules, maintained by
//! Mozilla, vendored in `data/` with its licence and provenance recorded in
//! `data/README.md`.
//!
//! # The rules, which are not quite as simple as they look
//!
//! Three kinds, and the interaction between the last two is the whole reason
//! this is a module rather than a set lookup:
//!
//! - **Ordinary** — `com.ac` — matches that exact suffix.
//! - **Wildcard** — `*.ck` — any single label followed by `ck` is a suffix, so
//!   `foo.ck` is a registry and `bar.foo.ck` is a site.
//! - **Exception** — `!www.ck` — overrides a wildcard, so `www.ck` is a site
//!   despite the rule above.
//!
//! The prevailing rule is the longest match, except that an exception always
//! wins. If nothing matches, the rule is `*`: the last label alone is the
//! suffix, which is what makes an unknown new TLD behave sensibly.

use std::collections::HashSet;
use std::sync::OnceLock;

/// The list itself, compiled in.
///
/// Vendored rather than fetched: a crawler that needs the network to decide
/// whether two hosts are the same site would make its own link graph depend on
/// something outside the crawl.
const LIST: &str = include_str!("../data/public_suffix_list.dat");

struct Rules {
    ordinary: HashSet<&'static str>,
    /// Stored without the leading `*.`, so a lookup asks about the parent.
    wildcard: HashSet<&'static str>,
    /// Stored without the leading `!`.
    exception: HashSet<&'static str>,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| {
        let mut ordinary = HashSet::new();
        let mut wildcard = HashSet::new();
        let mut exception = HashSet::new();

        for line in LIST.lines() {
            let line = line.trim();
            // Comments start with `//`; blank lines separate sections. The
            // ICANN and private sections are both used: a site hosted on
            // someone's subdomain service is still a different site from its
            // neighbours, which is exactly the case this exists for.
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            if let Some(rest) = line.strip_prefix('!') {
                exception.insert(rest);
            } else if let Some(rest) = line.strip_prefix("*.") {
                wildcard.insert(rest);
            } else {
                ordinary.insert(line);
            }
        }

        Rules {
            ordinary,
            wildcard,
            exception,
        }
    })
}

/// Byte offsets where each label of `host` starts.
///
/// Every candidate suffix is a slice of the original string rather than a
/// joined copy, so a lookup allocates nothing — this runs once per link in a
/// crawl, which is tens of millions of times.
fn label_starts(host: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (at, byte) in host.bytes().enumerate() {
        if byte == b'.' {
            starts.push(at + 1);
        }
    }
    starts
}

/// Whether the host is an IP address rather than a name.
///
/// An address has no registrable domain: `127.0.0.1` is not a subdomain of
/// `0.1`. Without this, two unrelated machines on one subnet would look like
/// one site, and — worse in the other direction — two addresses differing in
/// the last octet would look like different sites for the wrong reason.
fn is_address(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
        || host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .is_some_and(|inner| inner.parse::<std::net::Ipv6Addr>().is_ok())
}

/// The registrable domain of `host`: its public suffix plus one label.
///
/// This is the unit of "same site". Returns the whole host when there is
/// nothing to strip — an address, a single label, or a host that *is* a public
/// suffix, which is a thing crawls encounter and which has no owner to
/// attribute a link to.
pub fn registrable_domain(host: &str) -> &str {
    if host.is_empty() || is_address(host) {
        return host;
    }

    let starts = label_starts(host);
    let rules = rules();
    let candidate = |index: usize| &host[starts[index]..];

    // An exception wins outright, and its public suffix is the rule with its
    // leftmost label removed — so the exception itself is registrable.
    for index in 0..starts.len() {
        if rules.exception.contains(candidate(index)) {
            return candidate(index);
        }
    }

    // Otherwise the longest match, which is the smallest starting index.
    // Default `*`: the last label alone, so an unknown TLD still yields a
    // sensible site.
    let mut suffix = starts.len() - 1;
    for index in 0..starts.len() {
        if rules.ordinary.contains(candidate(index)) {
            suffix = index;
            break;
        }
        // `*.ck` means "one label, then ck": this label is part of the suffix
        // when the *next* one matches a wildcard rule.
        if index + 1 < starts.len() && rules.wildcard.contains(candidate(index + 1)) {
            suffix = index;
            break;
        }
    }

    if suffix == 0 {
        // The host is itself a public suffix. Nobody owns it, so there is no
        // smaller site to reduce to.
        return host;
    }
    candidate(suffix - 1)
}

#[cfg(test)]
mod tests {
    use super::registrable_domain;

    #[test]
    fn ordinary_domains_reduce_to_one_label_plus_the_suffix() {
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("www.example.com"), "example.com");
        assert_eq!(registrable_domain("a.b.c.example.com"), "example.com");
    }

    #[test]
    fn multi_label_registries_are_known_not_guessed() {
        // The case the old two-label approximation got wrong in one direction.
        assert_eq!(registrable_domain("example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("www.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("shop.example.com.au"), "example.com.au");
    }

    #[test]
    fn subdomain_hosting_services_are_registries() {
        // And the case it got wrong in the other: two projects on one hosting
        // service are two sites, and must be able to vouch for each other.
        assert_eq!(registrable_domain("alice.github.io"), "alice.github.io");
        assert_eq!(registrable_domain("bob.github.io"), "bob.github.io");
        assert_ne!(
            registrable_domain("alice.github.io"),
            registrable_domain("bob.github.io")
        );
    }

    #[test]
    fn a_host_that_is_only_a_suffix_has_no_smaller_site() {
        assert_eq!(registrable_domain("co.uk"), "co.uk");
        assert_eq!(registrable_domain("com"), "com");
        assert_eq!(registrable_domain("github.io"), "github.io");
    }

    #[test]
    fn wildcard_rules_are_honoured() {
        // `*.ck` in the list: one label then `ck` is a registry.
        assert_eq!(registrable_domain("example.foo.ck"), "example.foo.ck");
        assert_eq!(registrable_domain("a.b.example.foo.ck"), "example.foo.ck");
    }

    #[test]
    fn exception_rules_beat_the_wildcard_they_sit_under() {
        // `!www.ck` overrides `*.ck`, so www.ck is a site despite the wildcard.
        assert_eq!(registrable_domain("www.ck"), "www.ck");
    }

    #[test]
    fn an_unknown_suffix_falls_back_to_one_label() {
        // A TLD delegated after this copy of the list was vendored. Treating
        // it as an ordinary suffix is the sensible failure: the same answer
        // the list would give for a plain new TLD.
        assert_eq!(
            registrable_domain("example.zzznotarealtldatall"),
            "example.zzznotarealtldatall"
        );
        assert_eq!(
            registrable_domain("www.example.zzznotarealtldatall"),
            "example.zzznotarealtldatall"
        );
    }

    #[test]
    fn an_address_is_its_own_site() {
        // 127.0.0.1 is not a subdomain of 0.1, and a crawl of a local fixture
        // or an IP-addressed host must not have links between machines folded
        // together by a rule about names.
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
        assert_eq!(registrable_domain("192.168.1.10"), "192.168.1.10");
        assert_eq!(registrable_domain("::1"), "::1");
    }

    #[test]
    fn a_bare_label_is_its_own_site() {
        assert_eq!(registrable_domain("localhost"), "localhost");
        assert_eq!(registrable_domain(""), "");
    }

    #[test]
    fn the_list_actually_parsed() {
        // A vendored data file that silently failed to load would make every
        // lookup fall back to "last label", which looks like working software
        // and is the old bug with extra steps.
        assert_eq!(registrable_domain("bbc.co.uk"), "bbc.co.uk");
        assert_ne!(registrable_domain("bbc.co.uk"), "co.uk");
    }
}
