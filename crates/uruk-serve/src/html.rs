//! The pages, as HTML.
//!
//! Three rules, from the brief's Section 9 and Section 10, and none of them is
//! a preference:
//!
//! - **No JavaScript.** Not "degrades gracefully without it" — there is none.
//!   A results page is a list of links, and a list of links has not needed a
//!   script since 1995. It follows that the pages work in a text browser.
//! - **No third-party anything.** No web fonts, no analytics, no CDN. The
//!   stylesheet is inlined, which also means one request rather than two.
//! - **Under 20 KB.** A test measures it rather than trusting it.
//!
//! # Escaping
//!
//! A search box that reflects what was typed is the classic place to get
//! cross-site scripting wrong, and every page here reflects the query — in the
//! box, in the `<title>`, and in the result titles and snippets that come from
//! crawled pages we do not control. So [`escape`] is applied to **every**
//! interpolated value without exception, and the tests include the usual
//! attack shapes.

use std::fmt::Write as _;

/// Escape text for inclusion anywhere in HTML, including inside an attribute.
///
/// Both quote characters are escaped, not just double, so the same function is
/// safe in `href="…"` and `href='…'` alike. Being able to use one escaper
/// everywhere is worth more than the handful of bytes a context-specific one
/// would save.
pub fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Percent-encode a query string value.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// The whole stylesheet.
///
/// Deliberately tiny and deliberately inline. System fonts only: a web font is
/// a third-party request, a privacy leak and several hundred kilobytes.
const STYLE: &str = "\
:root{color-scheme:light dark}\
body{margin:0;padding:2rem 1rem;font:16px/1.55 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;\
max-width:44rem;margin-inline:auto;color:#1a1a1a;background:#fefefe}\
@media(prefers-color-scheme:dark){body{color:#e6e6e6;background:#161616}\
a{color:#8ab4f8}.u{color:#7fb77f}.m{color:#9a9a9a}}\
h1{font-size:1.4rem;margin:0 0 1rem}\
form{display:flex;gap:.5rem;margin:0 0 1.5rem}\
input[type=search]{flex:1;padding:.55rem .7rem;font-size:1rem;border:1px solid #bbb;border-radius:4px;\
background:inherit;color:inherit}\
button{padding:.55rem 1rem;font-size:1rem;border:1px solid #bbb;border-radius:4px;\
background:inherit;color:inherit;cursor:pointer}\
ol{list-style:none;padding:0;margin:0}\
li{margin:0 0 1.6rem}\
a{color:#1a4fa0;text-decoration:none}\
a:hover,a:focus{text-decoration:underline}\
.t{font-size:1.1rem}\
.u{color:#2a7a2a;font-size:.85rem;word-break:break-all;margin:.1rem 0 .3rem}\
.s{margin:0}\
.m{color:#666;font-size:.8rem}\
mark{background:#ffe9a8;color:inherit}\
@media(prefers-color-scheme:dark){mark{background:#5a4a12;color:inherit}}\
footer{margin-top:3rem;font-size:.8rem;color:#666}\
";

/// Wrap body content in the page shell.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <title>{}</title>\n\
         <style>{STYLE}</style>\n\
         </head>\n\
         <body>\n{body}\n\
         <footer><a href=\"/\">uruk</a> · \
         <a href=\"/privacy\">privacy</a> · \
         <a href=\"/crawler\">crawler</a></footer>\n\
         </body>\n</html>\n",
        escape(title)
    )
}

/// The search form. Shown on every page that takes a query.
fn form(query: &str) -> String {
    format!(
        // aria-label rather than a hidden <label>: text browsers render
        // `hidden` unevenly and the page then reads "SearchSearch".
        "<form action=\"/search\" method=\"get\" role=\"search\">\
         <input type=\"search\" id=\"q\" name=\"q\" value=\"{}\" autofocus \
         aria-label=\"Search\" autocomplete=\"off\" spellcheck=\"false\">\
         <button type=\"submit\">Search</button>\
         </form>",
        escape(query)
    )
}

/// The homepage: a search box, and nothing else.
pub fn home() -> String {
    page(
        "uruk",
        &format!(
            "<h1>uruk</h1>\n{}\n\
             <p class=\"m\">A search engine that returns links, not answers. \
             Try <code>\"quoted phrases\"</code>, <code>-exclusion</code> or \
             <code>site:example.com</code>.</p>",
            form("")
        ),
    )
}

/// One result, as it appears on the page.
#[derive(Debug, Clone)]
pub struct ResultRow {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// Byte ranges in `snippet` to mark. From the snippet builder.
    pub highlights: Vec<(usize, usize)>,
    /// One line of score breakdown, shown only when explanations are on.
    pub explanation: Option<String>,
}

