//! `robots.txt` parsing and matching, per RFC 9309 plus the widely-implemented
//! extensions.
//!
//! This is written rather than taken from a crate on purpose. Obeying
//! `robots.txt` is a stated value of this project, the matching rules are
//! subtle enough to get quietly wrong, and "we were polite" is a claim we
//! should be able to back with tests.
//!
//! The rules implemented here:
//!
//! - Records are `field: value`, `#` starts a comment, field names are
//!   case-insensitive.
//! - Consecutive `User-agent` lines introduce one group, which the following
//!   rules belong to.
//! - A group applies to us if one of its user-agent values is a case-
//!   insensitive prefix of our product token. The group whose matching value is
//!   **longest** wins; `*` is the fallback and always loses to a real match.
//! - Path patterns support `*` (any sequence) and a trailing `$` (end anchor).
//! - Of the rules that match, the one with the **longest pattern** wins. On a
//!   tie, `Allow` beats `Disallow` — this is what lets a site say "stay out of
//!   `/private/` except `/private/index.html`".
//! - An empty `Disallow:` value is not a rule; it is the conventional way of
//!   saying "everything is allowed".

use std::time::Duration;

/// What the HTTP status of `robots.txt` itself implies, before any parsing.
///
/// RFC 9309 §2.3.1 is specific about this and it is easy to get backwards: a
/// server that refuses to show us `robots.txt` has not thereby granted us
/// permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// 2xx: use the body.
    Body(String),
    /// 4xx other than 401/403: no restrictions.
    Missing,
    /// 401/403: treat the entire site as disallowed.
    AccessDenied,
    /// 5xx or a network failure: treat as disallowed for now and try later.
    Unavailable,
}

#[derive(Debug, Clone)]
struct Rule {
    allow: bool,
    pattern: String,
}

#[derive(Debug, Clone, Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay: Option<Duration>,
}

/// A parsed `robots.txt`, ready to answer questions about paths.
#[derive(Debug, Clone)]
pub struct Robots {
    groups: Vec<Group>,
    /// Sitemap URLs are global rather than group-scoped. Useful seed material.
    sitemaps: Vec<String>,
    /// Set when the status code alone decided the outcome.
    blanket: Option<bool>,
}

impl Robots {
    /// Build from a fetch outcome.
    pub fn from_fetch(fetched: &Fetched) -> Self {
        match fetched {
            Fetched::Body(body) => Self::parse(body),
            Fetched::Missing => Self::blanket(true),
            Fetched::AccessDenied | Fetched::Unavailable => Self::blanket(false),
        }
    }

    fn blanket(allowed: bool) -> Self {
        Self { groups: Vec::new(), sitemaps: Vec::new(), blanket: Some(allowed) }
    }

    /// Parse a `robots.txt` body.
    pub fn parse(body: &str) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        let mut sitemaps = Vec::new();
        // True while we are reading the `User-agent` lines that open a group,
        // so that a run of them shares one rule set.
        let mut collecting_agents = false;

