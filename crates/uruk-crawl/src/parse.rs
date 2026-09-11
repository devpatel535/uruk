//! HTML in, clean article text and links out.
//!
//! A fetched page is mostly not the article. It is navigation, a cookie
//! banner, a sidebar of related posts, a footer of legal links, and somewhere
//! in the middle the thing the author actually wrote. Indexing the chrome is
//! how you end up with every page on a site matching "privacy policy" and
//! "skip to content".
//!
//! The approach is the readability family, which independent benchmarks still
//! put at the top for this task (`RESEARCH.md` §4). Two stages:
//!
//! 1. **Cut** the elements that are never article text — scripts, navigation,
//!    footers, forms — and any container whose class or id names it as
//!    boilerplate (`sidebar`, `comments`, `related`, `newsletter`).
//! 2. **Choose** the remaining container with the best text score: many words,
//!    few of them inside links, and not too much markup per word. A block of
//!    prose beats a list of links even when the list is longer.
//!
//! The same pass collects the signals Phase 4 wants as content-quality proxies:
//! text-to-markup ratio, link density and script count.

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{ElementRef, Html, Node, Selector};
use url::Url;

use crate::url::{self as urlnorm, Rejected};

/// Elements whose entire subtree is discarded. None of these ever contain the
/// article, and several of them (`script`, `style`) would otherwise contribute
/// code to the text.
const CUT_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "nav", "header", "footer", "aside", "form",
    "iframe", "svg", "canvas", "button", "select", "textarea", "menu", "dialog", "object",
    "embed", "video", "audio", "map", "picture",
];

/// Class and id tokens that mark a container as furniture. Matched against
/// whole tokens, never as substrings — "commentary" must not be cut because it
/// contains "comment".
const BOILERPLATE_TOKENS: &[&str] = &[
    "nav", "navbar", "navigation", "menu", "sidebar", "side", "footer", "header", "masthead",
    "comment", "comments", "disqus", "related", "recommended", "share", "sharing", "social",
    "advert", "advertisement", "ads", "ad", "promo", "promotion", "sponsor", "sponsored",
    "cookie", "cookies", "consent", "banner", "popup", "modal", "overlay", "newsletter",
    "subscribe", "signup", "breadcrumb", "breadcrumbs", "pagination", "pager", "widget",
    "toolbar", "skip", "screen-reader", "sr-only", "visually-hidden", "copyright", "legal",
];

/// Tags after which a newline is inserted, so that extracted text keeps
/// sentence and paragraph boundaries instead of running words together.
const BLOCK_TAGS: &[&str] = &[
    "p", "div", "section", "article", "main", "li", "tr", "br", "h1", "h2", "h3", "h4", "h5",
    "h6", "blockquote", "pre", "td", "th", "dd", "dt", "figcaption", "hr",
];

fn selector(spec: &str) -> Selector {
    Selector::parse(spec).expect("static selector must parse")
}

static TITLE: LazyLock<Selector> = LazyLock::new(|| selector("title"));
static META: LazyLock<Selector> = LazyLock::new(|| selector("meta"));
static LINK_REL: LazyLock<Selector> = LazyLock::new(|| selector("link[rel]"));
static ANCHOR: LazyLock<Selector> = LazyLock::new(|| selector("a[href]"));
static HEADING: LazyLock<Selector> = LazyLock::new(|| selector("h1, h2, h3"));
static HTML_TAG: LazyLock<Selector> = LazyLock::new(|| selector("html"));
static BODY: LazyLock<Selector> = LazyLock::new(|| selector("body"));
static CANDIDATE: LazyLock<Selector> =
    LazyLock::new(|| selector("article, main, div, section, [role=main]"));
static SCRIPT: LazyLock<Selector> = LazyLock::new(|| selector("script"));
static CUT_SET: LazyLock<HashSet<&'static str>> = LazyLock::new(|| CUT_TAGS.iter().copied().collect());
static BLOCK_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| BLOCK_TAGS.iter().copied().collect());
static BOILERPLATE_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| BOILERPLATE_TOKENS.iter().copied().collect());