/// Render `snippet` with `highlights` wrapped in `<mark>`.
///
/// Escaping happens per fragment, because escaping the whole string first
/// would move every byte offset the highlights refer to.
fn marked(snippet: &str, highlights: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(snippet.len() + highlights.len() * 16);
    let mut cursor = 0usize;

    for &(from, to) in highlights {
        // Ignore anything out of order or out of range rather than panicking:
        // these offsets came from another component.
        if from < cursor || to > snippet.len() || from >= to {
            continue;
        }
        if !snippet.is_char_boundary(from) || !snippet.is_char_boundary(to) {
            continue;
        }
        out.push_str(&escape(&snippet[cursor..from]));
        out.push_str("<mark>");
        out.push_str(&escape(&snippet[from..to]));
        out.push_str("</mark>");
        cursor = to;
    }
    out.push_str(&escape(&snippet[cursor..]));
    out
}

/// The results page.
pub fn results(query: &str, hits: &[ResultRow], matched: usize, millis: f64) -> String {
    let mut body = format!("<h1><a href=\"/\">uruk</a></h1>\n{}\n", form(query));

    if hits.is_empty() {
        let _ = write!(
            body,
            "<p>No pages match <strong>{}</strong>.</p>\
             <p class=\"m\">Every word has to appear — that is the default. \
             Try fewer words.</p>",
            escape(query)
        );
        return page(&format!("{query} — uruk"), &body);
    }

    let _ = write!(
        body,
        "<p class=\"m\">{matched} {} in {millis:.0} ms</p>\n<ol>",
        if matched == 1 { "result" } else { "results" }
    );

    for hit in hits {
        let _ = write!(
            body,
            "<li><a class=\"t\" href=\"{href}\" rel=\"noreferrer noopener\">{title}</a>\
             <div class=\"u\">{shown}</div>",
            href = escape(&hit.url),
            title = escape(&hit.title),
            shown = escape(&hit.url),
        );
        if !hit.snippet.is_empty() {
            let _ = write!(
                body,
                "<p class=\"s\">{}</p>",
                marked(&hit.snippet, &hit.highlights)
            );
        }
        if let Some(explanation) = &hit.explanation {
            let _ = write!(body, "<p class=\"m\">{}</p>", escape(explanation));
        }
        body.push_str("</li>");
    }
    body.push_str("</ol>");
    page(&format!("{query} — uruk"), &body)
}

/// What we can and cannot see. Phase 6 of the brief asks for this to be
/// documented honestly, including what we *can* observe.
pub fn privacy() -> String {
    page(
        "Privacy — uruk",
        "<h1>Privacy</h1>\n\
         <p>No cookies are set, by this site or anyone else. There are no \
         accounts. Nothing on these pages is fetched from a third party: no \
         fonts, no analytics, no trackers, no content delivery network. There \
         is no JavaScript.</p>\n\
         <p>Your search is not tied to you. It is not stored against an IP \
         address, a cookie, or any other identifier.</p>\n\
         <p>Following a result sends no referrer, so the site you land on is \
         not told what you searched for.</p>\n\
         <h2>What we can see</h2>\n\
         <p>Being honest about the limits: your browser sends this server your \
         IP address and user-agent in order to reach it at all, as it does to \
         every site. That is true of any web server and we would rather say so \
         than imply otherwise. Whether those are written to a log depends on \
         who is running this instance — which is why it is self-hostable, and \
         why you can run your own.</p>\n\
         <p>Search terms are visible to whoever operates the server while the \
         request is being answered. They are not stored against you.</p>",
    )
}

/// Who our crawler is and how to stop it.
///
/// `RESEARCH.md` §5.1 argues this page has to exist before the first fetch:
/// the user-agent points at it, and in 2026 a crawler that cannot be
/// identified is assumed to be an AI scraper.
pub fn crawler(user_agent: &str) -> String {
    page(
        "Our crawler — uruk",
        &format!(
            "<h1>uruk-crawl</h1>\n\
             <p>We run a web crawler to build a search index. It identifies \
             itself as:</p>\n\
             <pre>{}</pre>\n\
             <h2>How it behaves</h2>\n\
             <ul>\n\
             <li>It reads <code>robots.txt</code> before anything else on a \
             host, and obeys it.</li>\n\
             <li>It honours <code>Crawl-delay</code>, which is not part of the \
             standard and which some crawlers ignore.</li>\n\
             <li>It makes at most one request to a site at a time, and waits \
             several seconds between them.</li>\n\
             <li>It honours <code>noindex</code>, <code>nofollow</code>, \
             <code>noarchive</code>, <code>nosnippet</code> and \
             <code>max-snippet</code>.</li>\n\
             <li>It stores the readable text of a page, never the page itself, \
             and it never trains anything on what it reads.</li>\n\
             </ul>\n\
             <h2>How to stop it</h2>\n\
             <p>Add this to your <code>robots.txt</code>:</p>\n\
             <pre>User-agent: uruk-crawl\nDisallow: /</pre>\n\
             <p>It takes effect on our next visit to your site. If something \
             has gone wrong and you need it stopped sooner, the contact address \
             is on the repository linked in the user-agent above.</p>",
            escape(user_agent)
        ),
    )
}