        for line in body.lines() {
            let line = match line.split_once('#') {
                Some((before, _)) => before,
                None => line,
            }
            .trim();
            if line.is_empty() {
                continue;
            }
            let Some((field, value)) = line.split_once(':') else { continue };
            let field = field.trim().to_ascii_lowercase();
            let value = value.trim();

            match field.as_str() {
                "user-agent" => {
                    if !collecting_agents || groups.is_empty() {
                        groups.push(Group::default());
                        collecting_agents = true;
                    }
                    if let Some(group) = groups.last_mut() {
                        group.agents.push(value.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" => {
                    collecting_agents = false;
                    // A rule before any `User-agent` line has no group; the
                    // conventional reading is that it applies to everyone.
                    if groups.is_empty() {
                        groups.push(Group { agents: vec!["*".into()], ..Group::default() });
                    }
                    // `Disallow:` with an empty value means "nothing is
                    // disallowed" and must not become a rule matching "".
                    if field == "disallow" && value.is_empty() {
                        continue;
                    }
                    if let Some(group) = groups.last_mut() {
                        group.rules.push(Rule { allow: field == "allow", pattern: value.to_owned() });
                    }
                }
                "crawl-delay" => {
                    collecting_agents = false;
                    if let Some(group) = groups.last_mut()
                        && let Ok(seconds) = value.parse::<f64>()
                        && seconds.is_finite()
                        && seconds >= 0.0
                    {
                        group.crawl_delay = Some(Duration::from_secs_f64(seconds.min(3600.0)));
                    }
                }
                "sitemap" => sitemaps.push(value.to_owned()),
                _ => {}
            }
        }

        Self { groups, sitemaps, blanket: None }
    }

    /// The group that applies to `product_token`, most specific match winning.
    fn group_for(&self, product_token: &str) -> Option<&Group> {
        let token = product_token.to_ascii_lowercase();
        let mut best: Option<(usize, &Group)> = None;

        for group in &self.groups {
            for agent in &group.agents {
                // `*` is the fallback; specificity 0 so any real match beats it.
                let specificity = if agent == "*" {
                    0
                } else if token.starts_with(agent.as_str()) {
                    agent.len()
                } else {
                    continue;
                };
                if best.is_none_or(|(best_spec, _)| specificity > best_spec) {
                    best = Some((specificity, group));
                }
            }
        }
        best.map(|(_, group)| group)
    }

    /// May `product_token` fetch `path`?
    ///
    /// `path` should include the query string, since patterns are allowed to
    /// match against it.
    pub fn allows(&self, product_token: &str, path: &str) -> bool {
        if let Some(allowed) = self.blanket {
            return allowed;
        }
        let Some(group) = self.group_for(product_token) else {
            return true; // No group speaks to us, so nothing restricts us.
        };

        // Longest matching pattern wins; Allow wins ties.
        let mut best: Option<(usize, bool)> = None;
        for rule in &group.rules {
            if glob_match(&rule.pattern, path) {
                let len = rule.pattern.len();
                let better = match best {
                    None => true,
                    Some((best_len, best_allow)) => {
                        len > best_len || (len == best_len && rule.allow && !best_allow)
                    }
                };
                if better {
                    best = Some((len, rule.allow));
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }

    /// The `Crawl-delay` this crawler should honour, if the site declares one.
    ///
    /// Not part of RFC 9309, and Google ignores it. We honour it: a site that
    /// took the trouble to ask for a slower rate should get one.
    pub fn crawl_delay(&self, product_token: &str) -> Option<Duration> {
        self.group_for(product_token).and_then(|group| group.crawl_delay)
    }

    /// Sitemap URLs declared in the file. Good seed material.
    pub fn sitemaps(&self) -> &[String] {
        &self.sitemaps
    }
}

/// Match a `robots.txt` path pattern against a path.
///
/// `*` matches any sequence; a trailing `$` anchors the match to the end.
/// Operates on bytes so that a non-UTF-8-boundary index cannot panic.
fn glob_match(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(rest) => (rest, true),
        None => (pattern, false),
    };
    let path = path.as_bytes();
    let segments: Vec<&[u8]> = pattern.as_bytes().split(|&b| b == b'*').collect();

    // No wildcard: a plain prefix test, or an exact one when anchored.
    if segments.len() == 1 {
        let only = segments[0];
        return if anchored { path == only } else { path.starts_with(only) };
    }

    let last = segments.len() - 1;
    let mut pos = 0usize;
    for (i, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            continue; // leading, trailing or doubled `*`
        }
        if i == 0 {
            if !path.starts_with(segment) {
                return false;
            }
            pos = segment.len();
        } else if i == last && anchored {
            // The final literal must sit at the very end, not merely somewhere.
            return path.len() >= pos + segment.len() && path[pos..].ends_with(segment);
        } else {
            match find(&path[pos..], segment) {
                Some(offset) => pos += offset + segment.len(),
                None => return false,
            }
        }
    }
    // Falling out of the loop while anchored means the pattern ended with `*`,
    // which accepts any remaining tail.
    true
}

/// Index of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::{Fetched, Robots};
    use std::time::Duration;

    const US: &str = "uruk-crawl";

    #[test]
    fn missing_robots_allows_everything() {
        let robots = Robots::from_fetch(&Fetched::Missing);
        assert!(robots.allows(US, "/anything"));
    }

    #[test]
    fn forbidden_robots_disallows_everything() {
        // A server that hides robots.txt has not granted permission.
        let robots = Robots::from_fetch(&Fetched::AccessDenied);
        assert!(!robots.allows(US, "/"));
    }

    #[test]
    fn unavailable_robots_disallows_everything() {
        let robots = Robots::from_fetch(&Fetched::Unavailable);
        assert!(!robots.allows(US, "/"));
    }

    #[test]
    fn disallow_all_blocks_us() {
        let robots = Robots::parse("User-agent: *\nDisallow: /");
        assert!(!robots.allows(US, "/"));
        assert!(!robots.allows(US, "/page"));
    }

    #[test]
    fn empty_disallow_means_allow_everything() {
        let robots = Robots::parse("User-agent: *\nDisallow:");
        assert!(robots.allows(US, "/anything"));
    }

    #[test]
    fn longest_matching_rule_wins() {
        let robots = Robots::parse("User-agent: *\nDisallow: /private/\nAllow: /private/public.html");
        assert!(!robots.allows(US, "/private/secret.html"));
        assert!(robots.allows(US, "/private/public.html"));
    }

    #[test]
    fn allow_beats_disallow_on_an_exact_tie() {
        let robots = Robots::parse("User-agent: *\nDisallow: /x\nAllow: /x");
        assert!(robots.allows(US, "/x"));
    }

    #[test]
    fn wildcards_match_any_sequence() {
        let robots = Robots::parse("User-agent: *\nDisallow: /*/private");
        assert!(!robots.allows(US, "/a/private"));
        assert!(!robots.allows(US, "/a/b/c/private"));
        assert!(robots.allows(US, "/private"));
    }

    #[test]
    fn dollar_anchors_to_the_end() {
        let robots = Robots::parse("User-agent: *\nDisallow: /*.pdf$");
        assert!(!robots.allows(US, "/doc.pdf"));
        assert!(!robots.allows(US, "/dir/doc.pdf"));
        // Anchored, so this must NOT match.
        assert!(robots.allows(US, "/doc.pdf.html"));
    }

    #[test]
    fn a_named_group_beats_the_wildcard_group() {
        let robots = Robots::parse(
            "User-agent: *\nDisallow: /\n\nUser-agent: uruk-crawl\nDisallow: /admin",
        );
        assert!(robots.allows(US, "/page"), "our own group should apply, not *");
        assert!(!robots.allows(US, "/admin"));
    }

    #[test]
    fn a_user_agent_prefix_matches_our_token() {
        let robots = Robots::parse("User-agent: *\nDisallow: /\n\nUser-agent: uruk\nDisallow: /x");
        assert!(robots.allows(US, "/page"));
        assert!(!robots.allows(US, "/x"));
    }

    #[test]
    fn the_more_specific_of_two_named_groups_wins() {
        let robots = Robots::parse(
            "User-agent: uruk\nDisallow: /\n\nUser-agent: uruk-crawl\nDisallow: /only-this",
        );
        assert!(robots.allows(US, "/page"));
        assert!(!robots.allows(US, "/only-this"));
    }

    #[test]
    fn user_agent_matching_ignores_case() {
        let robots = Robots::parse("User-Agent: URUK-CRAWL\nDisallow: /x");
        assert!(!robots.allows(US, "/x"));
    }

    #[test]
    fn consecutive_user_agent_lines_share_one_rule_set() {
        let robots = Robots::parse("User-agent: alpha\nUser-agent: uruk-crawl\nDisallow: /shared");
        assert!(!robots.allows(US, "/shared"));
    }

    #[test]
    fn rules_for_other_crawlers_do_not_bind_us() {
        let robots = Robots::parse("User-agent: Googlebot\nDisallow: /\n");
        assert!(robots.allows(US, "/page"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let robots = Robots::parse("# leading comment\n\nUser-agent: *  # us\nDisallow: /x # why\n");
        assert!(!robots.allows(US, "/x"));
        assert!(robots.allows(US, "/y"));
    }

    #[test]
    fn crawl_delay_is_read_and_clamped() {
        let robots = Robots::parse("User-agent: *\nCrawl-delay: 2.5");
        assert_eq!(robots.crawl_delay(US), Some(Duration::from_secs_f64(2.5)));

        let absurd = Robots::parse("User-agent: *\nCrawl-delay: 99999999");
        assert_eq!(absurd.crawl_delay(US), Some(Duration::from_secs(3600)));

        let broken = Robots::parse("User-agent: *\nCrawl-delay: soon");
        assert_eq!(broken.crawl_delay(US), None);
    }

    #[test]
    fn sitemaps_are_collected() {
        let robots = Robots::parse("Sitemap: https://a.test/sitemap.xml\nUser-agent: *\nDisallow:");
        assert_eq!(robots.sitemaps(), ["https://a.test/sitemap.xml"]);
    }

    #[test]
    fn patterns_may_match_the_query_string() {
        let robots = Robots::parse("User-agent: *\nDisallow: /*?sort=");
        assert!(!robots.allows(US, "/list?sort=price"));
        assert!(robots.allows(US, "/list?page=2"));
    }

    #[test]
    fn rules_before_any_user_agent_line_apply_to_everyone() {
        let robots = Robots::parse("Disallow: /x\n");
        assert!(!robots.allows(US, "/x"));
    }

    #[test]
    fn garbage_input_does_not_panic_and_restricts_nothing() {
        let robots = Robots::parse("\u{0}\u{1}not a robots file\njust prose, no colons\n");
        assert!(robots.allows(US, "/"));
    }
}