/// What a page asks crawlers to do with it.
///
/// These come from `<meta name="robots">` and the `X-Robots-Tag` header. The
/// brief does not mention them; `RESEARCH.md` §5.9 argues they need handling
/// from the first crawl rather than retrofitting at Phase 5, because
/// `noarchive` and `nosnippet` are how a site says "index me but do not quote
/// me" and ignoring that is how a polite crawler becomes a complaint.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per robots directive; a bitfield would hide the correspondence"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directives {
    /// May this page appear in results at all?
    pub index: bool,
    /// May we follow its links?
    pub follow: bool,
    /// May we keep a copy of its text?
    pub archive: bool,
    /// May we show a snippet of it?
    pub snippet: bool,
    /// Longest snippet the page permits, in characters. `Some(0)` means none.
    pub max_snippet: Option<usize>,
}

impl Default for Directives {
    fn default() -> Self {
        Self { index: true, follow: true, archive: true, snippet: true, max_snippet: None }
    }
}

impl Directives {
    /// Apply one comma-separated directive list, from a meta tag or a header.
    pub fn apply(&mut self, content: &str) {
        for raw in content.split(',') {
            let token = raw.trim().to_ascii_lowercase();
            match token.as_str() {
                "noindex" => self.index = false,
                "nofollow" => self.follow = false,
                "noarchive" | "nocache" => self.archive = false,
                "nosnippet" => {
                    self.snippet = false;
                    self.max_snippet = Some(0);
                }
                "none" => {
                    self.index = false;
                    self.follow = false;
                }
                "all" => {
                    self.index = true;
                    self.follow = true;
                }
                _ => {
                    if let Some(value) = token.strip_prefix("max-snippet:")
                        && let Ok(limit) = value.trim().parse::<i64>()
                    {
                        // -1 means "no limit"; anything else caps us.
                        self.max_snippet = if limit < 0 {
                            None
                        } else {
                            Some(usize::try_from(limit).unwrap_or(0))
                        };
                        self.snippet = self.max_snippet != Some(0);
                    }
                }
            }
        }
    }
}

/// A link found on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: Url,
    /// The clickable text. Anchor text is a ranking signal (`RESEARCH.md` §4)
    /// and often describes the target better than the target's own title.
    pub anchor: String,
    /// `rel="nofollow"`, `ugc` or `sponsored`: the author declines to vouch for
    /// this target, so it must not pass authority when we build the link graph.
    pub nofollow: bool,
}

/// Cheap content-quality measures, computed while we are already walking the
/// document. Phase 4 uses these to rank pages that are mostly chrome lower.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quality {
    /// Extracted text length divided by total HTML length.
    pub text_ratio: f64,
    /// Fraction of the article's words that sit inside a link.
    pub link_density: f64,
    /// Number of `<script>` elements. A proxy for how much of the page is ads
    /// and tracking rather than writing.
    pub scripts: usize,
    pub words: usize,
}

/// The useful content of a fetched page.
#[derive(Debug, Clone)]
pub struct Page {
    pub title: String,
    /// Article text with the boilerplate removed.
    pub text: String,
    /// `h1`–`h3` contents, kept separately for field weighting at index time.
    pub headings: Vec<String>,
    pub links: Vec<Link>,
    pub lang: Option<String>,
    /// `<link rel="canonical">`, if the page declares one.
    pub canonical: Option<Url>,
    pub directives: Directives,
    pub quality: Quality,
}