/// A page that does not exist.
pub fn not_found() -> String {
    page(
        "Not found — uruk",
        "<h1>Not found</h1>\n<p>There is no page here. \
         <a href=\"/\">Search instead</a>.</p>",
    )
}

/// A query string for a search URL, used by tests and by redirects.
pub fn search_path(query: &str) -> String {
    format!("/search?q={}", urlencode(query))
}

#[cfg(test)]
mod tests {
    use super::{
        ResultRow, crawler, escape, home, marked, not_found, privacy, results, search_path,
    };

    fn hit(title: &str, url: &str, snippet: &str) -> ResultRow {
        ResultRow {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
            highlights: Vec::new(),
            explanation: None,
        }
    }

    #[test]
    fn escaping_neutralises_the_usual_attacks() {
        assert_eq!(
            escape("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
        assert_eq!(
            escape("\" onmouseover=\"evil()"),
            "&quot; onmouseover=&quot;evil()"
        );
        assert_eq!(escape("' onload='evil()"), "&#39; onload=&#39;evil()");
        assert_eq!(escape("a & b"), "a &amp; b");
    }

    #[test]
    fn escaping_leaves_ordinary_text_alone() {
        assert_eq!(
            escape("Clay tablets — Uruk, 3200 BC"),
            "Clay tablets — Uruk, 3200 BC"
        );
    }

    #[test]
    fn a_hostile_query_cannot_break_out_of_the_search_box() {
        let attack = "\"><script>alert(document.domain)</script>";
        let html = results(attack, &[], 0, 1.0);
        assert!(!html.contains("<script>"), "script tag survived: {html}");
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_hostile_page_title_cannot_inject() {
        // Result titles come from crawled pages, which are not ours.
        let hostile = hit("<img src=x onerror=alert(1)>", "https://a.test/", "text");
        let html = results("clay", std::slice::from_ref(&hostile), 1, 1.0);
        assert!(!html.contains("<img"), "tag survived: {html}");
    }

    #[test]
    fn a_hostile_url_cannot_break_the_href_attribute() {
        let hostile = hit("Title", "https://a.test/\" onclick=\"evil()", "text");
        let html = results("clay", std::slice::from_ref(&hostile), 1, 1.0);
        assert!(
            !html.contains("onclick=\"evil"),
            "attribute escaped the quotes: {html}"
        );
    }

    #[test]
    fn pages_carry_no_javascript_at_all() {
        let hostile = hit("Title", "https://a.test/", "text");
        for html in [
            home(),
            results("clay", std::slice::from_ref(&hostile), 1, 2.0),
            results("nothing", &[], 0, 1.0),
            privacy(),
            crawler("uruk-crawl/0.1"),
            not_found(),
        ] {
            let lowered = html.to_lowercase();
            assert!(!lowered.contains("<script"), "a script tag: {html}");
            assert!(!lowered.contains("javascript:"), "a javascript: URL");
            assert!(!lowered.contains(" onclick"), "an inline handler");
            assert!(!lowered.contains(" onload"), "an inline handler");
        }
    }

    #[test]
    fn pages_fetch_nothing_from_anywhere_else() {
        // The stylesheet is inlined and there are no fonts, scripts or images.
        for html in [home(), privacy(), crawler("uruk-crawl/0.1"), not_found()] {
            assert!(
                !html.contains("<link rel=\"stylesheet\""),
                "an external stylesheet"
            );
            assert!(!html.contains("<img"), "an image request");
            assert!(!html.contains("//fonts."), "a web font");
            assert!(
                !html.contains("https://"),
                "an absolute external URL: {html}"
            );
        }
    }

    #[test]
    fn results_do_not_leak_the_query_to_the_sites_they_link_to() {
        let one = hit("Title", "https://a.test/page", "text");
        let html = results("clay", std::slice::from_ref(&one), 1, 1.0);
        assert!(
            html.contains("rel=\"noreferrer noopener\""),
            "outbound links leak a referrer"
        );
    }

    #[test]
    fn a_full_results_page_stays_under_the_weight_budget() {
        // The brief's target is 20 KB. Ten results with long titles, URLs and
        // snippets is the worst realistic case.
        let hits: Vec<ResultRow> = (0..10)
            .map(|i| {
                ResultRow {
                    title: format!("A fairly long result title number {i} about clay tablets and the invention of the receipt in Uruk"),
                    url: format!("https://some-quite-long-hostname.test/section/subsection/article-{i}-with-a-slug"),
                    snippet: "…".to_owned() + &"the scribes of Uruk pressed a reed into wet clay to record barley and sheep for the temple storehouse ".repeat(3),
                    highlights: vec![(10, 14)],
                    explanation: Some(format!("score 3.{i}12 (text 2.900 + proximity 0.300 + quality 0.400)")),
                }
            })
            .collect();

        let html = results("clay tablets", &hits, 1_234, 4.2);
        assert!(html.len() < 20_000, "results page is {} bytes", html.len());
        // And the homepage should be very small indeed.
        assert!(home().len() < 4_000, "homepage is {} bytes", home().len());
    }

    #[test]
    fn highlights_are_marked_and_still_escaped() {
        // "bad" spans bytes 0..3 of the snippet.
        let marked = marked("bad <b> word", &[(0, 3)]);
        assert!(marked.starts_with("<mark>bad</mark>"));
        assert!(
            marked.contains("&lt;b&gt;"),
            "the tag was not escaped: {marked}"
        );
    }

    #[test]
    fn out_of_range_highlights_are_ignored_rather_than_panicking() {
        // These offsets come from another component; trusting them blindly
        // would turn an off-by-one there into a crash here.
        assert_eq!(marked("short", &[(0, 999)]), "short");
        assert_eq!(marked("short", &[(4, 2)]), "short");
        // Mid-character offsets in multibyte text.
        assert_eq!(marked("café", &[(3, 4)]), "café");
    }

    #[test]
    fn overlapping_highlights_do_not_duplicate_text() {
        let marked = marked("abcdef", &[(0, 3), (1, 4)]);
        assert_eq!(marked, "<mark>abc</mark>def");
    }

    #[test]
    fn a_result_is_readable_in_a_text_browser() {
        // Lynx has no CSS, so structure has to carry the layout: the URL must
        // be in a block element or it runs onto the end of the title, and the
        // search box must not need a `hidden` label it may not honour.
        let one = hit("A Title", "https://a.test/page", "some text");
        let html = results("clay", std::slice::from_ref(&one), 1, 1.0);

        assert!(
            html.contains("<div class=\"u\">"),
            "the URL is not in a block element"
        );
        assert!(
            !html.contains("hidden>"),
            "a hidden label will render in Lynx"
        );
        assert!(
            html.contains("aria-label=\"Search\""),
            "the search box has no label"
        );
    }

    #[test]
    fn an_empty_result_set_says_so_and_explains_why() {
        let html = results("clay babylon", &[], 0, 1.0);
        assert!(html.contains("No pages match"));
        // AND is the default and users need telling, because it is not what
        // they are used to.
        assert!(html.contains("Every word has to appear"));
    }

    #[test]
    fn the_crawler_page_says_how_to_block_us() {
        let html = crawler("uruk-crawl/0.1 (+https://example.test/crawler)");
        assert!(html.contains("User-agent: uruk-crawl"));
        assert!(html.contains("Disallow: /"));
        assert!(html.contains("robots.txt"));
    }

    #[test]
    fn the_privacy_page_admits_what_we_can_see() {
        // A privacy page that only lists what we do not do is marketing.
        let html = privacy();
        assert!(html.contains("What we can see"));
        assert!(html.contains("IP address"));
    }

    #[test]
    fn search_paths_are_encoded() {
        assert_eq!(search_path("clay tablets"), "/search?q=clay+tablets");
        assert_eq!(search_path("a&b=c"), "/search?q=a%26b%3Dc");
        assert_eq!(search_path("\"quoted\""), "/search?q=%22quoted%22");
    }

    #[test]
    fn every_page_is_valid_enough_to_render_in_a_text_browser() {
        for html in [
            home(),
            privacy(),
            crawler("x"),
            not_found(),
            results("q", &[], 0, 1.0),
        ] {
            assert!(html.starts_with("<!doctype html>"));
            assert!(html.contains("<html lang=\"en\">"));
            assert!(html.trim_end().ends_with("</html>"));
            // Balanced enough that a naive parser copes.
            assert_eq!(html.matches("<body>").count(), 1);
            assert_eq!(html.matches("</body>").count(), 1);
        }
    }
}
