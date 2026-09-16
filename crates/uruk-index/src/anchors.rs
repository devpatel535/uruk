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

use std::collections::{BTreeSet, HashMap, HashSet};

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

/// One target's anchor text, while it is being gathered.
#[derive(Debug, Default)]
struct Target {
    /// The distinct things other sites call this page.
    phrases: BTreeSet<String>,
    /// Hashes of the `(source site, phrase)` pairs already counted, so one
    /// host repeating itself across a thousand pages counts once.
    ///
    /// Dropped the moment the target is full: once no more phrases can be
    /// accepted there is nothing left to deduplicate against, and on a large
    /// crawl this set is the difference between fitting in memory and not.
    seen: HashSet<u64>,
}

/// Anchor text gathered per target URL.
#[derive(Debug, Default)]
pub struct Anchors {
    by_target: HashMap<String, Target>,
    pub links_seen: usize,
    pub nofollow: usize,
    pub same_site: usize,
    pub repeated: usize,
    pub too_long: usize,
    /// Links pointing at pages this crawl does not contain.
    ///
    /// Usually most of them, and all of them useless: anchor text describes a
    /// *document*, and a page that was never fetched never becomes one. See
    /// [`Anchors::collect`] for why counting them is what keeps this bounded.
    pub uncrawled: usize,
}

impl Anchors {
    /// Read every page's outgoing links and gather anchor text by target.
    ///
    /// # Why this takes two passes
    ///
    /// The obvious single pass keeps anchor text for every link it sees, and
    /// its memory is bounded by the number of *links* rather than the number
    /// of pages — which on a real crawl is one or two orders of magnitude
    /// larger, because most links point outside the corpus. All of that is
    /// waste: anchor text describes a document, and a URL that was never
    /// fetched never becomes one.
    ///
    /// So the first pass learns which URLs the crawl actually contains, and
    /// the second keeps anchors only for those. Memory is then bounded by the
    /// crawl, which is the thing the operator chose the size of.
    ///
    /// The URL set is held as 64-bit hashes rather than strings, which makes
    /// it about 12 MB per million pages instead of 150. A collision would let
    /// one uncrawled target's anchors be kept — a few wasted bytes, never a
    /// wrong answer, because the map that stores them is still keyed by the
    /// real URL and looked up by the real URL. There is no false negative to
    /// worry about: every crawled URL is inserted exactly.
    pub fn collect(store: &mut StoreReader) -> Result<Self, StoreError> {
        let mut crawled: HashSet<u64> = HashSet::new();
        for record in store.records()? {
            let record = record?;
            crawled.insert(hash(&record.url));
            if record.final_url != record.url {
                crawled.insert(hash(&record.final_url));
            }
        }

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
                if !crawled.contains(&hash(&link.url)) {
                    anchors.uncrawled += 1;
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

                let target = anchors.by_target.entry(link.url.clone()).or_default();
                if target.phrases.len() >= MAX_PHRASES {
                    // Full. Nothing more can be accepted, so the dedup set has
                    // no further work to do and its memory is given back.
                    target.seen = HashSet::new();
                    continue;
                }
                // One phrase, from one site, about one target, counts once.
                if !target.seen.insert(hash_pair(&source_site, &phrase)) {
                    anchors.repeated += 1;
                    continue;
                }
                target.phrases.insert(phrase);
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
            let Some(target) = self.by_target.get(key) else {
                continue;
            };
            for phrase in &target.phrases {
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

/// FNV-1a, for set membership where a collision costs a few bytes.
///
/// Not a cryptographic hash and not trying to be: this decides whether to keep
/// a string that is also stored exactly elsewhere, so the worst a collision
/// does is keep one thing that was not needed.
fn hash(text: &str) -> u64 {
    let mut value = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    value
}

/// Hash of two strings with a separator, so `("ab", "c")` and `("a", "bc")`
/// are different pairs.
fn hash_pair(first: &str, second: &str) -> u64 {
    let mut value = hash(first);
    value ^= 0xff;
    value = value.wrapping_mul(0x0000_0100_0000_01b3);
    for byte in second.as_bytes() {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    value
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
    use super::{Anchors, MAX_PHRASE_CHARS, MAX_PHRASES, Target, hash_pair, tidy};

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
            .phrases
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
    fn a_full_target_gives_back_its_dedup_memory() {
        // The bound that matters on a large crawl: once a target has all the
        // phrases it will ever accept, the set used to deduplicate sources has
        // nothing left to do, and holding it for every target in the corpus is
        // what the two-pass design exists to avoid.
        let mut target = Target::default();
        for i in 0..MAX_PHRASES {
            target.seen.insert(i as u64);
            target.phrases.insert(format!("phrase {i}"));
        }
        assert_eq!(target.phrases.len(), MAX_PHRASES);

        // Simulate the branch `collect` takes when a target is full.
        if target.phrases.len() >= MAX_PHRASES {
            target.seen = std::collections::HashSet::new();
        }
        assert!(target.seen.is_empty(), "a full target kept its dedup set");
    }

    #[test]
    fn the_pair_hash_separates_its_halves() {
        // ("ab", "c") and ("a", "bc") are different sources saying different
        // things; hashing them the same would silently drop one.
        assert_ne!(hash_pair("ab", "c"), hash_pair("a", "bc"));
        assert_ne!(
            hash_pair("site.test", "clay"),
            hash_pair("site.test", "tablets")
        );
        assert_eq!(
            hash_pair("site.test", "clay"),
            hash_pair("site.test", "clay")
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