/// Parse a fetched page.
///
/// `base` is the URL it was fetched from, used to resolve relative links.
/// `header_directives` carries anything an `X-Robots-Tag` response header said,
/// which is merged with the page's own meta tags.
pub fn parse(html: &str, base: &Url, header_directives: Directives) -> Page {
    let document = Html::parse_document(html);

    let title = document
        .select(&TITLE)
        .next()
        .map(|element| squeeze(&element.text().collect::<String>()))
        .unwrap_or_default();

    let mut directives = header_directives;
    for meta in document.select(&META) {
        let name = meta.value().attr("name").unwrap_or_default().to_ascii_lowercase();
        // `robots` addresses every crawler; our own product token addresses us
        // specifically and wins where both are present.
        if (name == "robots" || name == "uruk-crawl")
            && let Some(content) = meta.value().attr("content")
        {
            directives.apply(content);
        }
    }

    let lang = document
        .select(&HTML_TAG)
        .next()
        .and_then(|element| element.value().attr("lang"))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());

    let canonical = document
        .select(&LINK_REL)
        .find(|element| {
            element
                .value()
                .attr("rel")
                .is_some_and(|rel| rel.eq_ignore_ascii_case("canonical"))
        })
        .and_then(|element| element.value().attr("href"))
        .and_then(|href| urlnorm::resolve(base, href).ok());

    let headings: Vec<String> = document
        .select(&HEADING)
        .map(|element| squeeze(&element.text().collect::<String>()))
        .filter(|text| !text.is_empty())
        .take(64)
        .collect();

    let links = collect_links(&document, base);
    let scripts = document.select(&SCRIPT).count();

    let (text, link_density) = extract_article(&document);
    let words = text.split_whitespace().count();

    let quality = Quality {
        text_ratio: if html.is_empty() {
            0.0
        } else {
            text.len() as f64 / html.len() as f64
        },
        link_density,
        scripts,
        words,
    };

    Page { title, text, headings, links, lang, canonical, directives, quality }
}

fn collect_links(document: &Html, base: &Url) -> Vec<Link> {
    let mut links = Vec::new();
    let mut seen = HashSet::new();

    for anchor in document.select(&ANCHOR) {
        let Some(href) = anchor.value().attr("href") else { continue };
        let url = match urlnorm::resolve(base, href) {
            Ok(url) => url,
            Err(Rejected::NotWebScheme | Rejected::NoHost | Rejected::TooLong) => continue,
        };
        if !seen.insert(url.to_string()) {
            continue;
        }
        let rel = anchor.value().attr("rel").unwrap_or_default().to_ascii_lowercase();
        links.push(Link {
            url,
            anchor: squeeze(&anchor.text().collect::<String>()),
            nofollow: rel
                .split_whitespace()
                .any(|token| matches!(token, "nofollow" | "ugc" | "sponsored")),
        });
    }
    links
}

/// Does this element's class or id mark it as furniture?
fn is_boilerplate(element: &ElementRef) -> bool {
    let value = element.value();
    let mut attributes = String::new();
    if let Some(class) = value.attr("class") {
        attributes.push_str(class);
        attributes.push(' ');
    }
    if let Some(id) = value.attr("id") {
        attributes.push_str(id);
    }
    if attributes.is_empty() {
        return false;
    }
    // Whole-token matching only: "commentary" must survive, "comment-list" must not.
    attributes
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .any(|token| BOILERPLATE_SET.contains(token.to_ascii_lowercase().as_str()))
}

/// Text of a subtree, skipping cut tags and boilerplate containers.
fn subtree_text(element: ElementRef) -> String {
    let mut out = String::new();
    walk(*element, &mut out);
    squeeze(&out)
}

fn walk(node: ego_tree::NodeRef<'_, Node>, out: &mut String) {
    for child in node.children() {
        match child.value() {
            Node::Text(text) => {
                out.push_str(text);
                out.push(' ');
            }
            Node::Element(element) => {
                let name = element.name();
                if CUT_SET.contains(name) {
                    continue;
                }
                if let Some(as_element) = ElementRef::wrap(child)
                    && is_boilerplate(&as_element)
                {
                    continue;
                }
                walk(child, out);
                if BLOCK_SET.contains(name) {
                    out.push('\n');
                }
            }
            _ => {}
        }
    }
}

