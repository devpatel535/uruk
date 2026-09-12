# Phase 0 — Research

This is the document you asked for before any engine code gets written. It
covers four things: what search engines of that era actually did, which of
their failures we are designing around, where each of our ideas is borrowed
from, and — the part I'd read first — the places where the plan in the brief
does not survive contact with the web as it is in 2026.

I've defined the jargon the first time it appears. Where I disagree with the
brief I've said so plainly and given the reason.

**Contents**

1. [The short version](#1-the-short-version)
2. [What the old architecture actually was](#2-what-the-old-architecture-actually-was)
3. [What went wrong: the post-mortems](#3-what-went-wrong-the-post-mortems)
4. [Where each of our ideas comes from](#4-where-each-of-our-ideas-comes-from)
5. [Where the plan runs into trouble](#5-where-the-plan-runs-into-trouble)
6. [What "small on disk" actually means, in numbers](#6-what-small-on-disk-actually-means-in-numbers)
7. [Technology choices, argued](#7-technology-choices-argued)
8. [Licence, and the limits of what it protects](#8-licence-and-the-limits-of-what-it-protects)
9. [The name: crates and domains](#9-the-name-crates-and-domains)
10. [What I need from you](#10-what-i-need-from-you)
11. [Sources](#11-sources)

---

## 1. The short version

The engineering in this brief is sound. Every component you've described has
been built before, is well documented, and is within reach for one person in
Rust. Nothing in Sections 5 through 9 worries me technically.

Four things do worry me, in descending order:

**First, the corpus is the product, not the ranking.** You will not out-rank
Google. Their ranking is tuned against behavioural data from billions of
sessions, and Section 8 correctly refuses to collect that. What you *can* do,
which they structurally cannot, is refuse to index entire categories of the
web. Google must return something for every query; we don't. Everything good
about this engine will come from what we choose not to crawl. That reframes
"which subject area should the first crawl cover?" from a taste question into
the single most important technical decision in the project.

**Second, getting the pages is now harder than indexing them.** In 2010 a
polite crawler could fetch nearly anything. In 2026 Cloudflare sits in front
of roughly a fifth of the web and has blocked unknown crawlers by default for
new domains since July 2025; proof-of-work walls like Anubis are deployed in
front of GNOME, FFmpeg, Wine, the kernel mailing list archives and Codeberg;
and many `robots.txt` files now allowlist Googlebot and Bingbot and deny
everything else. A brand-new user-agent reads as an AI scraper, because in
2026 most brand-new user-agents are. Budget for losing a real fraction of your
target hosts, and treat crawler identity and reputation as a Phase 1
deliverable, not an afterthought.

**Third, the premise needs one correction.** The brief says the 2008–2014
experience "wasn't lost to a technical limitation, it was lost to business
decisions." That's about two-thirds true. The other third is that the web
itself moved. A great deal of what people search for now lives in places a
crawler cannot reach or is not allowed to: Reddit, Discord, YouTube,
newsletters, paywalled news, app-only communities. Even a perfect 2010 engine
pointed at the 2026 web would return thin results for many queries, because
the pages genuinely aren't there any more. This doesn't sink the project — it
sharpens it. It means the subject area has to be one where the good writing is
still sitting on the open, static, text web.

**Fourth, two specific pieces of the plan are wrong as written**, and I'd
change them before we build: PageRank on a small crawl does close to nothing
useful (§5.3), and per-document AI-content detection will disproportionately
delete exactly the writing this engine exists to surface (§5.6).

Two prior projects should shape how you think about the odds. **Marginalia
Search** is one person, custom crawler and index, running on his own hardware
for around $200 a month and about an hour a week of maintenance — that is the
existence proof that this is possible at a sane scale. **Gigablast** was also
one person, ran a genuinely independent index for over twenty years, and went
offline in April 2023 with no announcement at all. The difference between
those two outcomes appears to be scope discipline. Aim at Marginalia's
posture, not Gigablast's.

---

## 2. What the old architecture actually was

### 2.1 The shape of the thing

Strip away twenty-five years and a search engine is four programs and two
files.

The **crawler** walks the web and writes down what it finds. The **indexer**
turns that pile of text inside out. The **query engine** looks words up and
puts the matches in order. The **front end** draws a box and a list. The two
files are the **document store** (the text you fetched, kept so you can show a
snippet) and the **inverted index** (the lookup structure).

Brin and Page's 1998 paper describes exactly this, and it is still the clearest
description of the architecture. Their Google had a crawler, a "repository" of
compressed pages, a "forward index" of document-to-word, an "inverted index" of
word-to-document, a lexicon of 14 million words, and a link database for
PageRank. Their whole repository was 147 GB. The useful thing about reading it
now is how little the shape has changed.

### 2.2 The inverted index, in plain words

A **forward index** is the natural way round: "document 4 contains the words
*clay*, *tablet*, *Uruk*." Useful for showing a page, useless for searching,
because answering "who contains *tablet*?" means reading every document.

An **inverted index** is the same information flipped: "the word *tablet*
appears in documents 4, 9 and 12." Now the question is one lookup. That flip is
the entire reason search takes milliseconds instead of minutes.

The list hanging off each word is called a **posting list**, and each entry in
it is a **posting**. A posting is at minimum a document ID. Ours will also
carry:

- the **term frequency** — how many times the word appears in that document,
  which BM25 needs;
- the **positions** — where in the document each occurrence sits, which is what
  makes `"quoted phrase search"` and proximity scoring possible.

Positions are expensive. They are usually the largest single thing in the
index — see §6 for what that costs us.

The **term dictionary** (Brin and Page called it the lexicon) maps a word to
where its posting list lives on disk. It's the index of the index.

### 2.3 Segments: the idea worth stealing most

Doug Cutting's Lucene, started in 1999, contributed the structural idea that
everything since has copied. An index is not one file that gets edited. It is a
set of **segments**: self-contained mini-indexes that are written once and
never modified. New documents create a new segment. Deletes are recorded as a
bitmap of "ignore these," not by rewriting anything. In the background, small
segments are merged into bigger ones and the originals are dropped.

This buys three things at once. Writes never block reads, because readers hold
an immutable set of files. Crash recovery is trivial, because a half-written
segment is simply not referenced yet. And compression gets much better, because
you're compressing a whole finished batch at once rather than patching bytes in
place.

Lucene's current on-disk layout is worth studying in detail. Postings are cut
into fixed blocks of 128 document IDs. Within a block the IDs are
**delta-encoded** — store the gaps between sorted IDs rather than the IDs
themselves, because 1,000,003 → 1,000,009 → 1,000,011 becomes 6 → 2, and small
numbers compress far better. Each block is then **bit-packed**: find the largest
value in the block, work out how many bits that needs, and pack every value at
that width. Any tail shorter than 128 falls back to variable-length integers.
Frequencies and positions use PFOR — same idea, but a small percentage of
outliers are stored separately as "exceptions" so one huge value doesn't force
a wide bit width on the whole block.

### 2.4 Ranking: PageRank got the attention, BM25 did the work

**PageRank** models a reader clicking links at random forever, and asks which
pages they'd spend most time on. Mathematically it's the stationary
distribution of a random walk on the link graph, with a damping factor
(conventionally 0.85) for "sometimes they get bored and jump somewhere else."
A link is a vote; votes from pages that themselves receive many votes count
more. **HITS**, the same year, split this into "hubs" (pages that link to good
things) and "authorities" (pages linked to by good hubs), computed per query
rather than once globally.

PageRank got the press. But the workhorse of relevance in every engine of that
era was **BM25** — a probabilistic scoring function from Robertson and
Sparck Jones' work going back to the 1970s and 80s. In plain words it says:

- a document containing your rare words is better than one containing your
  common words (**IDF** — inverse document frequency);
- a document containing your word ten times is better than once, but not ten
  times better; the benefit saturates (this is the `k1` parameter);
- a short document containing your word is better than a long one containing it
  the same number of times, because length dilutes (the `b` parameter).

That's it. It is three ideas and about fifteen lines of code, it has been the
baseline to beat for thirty years, and nothing we invent should be tried until
BM25 is working and measured. The brief is right to insist on this.

**BM25F** (Robertson, Zaragoza and Taylor, 2004) extends it to weighted fields —
a match in the title counts more than one in the body — and does so correctly.
The naive approach of scoring each field separately and adding the scores
breaks BM25's saturation curve; BM25F combines the *frequencies* across fields
first, with per-field weights, then applies saturation once. We should do it
the BM25F way from the start, because doing it the naive way and fixing it
later means re-tuning everything.

---

## 3. What went wrong: the post-mortems

### 3.1 Spam beat PageRank, and it beat it early

PageRank's assumption is that a link is an honest editorial judgement. That
assumption had a market price attached to it, and the market paid.

The attacks, roughly in order of appearance: **keyword stuffing** (repeat the
term, sometimes in white-on-white text); **link farms** (large sets of sites
that link to each other to manufacture PageRank); **doorway pages** (thousands
of near-identical pages each targeting a keyword variant, all funnelling to one
destination); **cloaking** (serve the crawler different content than the human);
**comment spam** (drop links into any form that accepts them, which is what
eventually produced `rel="nofollow"`); and **paid link networks**.

There's a body of academic work modelling this precisely: a "page farm" is the
set of pages contributing most of a target's PageRank score, and finding an
unnaturally dense one is good evidence of manipulation.

What actually worked as a countermeasure was **trust propagation** rather than
better link maths. **TrustRank** (Gyöngyi, Garcia-Molina and Pedersen, 2004)
starts from a small, hand-picked seed set of pages known to be good and
propagates trust outward along links, on the assumption that good pages rarely
link to bad ones. **Anti-TrustRank** does the reverse from a seed set of known
spam. Note what this really is: PageRank's uniform "jump anywhere" step
replaced with "jump back to somewhere we vouch for." It is an admission that
you cannot get authority out of link structure alone — you have to inject
human judgement at the root.

Google's own answers took years and arrived as sledgehammers. **Panda** (Feb
2011) went after thin and shallow content and effectively ended the content-farm
business model, with major farms losing 50–90% of traffic. **Penguin** (Apr
2012) went after manipulative link building. That Google needed thirteen years
and two dedicated algorithms to partially contain this should calibrate our
expectations.

**What this means for us.** We inherit the good half of this for free: our
crawl is seeded by hand, from sources we choose, with a bounded hop distance.
That *is* a TrustRank seed set. It is a genuinely strong position — the thing
Google had to reconstruct statistically, we get by construction. It also
argues, again, that curation is where our quality comes from.

### 3.2 Crawler traps

A trap is any part of a site that generates unbounded URLs. They are not
usually malicious; they're usually just a calendar.

- **Infinite calendars.** A date widget with a "next month" link generates URLs
  until the heat death of the universe.
- **Session IDs in URLs.** Every visit mints a new URL for the same page, so
  the crawler sees infinite distinct pages with identical content.
- **Faceted navigation.** Filter by colour, size, brand, price, sort order —
  the combinations multiply, and an e-commerce category page can generate tens
  of thousands of URLs that are all the same twenty products reordered.
- **Redirect loops and symlink loops.**
- **Parameter explosion** generally: tracking parameters, `?utm_*`, print
  views, and pagination that never terminates.

The standard defences are unglamorous and all necessary: a maximum crawl depth
from the seed; a hard cap on pages per host; URL normalisation that strips
fragments, known tracking parameters and session IDs before deduplication;
detection of repeating path segments (`/a/b/a/b/a/b/`); a cap on the number of
distinct query parameters; and content-level near-duplicate detection as the
backstop for everything the URL rules miss.

That backstop should be **SimHash**. Manku et al. at Google published the
method and the operating parameters: hash each document to a 64-bit
fingerprint built so that similar documents get similar fingerprints, and treat
two documents as near-duplicates if their fingerprints differ in at most 3 bit
positions. They ran this over 8 billion pages. It is cheap, it is one 64-bit
integer per document, and it catches mirrors, syndicated copies and
trap-generated repetition in one mechanism. We should implement it in Phase 1,
not later, because without it a single trap can poison a whole crawl.

### 3.3 Index bloat

Indexes grew faster than the content they described, for reasons that are
mostly still live:

- **Positions dominate.** They are typically the largest component. Storing
  them for every term in every document is the default and it is expensive.
- **Junk terms.** Web text produces enormous vocabularies of garbage:
  base64 fragments, minified identifiers, OCR noise, tracking tokens. Heaps'
  law says vocabulary keeps growing with corpus size and never saturates, and
  on web text most of that growth is noise. A term appearing in exactly one
  document still costs a dictionary entry.
- **Storing the raw page.** Keeping full HTML alongside the index roughly
  doubles or triples storage for something you only need in order to show a
  150-character snippet.
- **Re-indexing the same content.** Without dedupe, the same article on five
  mirrors is five documents, five posting sets, five of everything.

Our defences: keep only the extracted article text, never the HTML; SimHash
dedupe before indexing, not after; a minimum-length and character-class filter
on terms; and block compression on the document store so snippets cost one
block read rather than a whole-file decompression.

### 3.4 Open-source crawlers were slow and awkward to operate

Nutch is the reference and also the cautionary tale. Its design is a batch
loop: a **CrawlDb** — a big on-disk table holding every URL with its fetch
state and score — and rounds of *generate* (pick the top-scoring URLs, capped
per host for politeness), *fetch* (one queue per host), *parse*, and *update
the CrawlDb*. The data model is right and we should copy it. The operational
experience was not: it needed Hadoop to run at any scale, a crawl round was a
batch job you waited on, and tuning it was folklore.

The lesson for a one-person project is that the crawler must be a single
process you can start, stop, inspect and resume without ceremony. The CrawlDb
idea survives; the Hadoop job graph does not.

### 3.5 Where old ranking was genuinely bad

It's worth being honest that not everything about 2010 search was better:

- **Thin content ranked well.** Pages assembled to match a query, with nothing
  in them, routinely beat the article that actually answered it. That's what
  Panda was for.
- **No synonym or morphology handling worth the name.** Searching "running
  shoes" missed "shoes for runners." Stemming was crude and often wrong.
- **Brittle exact-match behaviour.** One misspelling and you got nothing.
- **Query intent was invisible.** "jaguar" is an animal, a car and a guitar
  pedal, and the engine had no way to know which, so it guessed by popularity.

We are choosing to accept most of this. Exact, predictable, boolean-AND
matching is the product. But we should accept it *deliberately* and know which
complaints from users are the cost of the design, and which are bugs.

---

## 4. Where each of our ideas comes from

| What we're doing | Borrowed from | Notes |
|---|---|---|
| Immutable segments, background merge | **Lucene** | The single most important structural idea |
| 128-doc posting blocks, delta + bit-packing, VInt tail | **Lucene** (`Lucene90PostingsFormat`) | Directly copy the block layout |
| PFOR with exceptions for freqs/positions | **Lucene**, from the PForDelta literature | Outliers stored separately so one big value doesn't widen a block |
| Block-max WAND top-k pruning | **Tantivy** | Lucene uses block-max MAXSCORE; either works |
| Compressed document store, decompressed per snippet | **Lucene** stored fields / **Tantivy** (LZ4, Zstd) | Zstd for us |
| CrawlDb: on-disk URL state table; generate/fetch/parse rounds; per-host queues | **Nutch** | Data model yes, Hadoop no |
| Front-coded term dictionary, B+-tree table layout | **Xapian** (glass backend), *Managing Gigabytes* | Xapian stores sorted docid lists as differences with small values in fewer bytes — same family as ours |
| 64-bit SimHash, Hamming distance ≤ 3 | **Manku et al., Google** | Validated at 8 billion pages |
| Cascade of XPath rules → readability fallback for article text | **Trafilatura** | Best mean F1 (0.937) and precision (0.978) in independent benchmarks |
| BM25 as the base scorer | **Robertson & Sparck Jones** | Nothing custom until this is measured |
| BM25F for title/heading/URL/anchor weighting | **Robertson, Zaragoza & Taylor 2004** | Combine frequencies first, saturate once |
| Trust propagation from a hand-picked seed set | **TrustRank** (Gyöngyi et al.) | Our seeding strategy already is this |
| Host-level authority and in-degree over page-level PageRank | **Najork, Zaragoza & Taylor 2007** | They found BM25F + simple in-degree beat BM25F + PageRank |
| Published crawler identity, static IP range, slow refresh cycle | **Marginalia** | Their crawler is identifiable and documented; ours must be too |
| User-steerable result filtering ("optics") | **Stract** | Worth stealing much later; fits "no personalisation" because the user states it explicitly |
| Elias-Fano / partitioned Elias-Fano as a compression candidate | **Ottaviano & Venturini, SIGIR 2014** | Constant-time random access and fast skipping without full decompression |

---

## 5. Where the plan runs into trouble

This is the section the brief asked for and the one I'd argue about.

### 5.1 Crawl access is the binding constraint, not code

The brief treats crawling as a solved engineering problem with a politeness
layer bolted on. In 2026 it is the riskiest part of the project.

What changed:

- **Cloudflare fronts more than 20% of the web** and since 1 July 2025 blocks
  known AI crawlers by default on new domains, at the infrastructure level —
  before a request ever reaches the site's `robots.txt`. Being polite does not
  help you here, because you never get to demonstrate politeness.
- **Proof-of-work walls.** Anubis, released January 2025, puts a SHA-256
  challenge in front of every visitor and is now deployed by GNOME, FFmpeg,
  Wine, the Linux kernel mailing list archives, FreeCAD, UNESCO and Codeberg —
  which is to say, in front of a good deal of exactly the kind of technical
  writing we'd want to index.
- **`robots.txt` allowlisting.** A growing number of sites permit Googlebot and
  Bingbot and deny `*`. This is a structural barrier to entry for new search
  engines and it is getting worse, not better.
- **We look like an AI scraper**, because a new user-agent with no reputation
  fetching a lot of text is, statistically, an AI scraper.

What to do about it, concretely, in Phase 1:

1. Ship a real crawler identity before the first fetch: a stable user-agent
   (`uruk-crawl/0.1 (+https://<our-domain>/crawler)`) and a page at that URL
   explaining who we are, what we index, how to block us, and a working contact
   address. Marginalia does this and it demonstrably helps.
2. Crawl from a stable IP range with correct reverse DNS, and publish the
   range. Do not crawl from a residential connection — it will be null-routed.
3. Obey `robots.txt` (RFC 9309), and also obey `Crawl-delay` even though it is
   not in the standard and Google ignores it. Obey `429` and `503` with
   `Retry-After`. Default to one request per host every few seconds, as the
   brief says.
4. Honour `noarchive`, `nosnippet` and `max-snippet` robots meta directives
   from day one. The brief doesn't mention these and Phase 5 needs them —
   see §5.9.
5. **Expect to lose hosts.** Instrument the crawler to report refusal reasons
   by category from the first run, so we learn the real number early rather
   than discovering it at scale.

I'd put the realistic expectation at losing 10–30% of general target hosts, and
considerably more on commercial and news sites. For a well-chosen niche of
independent technical or academic writing, it could be much better than that —
which is another reason the subject-area choice is load-bearing.

### 5.2 The premise needs one correction

Section 1 says the 2008–2014 experience "wasn't lost to a technical
limitation. It was lost to business decisions." I'd say two-thirds of it was.

The other third: the writing moved. Forum threads became Discord servers.
Personal blogs became newsletters behind an email gate. Q&A became Reddit,
which now sells its archive and blocks crawlers that haven't paid. Reference
writing became YouTube. Trade press went behind paywalls. None of that is
Google's doing, and none of it is recoverable by writing a better crawler.

This matters because it sets the ceiling on how good this can feel. For a query
whose good answers are all in a Discord server, we return nothing useful, and
so does Google — but the user blames us. Pick a subject area where the good
material is still sitting on the open, static, text web, and this problem
mostly evaporates. Pick one where it isn't, and no amount of ranking work will
save it.

### 5.3 PageRank on a small crawl does almost nothing — and there's evidence

This is the clearest "your plan won't work as written" item.

PageRank is defined over a link graph that is *representative*. Its meaning
comes from the whole web voting. On a topical crawl of a few million pages with
a bounded hop distance, the graph is truncated: most in-links to any page come
from outside the crawl and are invisible to us. What PageRank then measures is
roughly "how central is this page within the subgraph we happened to fetch,"
which is largely a function of our seed list and crawl order. We would be
ranking pages by an artefact of our own crawl scheduling and calling it
authority.

There's also direct evidence that even on a full web graph the payoff is
smaller than its reputation: Najork, Zaragoza and Taylor (2007) found that
**BM25F combined with simple in-degree outperformed BM25F combined with either
PageRank or HITS authority scores**. The expensive eigenvector computation lost
to counting links.

So: keep link analysis, drop the specific plan. What I'd build instead, at
Phase 6:

- **Host-level rather than page-level authority.** Collapse the graph to
  domains. A domain-level graph from a topical crawl is far less truncated than
  a page-level one, because most domains we care about are linked from several
  places inside our crawl.
- **Trust propagation from our seeds**, TrustRank-style, rather than uniform
  PageRank. We already hand-pick seeds; use them as the trust root.
- **In-degree as a baseline to beat.** Implement counting first. If PageRank
  doesn't beat it on the judged query set, don't ship PageRank.
- **Keep the weight small**, as the brief already says. This is the signal spam
  attacks hardest, and on our corpus it's also the weakest signal we have.

#### What Phase 6 actually built, and what it measured

> **Updated after building it.** All four bullets above were implemented in
> `uruk-link`. The design survived contact; one thing about it surprised me.

`uruk link` collapses a crawl to a **host** graph and scores every host two
ways. Four kinds of link never become an edge, and the list is more important
than either algorithm:

| Discarded | Why |
|---|---|
| `rel="nofollow"`, `ugc`, `sponsored` | The author explicitly declined to vouch. A crawler that ignores this makes every comment box a ballot. |
| Self-links, by apex domain | A navigation menu is not evidence. `blog.example.com → shop.example.com` is one site talking to itself. |
| Repeats within a page | Fifty links to one host from one page is one vote. |
| Repeats across a host | A host linking to another host is **one edge**, whatever the page count. |

That last row is the whole signal. **In-degree counts distinct source hosts,
not links.** Without it, authority is a function of how many pages a site has,
and the cheapest attack on the entire ranking is to generate pages — which is
precisely the attack that made 2011's web unsearchable.

**Measured on a fixture with a deliberate link farm.** Four hosts: a hub that
links to two single-page archives, and a farm of sixteen pages that all link to
its own "best page" and to the hub. Nineteen pages crawled, sixteen of them the
farm's.

```
  hosts              4
  host-to-host edges 5

  links that did not become edges
    same site                 29

  top hosts by in-degree
    1. archive A    1.000   in-degree 2   pages 1
    2. archive B    1.000   in-degree 2   pages 1
    3. hub          0.631   in-degree 1   pages 1
    4. the farm     0.000   in-degree 0   pages 16
```

The farm holds 84% of the corpus and scores **zero**, because all twenty-nine
of its votes were for itself. The single-page archives score 1.000 because two
other hosts pointed at them.

And the ranking moves accordingly. For the query `clay tablets`:

| | without `uruk link` | with it |
|---|---|---|
| 1 | the farm's stuffed target page | archive A |
| 2 | archive A | archive B |
| 3 | archive B | the hub |
| 4 | the hub | the farm's target page |

Text relevance alone puts the keyword-stuffed page first — correctly, on its own
terms, because it does repeat the query more. Host authority is what moves the
two pages a human would have picked above it. That is the signal doing exactly
the job §5.3 predicted, on a corpus built to be hostile.

**The surprise: the two methods agreed completely.** In-degree and TrustRank
produced a rank correlation of **1.000** on this graph, so `uruk link` says so
out loud:

```
  in-degree vs trustrank: rank correlation 1.000
    they agree; trustrank is not earning its iterations here
```

A four-host fixture is far too small to conclude anything about the web from,
and I am not claiming otherwise. But it makes the operational point concrete:
the expensive method has to *demonstrate* a difference before it is worth
running, and on small graphs there is often no difference to find. This is why
**in-degree is the default**, both are always computed and stored, and the
report prints the correlation on every run. Najork, Zaragoza and Taylor's 2007
result — BM25F plus in-degree beating BM25F plus PageRank — is the prior here,
and nothing so far argues against it.

**Two things worth knowing about the implementation:**

*The iteration cap was wrong, and silently so.* Power iteration contracts by a
factor of the damping each step, so reaching a residual of 1e-9 at damping 0.85
needs about 128 iterations. The cap was 100. The scores it produced were close
enough to look correct while `converged` was never true. It was caught only
because a test asserted convergence rather than assuming it; the iteration
count is now derived from the damping rather than guessed, and a test pins the
relationship across four damping values. This is the second time in this
project a wrong constant produced plausible output instead of an error, which
is a pattern worth naming: **numbers that are nearly right are harder to find
than numbers that are absent.**

*Same-site detection is an approximation with a known bias.* Without a Public
Suffix List, "same site" is "the last two labels match". That is wrong for
`example.co.uk` (two unrelated British sites look like one) and for
`*.github.io` (two unrelated projects look like one). Both errors **discard
votes that should have counted**; neither *creates* a vote. The asymmetry is
deliberate — an undercounting signal is weak, an overcounting one is an attack
surface — and there is a test pinning the known-wrong cases so that adopting a
real suffix list has a failing test to flip. Shipping the list is a licensing
and update-cadence decision, not a code change.

**Still not done, and deliberately:** anchor text as an index field. The link
graph now makes it available — anchors are stored per edge — but adding a
fifth field changes the segment format again, and §5.4's argument stands: it
should be added with a judged query set in place to measure whether it helps,
not before.

### 5.4 No click data means curation is the product

Section 8 rules out click-through rate, dwell time and personalisation. I agree
with the decision and I want to be explicit about the bill.

Behavioural data is the single strongest relevance signal anyone has. Refusing
it is a real handicap, not a free ethical win. Our substitutes are:

1. **A curated corpus.** Covered above. This is the main one.
2. **A judged query set** — a fixed list of test queries with hand-marked good
   results, scored with a standard metric (nDCG), so a ranking change can be
   measured instead of argued about.

The brief puts the judged query set in Phase 7. **I'd move it to Phase 4**, to
be built at the same time as the first BM25 implementation. You cannot tune a
ranker without one, and every hour spent tuning by eye before it exists is an
hour spent guessing. It doesn't need to be big — 50 queries with 10 judged
results each is enough to catch regressions, and it can grow.

### 5.5 "Decompressed per query, memory released when the query finishes"

The rule in Section 7 is directionally right and, taken literally, would make
us slower for no benefit.

The right design is to **memory-map** the compressed segment files and let the
operating system's page cache hold whichever compressed blocks are hot. You
don't "release" the page cache, and you shouldn't want to — it's shared,
reclaimable under pressure, and it's what makes the second query fast. Forcing
it out after every query would mean re-reading from disk constantly, which
shows up directly in the latency target.

What I think you actually want, and what I'd guarantee instead:

- the index is **never** fully decompressed, and never wholly resident;
- only the blocks a query touches are decoded;
- decoded data lives in a **per-query arena that is dropped when the query
  returns**, so the process's own heap returns to baseline between queries;
- any cache we add ourselves is explicitly bounded and configurable;
- the OS page cache is allowed to keep compressed blocks, and we measure both
  cold and warm latency so we're honest about which number we're quoting.

That gives you the small-footprint property you're after, with the privacy
property unchanged, without fighting the kernel.

### 5.6 Detecting AI-generated content: the per-document approach will backfire

Section 5 asks us to detect and downrank mass-produced AI content, and asks me
to be honest about the limits. Here is the honest answer: **do not run a
per-document AI-text classifier.** It will actively harm this project.

The evidence is unambiguous. OpenAI withdrew its own AI-text classifier after
it correctly identified only 26% of AI-written text while falsely flagging 9%
of human writing. Worse, a Stanford study found GPT detectors are badly biased
against non-native English writers: they flagged **61%** of TOEFL essays
written by humans as AI-generated, and at least one detector flagged 97.8% of
them. Rewriting those essays with more native-sounding vocabulary cut the false
positive rate from 61% to 12% — meaning the detectors are substantially
measuring "does this sound like a fluent native speaker," not "was this
machine-written."

Deploy that on a search index whose entire purpose is surfacing independent
writing, and we will systematically demote non-native English writers, plain
technical prose, and anyone whose style is unadorned. That is the opposite of
what this engine is for, and we would never see it happening.

What works instead is **site-level and economic signals**, because
mass-produced content is a business model and business models leave traces:

- publication cadence — 40 posts a day on a site with no masthead;
- template uniformity — near-identical DOM structure and article length across
  hundreds of pages (our SimHash machinery already gives us most of this);
- affiliate link density and tracker count, which the brief already lists under
  content-quality proxies;
- domain age against page count — a six-month-old domain with 50,000 articles;
- absence of any named author, contact page, or about page;
- doorway patterns — many near-identical pages differing only in a keyword.

And most effective of all: **a hand-maintained blocklist at the domain level**,
which is cheap, transparent, auditable, and correctable when we get it wrong.

The honest framing is that this is a curation problem wearing a classification
problem's clothes. Treat it as curation. Every signal above should be evidence
presented to a human for a domain-level decision, not an automatic per-page
penalty.

### 5.7 Stopwords: keep them

Section 6 says decide deliberately, so: **keep them, index them, don't drop
them.**

Dropping common words ("the", "of", "to") was a 1990s disk-space measure. It
breaks phrase search — `"to be or not to be"` becomes empty — and the brief
explicitly wants quoted phrase search to actually work. The modern answer to
their cost is not deletion but **block-max WAND**: store a maximum possible
score per posting block, and skip whole blocks that cannot enter the top ten.
Common terms stop being expensive without being lost.

### 5.8 Stemming: index the raw word, expand at query time

Related decision. **Stemming** means reducing words to a root form so that
"running", "runs" and "ran" all match. Doing it at index time is lossy and
irreversible: once you've stored `run`, you cannot answer an exact-match query
for `running`, and exact, predictable matching is a selling point here.

Recommendation: store the lowercased raw token as the primary index. If recall
turns out to be too low against the judged query set, add a *separate* stemmed
field later and give it a lower BM25F weight, so exact matches still win. That
way the decision is reversible and measurable, which the index-time version
isn't.

### 5.9 Snippets have a legal dimension the brief doesn't mention

Showing a short extract from someone else's page is normal and broadly
defensible, but two things deserve a decision now rather than at Phase 5:

- The EU's press publishers' right (DSM Directive Article 15) gives news
  publishers rights over snippets of their content. Keep snippets short, and
  keep a per-domain switch to reduce or suppress them.
- Honour `noarchive`, `nosnippet`, `max-snippet:N` and `noindex` robots meta
  tags and `X-Robots-Tag` headers. These are how a site says "index me but
  don't quote me," and ignoring them is the fastest way to turn a polite
  crawler into a complaint.

Both are cheap if designed in at Phase 1 (record the directives with the
document) and annoying to retrofit.

### 5.10 Sub-200ms is not where the difficulty is

For calibration: at one million documents on a normal desktop, a two-term AND
query scored with BM25 and pruned with block-max WAND is single-digit
milliseconds of scoring work. The 200ms budget has roughly an order of
magnitude of headroom.

Where the time will actually go is snippet generation — decompressing ten
document-store blocks to find and highlight the matching sentence — and the
cold-cache case, the first query after the process starts. Design the
document store for cheap random block access, and quote cold and warm latency
separately.

### 5.11 A bootstrap worth considering

Phases 2–4 (indexer, compression, ranking) are blocked on Phase 1 (crawler)
only because they need documents. **Common Crawl** publishes monthly crawls in
WARC format with a columnar URL index, which would let us develop and benchmark
the indexer, the compression schemes and the ranker against real web text
*while* the crawler is still being built and while we're still learning what
fraction of hosts will talk to us.

This doesn't replace our own crawler — independence is the point of the project
and Common Crawl is broad rather than deep. But it decouples the two riskiest
workstreams, and it means a compression benchmark on day one instead of week
six. Worth a decision; I'd take it.

---

## 6. What "small on disk" actually means, in numbers

> **Updated twice after building it.** This section originally contained an
> estimate. Then the indexer existed and the estimate was replaced with a
> measurement, which came in a third worse. Then Phase 5 rewrote the format and
> the measurement moved again — back to roughly where the estimate had been.
> All three numbers are kept below, in order, because the sequence is the
> interesting part.

### What was estimated

Assuming ~6 KB of clean text per page, ~1,000 tokens and ~400 distinct terms,
I projected postings at 1.0–1.5 bytes each, positions at ~1–1.5 bytes each, and
a total of **~3.5 GB per million pages**, with the index at **25–37%** of the
extracted text once positions were included.

Reading this back after Phase 5: the estimate was a reasonable account of what
the data costs when it is packed well. What it did not model was the format —
and the first format I wrote spent 40% of the index on bookkeeping the estimate
never imagined anyone would pay for. The estimate was not wrong about the
information. It was wrong to assume the implementation would be tight, which is
a different mistake and a more useful one to notice.

### What was measured

`cargo run --release --example index_size -p uruk-index -- 20000` builds a
synthetic corpus with the statistics real prose has — a Zipf-distributed
vocabulary, which is what decides how well delta encoding does — and indexes
it. On 20,000 documents averaging 5.8 KB of text, the **first** version of the
index came to:

| Component | Size | % of extracted text |
|---|---|---|
| Extracted text | 116.6 MB | 100% |
| Crawl store (Zstd, block-compressed) | 41.4 MB | 35.5% |
| **Index total** | **57.0 MB** | **48.9%** |
|   postings | 56.2 MB | 48.2% |
|   term dictionary (front-coded) | 0.5 MB | 0.4% |
|   host + document tables | 0.25 MB | 0.2% |

**5.72 bytes per posting**, and **5.2 KB per page** for store and index
together — which extrapolated to roughly **4.8 GB per million pages**, about a
third worse than estimated. At 100,000 documents the figures were unchanged:
5.72 bytes per posting, 48.7% of text, 5,144 bytes per page.

That is the number Phase 5 set out to beat. The rest of this section is what
beating it took, which was not what I expected.

### Why the estimate was wrong

Two things I did not cost:

- **Per-posting overhead.** Every posting carried a field bitmask and at least
  one count, which is two bytes before any document id or position is written.
  At roughly 515 postings per document that is over a kilobyte per page spent
  on bookkeeping.
- **Positions cost two bytes, not one.** Body positions run to ~800, and
  variable-byte encoding needs a second byte above 127. I had assumed roughly
  one byte each.

One byte came back before Phase 5 even started: the encoder used to store the
number of positions in each posting, which is exactly the sum of the field
counts sitting next to it. Removing it took the index from 57.3% to 48.9% of
the text — a good early illustration of how much a single byte per posting is
worth at this scale, and a hint about where the rest of the fat was.

### Phase 5, part one: the codecs, and a surprise

`cargo run --release --example codec_bench -p uruk-index` implements
variable-byte, Simple-9, frame-of-reference bit-packing and `PForDelta`, and
measures size and decode speed on gap distributions with the shape real
posting lists have. The brief asked for the trade-off rather than a
conclusion, so here it is.

**Document-id gaps** (4.4 million values, Zipf-weighted posting lists):

| codec | bits/value | decode Mvals/s | size vs varint |
|---|---|---|---|
| variable-byte | 11.26 | 195 | 100% |
| Simple-9 | 10.36 | 261 | 92% |
| bit-packed (FOR) | 9.62 | 268 | **85%** |
| `PForDelta` | 9.37 | 253 | **83%** |

**Position gaps within documents**, measured the way the index stored them at
the time — one short list per posting:

| codec | bits/value | decode Mvals/s | size vs varint |
|---|---|---|---|
| variable-byte | 13.68 | 50 | 100% |
| Simple-9 | 27.31 | 49 | **200%** |
| bit-packed (FOR) | 13.60 | 47 | 99% |
| `PForDelta` | 13.59 | 46 | 99% |

Four things worth taking from that, two of which I did not expect.

**Block codecs are faster, not just smaller.** On long lists they decode about
35% quicker than variable-byte, because there is no branch per value. The
usual framing of compression as a size-for-speed trade is backwards here.

**`PForDelta`'s patching earns much less than its reputation suggests** — 83%
against bit-packing's 85%. Patching exists to stop one outlier widening a
whole block, and it does: a synthetic block of small gaps with one large one
is less than half the size under `PForDelta`, and there is a test for exactly
that. But gaps drawn from a geometric distribution are *all* variable, so
there is no tidy 10% of outliers to patch — the width has to rise for the bulk
of the block regardless.

**Simple-9 is actively harmful on short lists.** A list of one value costs a
whole 32-bit word. Most terms appear once in most documents, so most position
lists were one or two values long, and Simple-9 doubled them.

**The largest finding was that the codec is not the lever.** Block schemes
gave essentially nothing on positions — not because they encode badly, but
because the lists were too short for a block ever to fill, so everything fell
through to the variable-byte tail. Switching every codec to the best available
would have taken the index from 49% of text to around 44%. Worth having, and
nowhere near enough.

So the benchmark's real output was not a codec ranking. It was a diagnosis:
**the format was wrong, and no codec could fix it from inside.**

### Phase 5, part two: the restructuring, which is where the index got small

The old layout was one self-describing record per document, interleaved:

```text
docgap, field mask, count per field, position gap, position gap, ...
docgap, field mask, count per field, position gap, ...
```

Two faults, both now obvious in hindsight:

1. **Nothing similar sat next to anything similar.** A block codec wants a run
   of values drawn from the same distribution. This layout gave it a document
   gap, then a mask, then a count, then two positions — runs of one. Blocks
   never filled, so every value fell through to the variable-byte tail.
2. **Every posting paid fixed bytes for facts that were already known.** The
   mask and the body's count were written on all 515 postings per page, when
   the overwhelming majority of postings touch the body and nothing else.

The index now writes four streams per term instead:

```text
varint  document count
stream  document-id gaps          (PForDelta blocks, varint tail)
stream  (frequency << 1) | flag   (PForDelta blocks, varint tail)
bytes   field detail, only for the postings whose flag is set
stream  position gaps             (PForDelta blocks, varint tail)
```

Positions are concatenated across every document in the list, resetting the
delta at each document boundary so the gaps stay small — this is what Lucene's
separate positions file is for, and it is what finally lets the blocks fill.
The field mask is written only for the minority of postings touching something
other than the body, and two numbers are dropped entirely because they are
recoverable: the count of positions (it is the frequency) and the body's own
count (it is the frequency minus the other fields').

**Measured, on the same corpora:**

| | before | after | change |
|---|---|---|---|
| bytes per posting, 20k docs | 5.72 | **3.55** | −38% |
| bytes per posting, 100k docs | 5.72 | **3.41** | −40% |
| index as % of text, 100k docs | 48.7% | **29.2%** | −40% |
| store + index per page, 100k docs | 5,144 B | **3,951 B** | −23% |
| extrapolated to 1M pages | 4.8 GB | **3.7 GB** | −23% |

I predicted about 3.7 bytes per posting before making the change. It came in
at 3.41, so the prediction was right in direction and slightly pessimistic in
size — the first honest prediction in this section, after the estimate at the
top of it was wrong by a third.

**What this means in general**, and it is the most transferable thing in this
document: *the data layout dominated the entropy coder by roughly an order of
magnitude.* The best codec swap available was worth 5 percentage points of
index size. Rearranging the same values into streams the codec could actually
work on was worth 20. The literature spends most of its pages on the coders;
the wins were in the layout.

The codec work was not wasted — `PForDelta` is what every stream now uses, and
the benchmark is what identified the layout as the problem. But the honest
summary of Phase 5 is: *benchmark the codecs to find out that the codecs are
not the problem.*

### Scaling, and the brief's target

Against the brief's hope of 15–25% of raw text, the honest position is:

- a **positionless** index would beat the target, but would give up phrase
  search and proximity, which the brief explicitly wants;
- the index we actually want is at **29% of text with positions**, down from
  49%, and I do not see another 40% available without giving something up.
  Getting under 25% would mean dropping positions for rare terms, or dropping
  the position stream for the body of very long documents — both of which
  trade a correctness property for bytes, and neither of which I would take
  without a judged query set to measure the damage.

I restated the goal as a number that can be checked on every build: **under
4 KB on disk per indexed page, everything included.** It was 5.2 KB. It is now
**3,951 bytes**, so that target is met, with the crawl store — not the index —
now the larger half.

| Corpus | Store + index today | Fits where |
|---|---|---|
| 100,000 pages | ~0.4 GB | Anywhere. Ships as a download. |
| 1 million pages | ~3.7 GB | A laptop. Offline personal search is practical. |
| 10 million pages | ~37 GB | A desktop or a cheap VPS with a decent disk. |
| 100 million pages | ~370 GB | A dedicated machine. Shippable to users: no. |
| 1 billion pages | ~3.7 TB | A small rack, and a different project. |

That still answers your fourth Section 15 question the same way: **a shippable
offline index is realistic up to about 1–10 million pages and stops being
realistic above that.** The corpus cap has to be chosen before the index format
is finalised, because a shippable index wants a single-file format with
different trade-offs than a server-side one.

**Crawl time**, for calibration: at one request per host every 3 seconds but
fetching many hosts in parallel, a single machine comfortably sustains 50–100
pages/second. One million pages is about 3–6 hours of wall-clock fetching; ten
million is a couple of days. Bandwidth, not politeness, is the limit, and 1M
pages is roughly 50 GB downloaded. This is all very tractable.

**Indexing time**, measured: 20,000 documents in 20 seconds and 100,000 in 96
seconds, single-threaded — so it scales linearly at about 1,040 pages a second,
and a million pages is about sixteen minutes. Indexing is not the bottleneck
and does not need to be parallel yet.

---

## 7. Technology choices, argued

**Rust for crawler, indexer and server: agreed**, and not only for speed. The
thing Rust really buys here is that memory discipline is checked rather than
intended — which is exactly the property the small-footprint goal needs, and
the property that's hardest to maintain by hand in a long-running crawler.

**Tantivy: both, in sequence.** This is the question the brief asks directly.

Tantivy is a Lucene-inspired index library in Rust, MIT-licensed (so it can be
a dependency of an AGPL work without friction), with segments, BM25,
positions, phrase queries, block-max WAND, and a compressed document store.
**Stract** — an independent, open-source web search engine in Rust with its own
crawler, funded by NLnet — is built on it, which is an existence proof that the
combination works for exactly our use case.

Against that: Sections 7 and 8 of the brief are the project's actual
differentiators. Benchmarking compression schemes against each other, and
producing a per-signal score breakdown for every result, are both things you
do to *your own* index, not inside someone else's.

So, sequenced:

1. **Phases 2–4 on Tantivy**, behind a narrow `Index` trait of our own design
   (index a document; look up a term; iterate postings; score). This gets an
   end-to-end search working in weeks rather than months, which is what "show
   me small working things early" asks for, and it gets real queries in front
   of you while the crawler is still maturing.
2. **Phase 5 implements `.uruk` segments behind the same trait**, with Tantivy
   retained as the benchmark baseline — so every compression scheme we try has
   a credible, well-optimised opponent to be measured against, which is exactly
   the trade-off table the brief asks for.

The cost is one trait boundary designed early. The benefit is that the risky,
interesting work happens against a working system with real data instead of on
a blank page.

**Python for ranking experiments: agreed, with one constraint.** The evaluation
harness should call the Rust binary and score its output, never reimplement
scoring in Python. Two implementations of a scorer diverge, silently, and then
you are tuning one engine and shipping another.

**Frontier storage: redb, not RocksDB.** The brief permits an embedded KV store
if justified. RocksDB in Rust means a large C++ build dependency, slow compiles
and a tuning surface built for workloads far larger than ours. **redb** is a
pure-Rust embedded key-value store with ACID transactions and no C++
toolchain — the whole crawl state stays a single file you can copy, and the
build stays fast. At the scale we're discussing (tens of millions of URLs) it's
comfortably sufficient. If we ever outgrow it the CrawlDb is a well-defined
component behind an interface and can be swapped.

**Front end: server-rendered HTML from the Rust binary: agreed.** For the HTTP
layer I'd use `axum` — it's the mainstream choice, it's a thin layer over
`hyper`, and it adds no client-side anything. A 20 KB page budget with no
JavaScript, no web fonts and no third-party requests is very comfortable; the
whole results page should come in under 5 KB.

---

## 8. Licence, and the limits of what it protects

**Recommendation: AGPL-3.0-or-later.** The brief asked me to argue for it, and
I do — but with two limits stated plainly, because the argument as usually made
claims more than the licence delivers.

**The case for.** Ordinary GPL has a gap: it triggers on *distribution*, and
running software as a web service isn't distribution. Someone can take GPL
code, modify it, run it as a hosted product and never publish a line. The AGPL
closes exactly that gap — if you let people use a modified version over a
network, you must offer them its source. For a search engine, which is only
ever experienced as a network service, this is the whole ballgame. It is also
the licence Marginalia Search uses, and Mastodon and Nextcloud use it for the
same reason.

`-or-later` rather than `-only`: it lets the project move to a future AGPL
version without tracking down every contributor.

**Limit one: the AGPL does not prohibit ads.** This is the important
correction. It compels source disclosure, nothing more. A company can fork
Uruk, publish their modifications exactly as required, and run an
ad-supported, tracking, AI-summarising version — fully compliant. What the
AGPL prevents is a *closed* fork, not an *ugly* one. The no-ads promise is kept
by us and by the project's trademark and name policy, not by the copyright
licence. Worth being clear-eyed about, since Section 0 frames AGPL as
preventing "a closed, ad-supported fork" — it prevents the closed half only.

**Limit two: adoption friction.** Some large companies ban AGPL code outright
(Google's internal policy is the well-known example). This costs us essentially
nothing for the engine itself — we aren't courting corporate adoption — but it
would matter for any piece we'd want others to build on. If the compression
codec turns out to be good work that the wider Rust ecosystem could use, the
right move is to split `uruk-codec` into its own crate under MIT/Apache-2.0
while everything else stays AGPL. That's a decision for Phase 5, not now.

**A third question the licence doesn't answer at all: the data.** The AGPL
covers our source code. It says nothing about the crawled corpus or the built
index, and we don't own the crawled content — we hold a copy of other people's
writing. If we ever distribute an index (which your fourth Section 15 question
contemplates), that needs its own decision, and it has real legal texture:
snippet rights, `noarchive` directives, takedown handling, and what happens
when a page is deleted but our index still has it. Flagging it now; it doesn't
block anything today.

---

## 9. The name: crates and domains

Checked on 2026-09-11, as asked, before either name is baked into anything
published.

**crates.io — all clear.** `uruk`, `uruk-crawl`, `uruk-index`, `uruk-query` and
`uruk-serve` are all unregistered. My advice is **don't reserve them yet**:
crates.io policy discourages name-squatting, publishing an empty placeholder is
poor manners, and the risk of someone else taking an obscure Sumerian city name
in the next few months is very low.

**Domains — the obvious ones are gone.** The egress policy on this machine
blocks WHOIS and RDAP, so I checked DNS instead. A domain that resolves is
definitely registered; a domain that doesn't resolve may still be registered
but parked without DNS, so the second group needs confirming at a registrar.

*Registered (resolves):* `uruk.com`, `uruk.org`, `uruk.net`, `uruk.io`,
`uruk.xyz`, `uruk.info`, `uruk.co`, `uruk.site`, `uruk.systems`, `uruk.cloud`,
`uruk.app`. Note that `uruk.net` and `uruk.xyz` both point at a well-known
registrar parking address — registered, unused, probably for sale at a price.

*No DNS, so possibly available — verify at a registrar:* `uruk.dev`,
`uruk.sh`, `uruk.page`, `uruk.tools`, `uruk.wiki`, `uruk.software`,
`uruk.engineering`, `uruksearch.com`, `uruk-search.com`, `geturuk.com`,
`urukhq.com`.

*Not possible:* `uruk.search`. `.search` is a closed brand TLD held by Google's
registry subsidiary and is not available for public registration.

**My recommendation: `uruk.dev`**, if it's actually free. It's short, it's
honest about what the project is, and `.dev` is HSTS-preloaded so it is
HTTPS-only by construction — a small but real alignment with the privacy
principles. `uruksearch.com` is the safe fallback. Either way, check at a
registrar before the crawler ships, because the user-agent string will contain
the domain and changing it later means changing the identity every site
operator has already seen.

---

## 10. What I need from you

Per your Section 3, one at a time — so here is the first, and the rest are
listed only so you can see where this is going.

**The question I need answered to start Phase 1:**

> **Which subject area should the first crawl cover?**

I'm asking with more weight than the brief gave it, for the reason in §5.2: the
answer determines how much of the good material is still reachable on the open
web, and therefore how good this can possibly feel. A subject where the
expertise still lives on static, text-heavy, independently-hosted pages will
make everything downstream easier. A subject where it has migrated to Discord,
YouTube and newsletters will make the engine feel broken no matter how good the
ranking is.

**You've asked to measure first, so the instrument exists**:
[`tools/crawlability-probe/`](tools/crawlability-probe/). It takes 125
hand-picked candidate domains across three areas — programming and technical,
science and academic, practical and hobbyist — and reports, per area, what
fraction are crawlable (`robots.txt` posture, Cloudflare, proof-of-work
walls), what fraction serve real text without JavaScript, and how densely they
link to one another. Two polite requests per domain, `robots.txt` obeyed,
standard library only.

The candidate lists deliberately include sites I expect to fail — big
publishers, `.gov` domains, and several well-known SEO content farms in the
practical list — because a sample that excludes them would make every area look
healthier than it is.

It has not been run yet: the environment this was written in has restricted
outbound HTTPS, and behind such a proxy every domain scores as blocked. It
needs one run from a normal connection. See the tool's README.

**Then, in order, and only after that one is settled:** the launch scale
(thousands vs. millions — §6 shows it changes the index format); where it runs;
and whether the index must be shippable for offline search (which caps the
corpus, per §6).

**And three smaller things I've recommended changing, which I'd like a yes or
no on when you've read the above:** move the judged query set from Phase 7 to
Phase 4 (§5.4); build Phases 2–4 on Tantivy behind our own trait, then replace
it in Phase 5 (§7); and drop per-document AI detection in favour of site-level
signals plus a hand-maintained blocklist (§5.6).

---

## 11. Sources

**Papers and reference texts**

- Brin & Page, *The Anatomy of a Large-Scale Hypertextual Web Search Engine*, 1998
- Robertson & Zaragoza, *The Probabilistic Relevance Framework: BM25 and Beyond*, 2009 — [ACM](https://dl.acm.org/doi/abs/10.1561/1500000019)
- Robertson, Zaragoza & Taylor, *Simple BM25 Extension to Multiple Weighted Fields* (BM25F), 2004
- Najork, Zaragoza & Taylor, *HITS on the Web: How does it Compare?*, 2007 — the in-degree vs. PageRank result
- Manku, Jain & Das Sarma, *Detecting Near-Duplicates for Web Crawling*, 2007 — [PDF](https://research.google.com/pubs/archive/33026.pdf)
- Gyöngyi, Garcia-Molina & Pedersen, *Combating Web Spam with TrustRank*, 2004
- Ottaviano & Venturini, *Partitioned Elias-Fano Indexes*, SIGIR 2014 — [PDF](https://homepages.dcc.ufmg.br/~mirella/DCC851/Exemplos%20Artigos/ottaviano-SIGIR2014-bestpaper.pdf)
- Pibiri & Venturini, *Techniques for Inverted Index Compression* — [arXiv](https://arxiv.org/pdf/1908.10598)
- Zhou & Pei, *Link Spam Target Detection Using Page Farms*, TKDD 2009 — [PDF](https://www.cs.sfu.ca/~jpei/publications/LinkSpamFarm-TKDD09.pdf)
- Witten, Moffat & Bell, *Managing Gigabytes*; Manning, Raghavan & Schütze, *Introduction to Information Retrieval*
- Liang et al., *GPT detectors are biased against non-native English writers*, 2023 — [arXiv](https://arxiv.org/pdf/2304.02819) · [ScienceDirect](https://www.sciencedirect.com/science/article/pii/S2666389923001307)

**Implementations**

- [Apache Lucene index file formats](https://lucene.apache.org/core/9_0_0/core/org/apache/lucene/codecs/lucene90/Lucene90PostingsFormat.html) — block layout, delta + bit-packing, PFOR
- [Tantivy](https://github.com/quickwit-oss/tantivy) — Rust, MIT, segments, BM25, block-max WAND, LZ4/Zstd doc store
- [Xapian](https://xapian.org/docs/overview.html) — glass backend, B+-tree tables, [scalability notes](https://xapian.org/docs/scalability.html)
- [Stract](https://github.com/StractOrg/stract) — independent Rust search engine built on Tantivy
- [Marginalia Search](https://github.com/MarginaliaSearch/MarginaliaSearch) — Java, AGPL-3.0, own crawler and index; ~32 GB RAM for a production-like deployment, 2–3 month crawl refresh
- [Trafilatura evaluation](https://trafilatura.readthedocs.io/en/stable/evaluation.html) — main-content extraction benchmarks
- Apache Nutch — CrawlDb, generate/fetch/parse rounds, per-host queues

**The 2026 crawling environment**

- [Cloudflare: blocking AI crawlers by default](https://www.cloudflare.com/press/press-releases/2025/cloudflare-just-changed-how-ai-crawlers-scrape-the-internet-at-large/) · [content controls](https://blog.cloudflare.com/control-content-use-for-ai-training/)
- [Anubis proof-of-work firewall](https://www.helpnetsecurity.com/2025/12/22/anubis-open-source-web-ai-firewall-protect-from-bots/) — adoption and limits
- [Robot discrimination: no robots allowed, except Googlebot](https://www.ctrl.blog/entry/robots-discrimination/)
- [Common Crawl URL index](https://commoncrawl.org/url-index) — WARC data and columnar index
- [Mojeek: 5 billion pages](https://blog.mojeek.com/2022/03/five-billion-pages.html) — independent index growth over time
- [Mojeek: Farewell Gigablast](https://blog.mojeek.com/2023/05/farewell-gigablast.html) — the cautionary case
- [AI detection tools falsely accuse international students](https://themarkup.org/machine-learning/2023/08/14/ai-detection-tools-falsely-accuse-international-students-of-cheating) — The Markup
