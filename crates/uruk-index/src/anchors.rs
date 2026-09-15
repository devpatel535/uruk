//! What other people call a page.
//!
//! Anchor text is the words inside a link. It belongs to the link's *target*,
//! not to the page it sits on, so collecting it needs a pass over the whole
//! crawl before any document can be indexed.
//!
//! It earns its place because **a page often does not contain the words people
//! use to look for it**. A university's admissions page may never say
//! "how to apply"; the hundred pages linking to it do. That is information the
//! page itself cannot provide, and it is why anchor text has been a strong
//! ranking signal since it was first used.
//!
//! # It is also the easiest field to attack
//!
//! Pointing a thousand links with identical text at a page is cheap, and doing
//! it to make a page rank for words that have nothing to do with it is old
//! enough to have a name. So most of this module is about what does **not**
//! count:
//!
//! - **A site's own links.** Navigation, breadcrumbs, "read more" — a site
//!   describing itself is not other people describing it. Same-site is decided
//!   by registrable domain, the same rule the link graph uses.
//! - **Repeats from one host.** A sitewide footer link is one host's opinion,
//!   however many pages carry it. Each distinct phrase counts once per source
//!   host, which is the anchor-text version of counting distinct hosts rather
//!   than links.
//! - **`rel="nofollow"`**, for the same reason the link graph ignores it: the
//!   author has declined to vouch.
//! - **Anything past the caps below.** Both a memory bound and a spam bound.
//!
//! What survives is a short piece of text per document: the distinct things
//! independent sites call it.

use std::collections::{BTreeSet, HashMap};

use uruk_crawl::store::{StoreError, StoreReader};

use crate::host::{registrable_domain, url_host};

/// Distinct phrases kept per document.
///
/// Twelve independent descriptions is already more signal than a title, and
/// the thirteenth is not adding much beyond a bigger attack surface.
pub const MAX_PHRASES: usize = 12;

/// Characters kept in one anchor phrase.
///
/// Link text is usually two or three words. A "phrase" of two hundred
/// characters is a paragraph somebody wrapped in a link, and indexing it as a
/// description of the target is wrong in the direction that helps an attacker.
pub const MAX_PHRASE_CHARS: usize = 80;

/// Characters kept per document, across all phrases.
pub const MAX_TOTAL_CHARS: usize = 400;

/// Anchor text gathered per target URL.
#[derive(Debug, Default)]
pub struct Anchors {
    /// Target URL -> the distinct phrases pointed at it.
    by_target: HashMap<String, BTreeSet<String>>,
    /// Sources already counted for a target, so one host cannot repeat itself.
    /// Keyed by target and source site together.
    counted: BTreeSet<(String, String, String)>,
    pub links_seen: usize,
    pub nofollow: usize,
    pub same_site: usize,
    pub repeated: usize,
    pub too_long: usize,
}

impl Anchors {
    /// Read every page's outgoing links and gather anchor text by target.
    ///
    /// A whole pass over the crawl before indexing starts. That is the cost of
    /// a signal that is a property of the corpus rather than of a page.
    pub fn collect(store: &mut StoreReader) -> Result<Self, StoreError> {
        let mut anchors = Self::default();

        for record in store.records()? {
            let record = record?;
            let Some(source) = url_host(&record.final_url) else {
                continue;
            };
            let source_site = registrable_domain(&source).to_owned();

            for link in &record.links {
                anchors.links_seen += 1;
                if link.nofollow {
                    anchors.nofollow += 1;
                    continue;
                }
                let Some(target_host) = url_host(&link.url) else {
                    continue;
                };
                if registrable_domain(&target_host) == source_site {
                    anchors.same_site += 1;
                    continue;
                }

                let phrase = tidy(&link.anchor);
                if phrase.is_empty() {
                    continue;
                }
                if phrase.chars().count() > MAX_PHRASE_CHARS {
                    anchors.too_long += 1;
                    continue;
                }

                // One phrase, from one site, about one target, counts once.
                let key = (link.url.clone(), source_site.clone(), phrase.clone());
                if !anchors.counted.insert(key) {
                    anchors.repeated += 1;
                    continue;
                }

                let phrases = anchors.by_target.entry(link.url.clone()).or_default();
                if phrases.len() < MAX_PHRASES {
                    phrases.insert(phrase);
                }
            }
        }

        Ok(anchors)
    }

    /// The anchor text for a document, as one string to tokenise.
    ///
    /// Matched on both the URL asked for and the one redirected to, because a
    /// link points at whichever the author wrote and the crawl stored both.
    pub fn text_for(&self, url: &str, final_url: &str) -> String {
        let mut out = String::new();
        for key in [url, final_url] {
            let Some(phrases) = self.by_target.get(key) else {
                continue;
            };
            for phrase in phrases {
                if out.len() + phrase.len() + 1 > MAX_TOTAL_CHARS {
                    return out;
                }
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(phrase);
            }
            if url == final_url {
                break;
            }
        }
        out
    }

    /// Documents with any anchor text at all.
    pub fn described(&self) -> usize {
        self.by_target.len()
    }
}

/// Collapse whitespace and lowercase, so "Clay  Tablets" and "clay tablets"
/// are one phrase rather than two slots in a bounded set.
fn tidy(anchor: &str) -> String {
    anchor
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{Anchors, MAX_PHRASE_CHARS, tidy};

    #[test]
    fn whitespace_and_case_do_not_make_two_phrases() {
        assert_eq!(tidy("  Clay   Tablets\n"), "clay tablets");
        assert_eq!(tidy("clay tablets"), tidy("CLAY  TABLETS"));
    }

    #[test]
    fn an_empty_anchor_is_nothing() {
        assert!(tidy("   ").is_empty());
    }

    #[test]
    fn text_for_falls_back_between_the_two_urls() {
        let mut anchors = Anchors::default();
        anchors
            .by_target
            .entry(String::from("https://a.test/final"))
            .or_default()
            .insert(String::from("clay tablets"));

        // Linked as the original, stored under the redirect target.
        assert_eq!(
            anchors.text_for("https://a.test/asked", "https://a.test/final"),
            "clay tablets"
        );
        assert!(
            anchors
                .text_for("https://b.test/x", "https://b.test/x")
                .is_empty()
        );
    }

    #[test]
    fn the_phrase_cap_is_a_character_count_not_a_byte_count() {
        // A cap in bytes would cut multi-byte text early and inconsistently.
        let phrase = "é".repeat(MAX_PHRASE_CHARS);
        assert_eq!(phrase.chars().count(), MAX_PHRASE_CHARS);
        assert!(
            phrase.len() > MAX_PHRASE_CHARS,
            "the test needs multi-byte text"
        );
    }
}