/// Words inside `<a>` elements within a subtree. Navigation is mostly link
/// text; prose mostly is not, which is what makes this discriminating.
fn link_words(element: ElementRef) -> usize {
    element
        .select(&ANCHOR)
        .map(|anchor| anchor.text().collect::<String>().split_whitespace().count())
        .sum()
}

/// Pick the container holding the article, and return its text plus the link
/// density of that container.
fn extract_article(document: &Html) -> (String, f64) {
    let mut best: Option<(f64, ElementRef)> = None;

    for candidate in document.select(&CANDIDATE) {
        if is_boilerplate(&candidate) {
            continue;
        }
        let text = subtree_text(candidate);
        let words = text.split_whitespace().count();
        // Too short to be an article; skipping these stops a stray <div> with a
        // headline in it from beating the real body.
        if words < 25 {
            continue;
        }
        let links = link_words(candidate).min(words);
        let density = links as f64 / words as f64;
        let tags = candidate.select(&CANDIDATE).count();

        // Reward words, punish words that are inside links, and punish markup
        // per word. The last term is what stops <body> — which contains every
        // candidate — from always winning on raw length.
        let score = words as f64 * (1.0 - density) - 4.0 * tags as f64;

        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, candidate));
        }
    }

    let chosen = best.map(|(_, element)| element).or_else(|| document.select(&BODY).next());

    let Some(element) = chosen else { return (String::new(), 0.0) };
    let text = subtree_text(element);
    let words = text.split_whitespace().count();
    let density = if words == 0 {
        0.0
    } else {
        link_words(element).min(words) as f64 / words as f64
    };
    (text, density)
}

/// Collapse runs of whitespace, preserving single newlines as paragraph marks.
fn squeeze(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    let mut pending_newline = false;

    for ch in raw.chars() {
        if ch == '\n' || ch == '\r' {
            pending_newline = true;
        } else if ch.is_whitespace() {
            pending_space = true;
        } else {
            if pending_newline && !out.is_empty() {
                out.push('\n');
            } else if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            pending_newline = false;
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Directives, parse};
    use url::Url;

    fn base() -> Url {
        Url::parse("https://a.test/blog/post.html").unwrap()
    }

    /// A page shaped like a real blog post: chrome on all four sides, article
    /// in the middle. Extraction has to find the middle.
    const BLOG_POST: &str = r#"<!doctype html>
<html lang="en-GB">
<head>
  <title>Clay tablets and the invention of the receipt</title>
  <link rel="canonical" href="/blog/clay-tablets">
  <script src="/analytics.js"></script>
</head>
<body>
  <nav class="site-nav"><a href="/">Home</a> <a href="/about">About</a> <a href="/archive">Archive</a></nav>
  <header class="masthead"><a href="/">Some Blog</a> — thoughts on old things</header>
  <div class="cookie-banner">We use cookies. <a href="/privacy">Privacy policy</a></div>
  <div id="wrapper">
    <article class="post">
      <h1>Clay tablets and the invention of the receipt</h1>
      <p>The scribes of Uruk were not writing poetry. They were counting sheep, and
      they needed the count to survive the walk from the pen to the temple.</p>
      <p>What they produced was a technology for making a promise outlive the person
      who made it. A tablet recorded that a quantity of barley had changed hands, and
      it did so in a form that could be checked by someone who had not been present.</p>
      <p>That is the whole idea, and every ledger since is a footnote to it. See also
      the <a href="/blog/cuneiform">notes on cuneiform</a> for the shape of the marks.</p>
    </article>
    <aside class="sidebar">
      <h3>Related posts</h3>
      <a href="/blog/a">Assyrian trade routes</a>
      <a href="/blog/b">The Sumerian king list</a>
      <a href="/blog/c">Barley prices in the third millennium</a>
    </aside>
  </div>
  <div class="comments"><h3>Comments</h3><p>First! Great post, very interesting stuff here.</p></div>
  <footer class="site-footer"><a href="/terms">Terms</a> <a href="/contact">Contact</a> Copyright 2026</footer>
  <script>window.track('pageview');</script>
</body></html>"#;

    #[test]
    fn extracts_the_article_and_drops_the_chrome() {
        let page = parse(BLOG_POST, &base(), Directives::default());

        assert!(page.text.contains("counting sheep"), "article body missing: {:?}", page.text);
        assert!(page.text.contains("footnote to it"));

        for chrome in ["Privacy policy", "Related posts", "Assyrian trade routes", "First!", "Terms"] {
            assert!(!page.text.contains(chrome), "chrome leaked into text: {chrome}");
        }
    }

    #[test]
    fn never_includes_script_contents() {
        let page = parse(BLOG_POST, &base(), Directives::default());
        assert!(!page.text.contains("track"), "script body leaked: {:?}", page.text);
    }

    #[test]
    fn reads_title_language_and_canonical() {
        let page = parse(BLOG_POST, &base(), Directives::default());
        assert_eq!(page.title, "Clay tablets and the invention of the receipt");
        assert_eq!(page.lang.as_deref(), Some("en-gb"));
        assert_eq!(page.canonical.unwrap().as_str(), "https://a.test/blog/clay-tablets");
    }

    #[test]
    fn collects_headings_for_field_weighting() {
        let page = parse(BLOG_POST, &base(), Directives::default());
        assert!(page.headings.iter().any(|h| h.contains("invention of the receipt")));
    }

    #[test]
    fn collects_links_from_the_whole_page_including_chrome() {
        // Links are for the frontier and the link graph, so unlike text they
        // are gathered everywhere, not just from the article.
        let page = parse(BLOG_POST, &base(), Directives::default());
        let targets: Vec<&str> = page.links.iter().map(|l| l.url.as_str()).collect();
        assert!(targets.contains(&"https://a.test/blog/cuneiform"));
        assert!(targets.contains(&"https://a.test/about"));
    }

    #[test]
    fn resolves_relative_links_and_keeps_anchor_text() {
        let page = parse(BLOG_POST, &base(), Directives::default());
        let link = page.links.iter().find(|l| l.url.path() == "/blog/cuneiform").unwrap();
        assert_eq!(link.anchor, "notes on cuneiform");
        assert!(!link.nofollow);
    }

    #[test]
    fn deduplicates_repeated_links() {
        let html = r#"<html><body><p>x</p>
            <a href="/same">one</a><a href="/same">two</a></body></html>"#;
        let page = parse(html, &base(), Directives::default());
        assert_eq!(page.links.len(), 1);
    }

    #[test]
    fn honours_nofollow_and_its_relatives() {
        let html = r#"<html><body>
            <a href="/a" rel="nofollow">a</a>
            <a href="/b" rel="ugc">b</a>
            <a href="/c" rel="sponsored noopener">c</a>
            <a href="/d" rel="noopener">d</a></body></html>"#;
        let page = parse(html, &base(), Directives::default());
        let flag = |path: &str| page.links.iter().find(|l| l.url.path() == path).unwrap().nofollow;
        assert!(flag("/a") && flag("/b") && flag("/c"));
        assert!(!flag("/d"), "noopener is not a nofollow");
    }

    #[test]
    fn skips_non_web_schemes_in_links() {
        let html = r#"<html><body><a href="mailto:x@a.test">mail</a>
            <a href="javascript:void(0)">js</a><a href="/real">real</a></body></html>"#;
        let page = parse(html, &base(), Directives::default());
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0].url.path(), "/real");
    }

    #[test]
    fn reads_meta_robots_directives() {
        let html = r#"<html><head><meta name="robots" content="noindex, nofollow"></head>
            <body><p>text</p></body></html>"#;
        let page = parse(html, &base(), Directives::default());
        assert!(!page.directives.index);
        assert!(!page.directives.follow);
        // Unmentioned directives keep their defaults.
        assert!(page.directives.archive);
    }

    #[test]
    fn reads_noarchive_and_nosnippet() {
        let html = r#"<html><head><meta name="robots" content="noarchive, nosnippet">
            </head><body><p>text</p></body></html>"#;
        let page = parse(html, &base(), Directives::default());
        assert!(!page.directives.archive);
        assert!(!page.directives.snippet);
        assert_eq!(page.directives.max_snippet, Some(0));
    }

    #[test]
    fn reads_max_snippet_including_the_no_limit_form() {
        let mut capped = Directives::default();
        capped.apply("max-snippet:120");
        assert_eq!(capped.max_snippet, Some(120));
        assert!(capped.snippet);

        let mut unlimited = Directives::default();
        unlimited.apply("max-snippet:-1");
        assert_eq!(unlimited.max_snippet, None);
    }

    #[test]
    fn none_means_neither_index_nor_follow() {
        let mut directives = Directives::default();
        directives.apply("none");
        assert!(!directives.index && !directives.follow);
    }

    #[test]
    fn header_directives_are_honoured_and_meta_can_add_to_them() {
        // X-Robots-Tag said noarchive; the page itself adds noindex.
        let mut from_header = Directives::default();
        from_header.apply("noarchive");
        let html = r#"<html><head><meta name="robots" content="noindex"></head>
            <body><p>text</p></body></html>"#;
        let page = parse(html, &base(), from_header);
        assert!(!page.directives.archive, "header directive was lost");
        assert!(!page.directives.index, "meta directive was lost");
    }

    #[test]
    fn our_own_product_token_is_recognised_in_a_meta_tag() {
        let html = r#"<html><head><meta name="uruk-crawl" content="noindex"></head>
            <body><p>t</p></body></html>"#;
        assert!(!parse(html, &base(), Directives::default()).directives.index);
    }

    #[test]
    fn commentary_is_not_mistaken_for_comments() {
        // Whole-token matching: a substring check would delete this article.
        let html = r#"<html><body><div class="commentary">
            <p>A long stretch of genuine writing about the subject at hand, which happens
            to live in a container whose class name merely begins with the same letters
            as the word we filter on, and which must therefore survive extraction
            entirely intact for this test to pass at all.</p></div></body></html>"#;
        let page = parse(html, &base(), Directives::default());
        assert!(page.text.contains("genuine writing"), "got: {:?}", page.text);
    }

    #[test]
    fn falls_back_to_the_body_when_nothing_scores() {
        let html = "<html><body>Just a bare sentence with no container at all.</body></html>";
        let page = parse(html, &base(), Directives::default());
        assert!(page.text.contains("bare sentence"));
    }

    #[test]
    fn empty_and_broken_html_do_not_panic() {
        for html in ["", "<html>", "<<<>>>", "<div><p>unclosed"] {
            let _ = parse(html, &base(), Directives::default());
        }
    }

    #[test]
    fn quality_signals_are_computed() {
        let page = parse(BLOG_POST, &base(), Directives::default());
        assert_eq!(page.quality.scripts, 2);
        assert!(page.quality.words > 50, "words: {}", page.quality.words);
        assert!(page.quality.text_ratio > 0.0 && page.quality.text_ratio < 1.0);
        // The article is prose, so few of its words are inside links.
        assert!(page.quality.link_density < 0.2, "density: {}", page.quality.link_density);
    }

    #[test]
    fn a_link_farm_page_shows_a_high_link_density() {
        use std::fmt::Write as _;
        let mut html = String::from("<html><body><div class='main'>");
        for i in 0..60 {
            let _ = write!(html, "<a href='/p{i}'>cheap discount product number {i}</a> ");
        }
        html.push_str("</div></body></html>");
        let page = parse(&html, &base(), Directives::default());
        assert!(page.quality.link_density > 0.8, "density: {}", page.quality.link_density);
    }

    #[test]
    fn text_keeps_paragraph_boundaries() {
        let page = parse(BLOG_POST, &base(), Directives::default());
        assert!(page.text.contains('\n'), "paragraphs were run together");
        // And words are never glued across tags.
        assert!(!page.text.contains("sheep.They"));
    }
}
