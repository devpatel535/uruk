# Uruk

> Named for the city where writing began. It doesn't tell you what the record
> says. It tells you where the record is.

**A search engine that returns links, not answers.** Independent crawler and
index. No ads, no tracking, no AI summaries.

---

## Status

**It works.** You can crawl a set of sites, build an index, and search it —
from the command line or from a web page.

```sh
uruk crawl  --seeds seeds.txt --out data/crawl --max-pages 1000
uruk index  --crawl data/crawl --out data/index
uruk merge  --index data/index --out data/index-merged
uruk link   --crawl data/crawl --seeds seeds.txt
uruk search --index data/index --crawl data/crawl "clay tablets"
uruk eval   --judgments queries.txt --without authority
uruk serve  --index data/index --crawl data/crawl
```

What is built, against the brief's order of work:

| Step | State |
|---|---|
| 0. Scaffold, licence, CI | done |
| 1. [Phase 0 research](RESEARCH.md) | done — **read this first** |
| 2. Crawler: fetch, parse, store politely | done |
| 3. Indexer: inverted index in `.uruk` segments | done |
| 4. Query engine: BM25F, phrases, a CLI | done |
| 5. Compression, benchmarked against alternatives | done — index cut from 49% to 29% of the text it describes; [the numbers, and why the codec was not the lever](RESEARCH.md#6-what-small-on-disk-actually-means-in-numbers) |
| 6. Link graph and authority scoring | done — host graph, in-degree and TrustRank; [what it measured against a link farm](RESEARCH.md#53-pagerank-on-a-small-crawl-does-almost-nothing--and-theres-evidence) |
| 7. Web front end | done |
| 8. Privacy hardening and self-host packaging | done — see [DEPLOYING.md](DEPLOYING.md) |
| 9. Scale the crawl, tune against a judged query set | harness done (`uruk eval`: nDCG, coverage, a significance test); the crawl itself is blocked on choosing a subject area |
| 10. Release quietly | yours to decide — nothing is published |
| 11. Browser | much later, as agreed |

[`RESEARCH.md`](RESEARCH.md) is still the thing to read before anything else.
It covers what engines of that era actually did, which of their failures this
is designed around, and — the part that matters — where the brief's plan does
not survive contact with the 2026 web.

### What is deliberately not done

Named here rather than discovered later. Each has a section in `RESEARCH.md`
explaining the reasoning, and none of them is a to-do that was forgotten.

- **Sub-200ms at a million pages, for two query shapes.** The budget holds to
  roughly 300,000 documents (worst case 65.0ms, measured), and to about three
  million for anything that is not a phrase over two extremely common words.
  What is left is not a missing optimisation but what an AND over two
  near-universal terms costs;
  [§6b](RESEARCH.md#6b-fast-to-search-measured--and-the-half-of-principle-4-nobody-had-checked)
  sets out why block-max pruning does not help and what would.
- **A real judged query set.** The harness exists; the judgments are a human
  judgement about a specific corpus and cannot be generated. See
  `crates/uruk-eval/examples/judgments.example.txt`.
- **Anchor text as an index field.** The link graph now stores anchors, so it
  is available — but adding a field changes the segment format, and §5.4's
  argument stands: add it with a way to measure whether it helped.

## Principles

These are constraints, not preferences. A design that violates one of them is
the wrong design.

1. **No AI-generated answers.** The engine points at sources. It never
   summarises, paraphrases, or writes prose. Machine learning may be used
   internally for ranking or spam detection; nothing generated is ever shown
   to a user as an answer.
2. **No ads. Ever.** No sponsored results, no promoted links, no affiliate
   rewrites. There is no ad slot in the design because there is nowhere to
   put one.
3. **No tracking.** No identifying cookies, no accounts, no per-person search
   history, no third-party scripts, no fingerprinting analytics.
4. **Small on disk, fast to search.** The index stays compressed at rest and is
   read per query, only for the posting lists a query actually mentions.
5. **Relevance beats popularity.** A page ranks because it matches the query
   and is linked to by credible pages — not because it is commercially large,
   recent, or frequently clicked.
6. **Plain output.** HTML and CSS. No JavaScript required to see results. It
   works in a text browser.

## How it fits together

| Component | Crate | What it does |
|---|---|---|
| CLI | `uruk` | One subcommand per component |
| Crawler | `uruk-crawl` | Fetches politely, extracts text and links, stores compressed |
| Indexer | `uruk-index` | Tokenises, builds `.uruk` segments, merges them on request |
| Link graph | `uruk-link` | Collapses links to hosts, scores authority, refuses to count self-votes |
| Query engine | `uruk-query` | Matches, ranks with BM25F, explains every result |
| Evaluation | `uruk-eval` | Judged queries, nDCG, and whether a change actually helped |
| Front end | `uruk-serve` | Server-rendered HTML, under 2 KB a page |

A crawl writes `pages.uruk` (block-compressed text) and `pages.idx`. An index
writes one or more `segment-*.uruk` files plus a readable `index.json`.
`uruk link` writes `authority.json` beside the crawl; `search` and `serve` pick
it up automatically, and rank on text alone if it is not there.

### What each piece actually guarantees

Claims worth being precise about, each of which has a test:

- The crawler reads `robots.txt` before anything else on a host and obeys it,
  including `Crawl-delay`, which is not in the standard. It makes at most one
  request to a host at a time and never faster than the configured delay —
  the end-to-end test asserts this by timing the requests a server receives.
- It honours `noindex`, `nofollow`, `noarchive`, `nosnippet` and
  `max-snippet`, from both meta tags and the `X-Robots-Tag` header.
- Redirects are not followed by the HTTP client. A 3xx goes back through the
  frontier so the target's own `robots.txt` is consulted.
- Search is boolean AND by default. `"quoted phrases"`, `-exclusion` and
  `site:host` all work, including phrases inside titles.
- Every result can show its per-signal score breakdown (`uruk search
  --explain`), and a test asserts the parts sum exactly to the score.
- A host linking to another host counts **once**, however many pages it uses,
  and a site cannot vote for itself — with "site" decided by the Public Suffix
  List, so `example.co.uk` is a site and two projects on one hosting service
  can vouch for each other. A fixture where a sixteen-page link farm
  is 84% of the corpus scores that farm zero.
- A ranking change is measured, not argued about — and `uruk eval` refuses to
  call a difference significant on too few queries, however large it is.
- The engine's one deliberate approximation — scoring proximity for the best
  hundred candidates rather than all of them — is checked by running both and
  comparing the results, not asserted.
- A merged index answers identically to the one it came from — same results,
  same order, same scores — which is asserted directly rather than by
  comparing files.
- The server sheds load rather than queueing it, and does so by counting
  searches rather than searchers — there is no per-visitor state to rate-limit
  against, deliberately.
- The server writes no access log and no record of any query. A test runs the
  real binary, searches for a unique string, and fails if that string appears
  in anything the process wrote.
- Served pages load nothing from anywhere else, set no cookies, and send no
  referrer to the sites they link to.
- A segment written by a different version of the format is refused by name,
  not decoded into plausible-looking wrong answers. That failure happened once
  during development and cost an afternoon, so the version is now checked on
  open and bumped on every encoding change.

## Building

Requires Rust 1.94 or newer; `rust-toolchain.toml` pins the version.

```sh
cargo build --release
cargo test --workspace          # 398 tests
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

Measuring what the index costs, on a corpus large enough to mean something:

```sh
cargo run --release --example index_size  -p uruk-index -- 100000
cargo run --release --example codec_bench -p uruk-index
cargo run --release --example query_bench -p uruk-query -- 100000
```

At 100,000 documents the index is **29% of the text it describes**, at 3.41
bytes per posting, and store and index together come to **3,951 bytes per
indexed page** — about 3.7 GB per million pages, with phrase search and
proximity intact. Both numbers print on every run, so a change that makes the
index fatter is visible immediately.

`query_bench` measures the other half of principle 4. At 100,000 documents
every case is inside the 200ms budget with the worst at **65.0ms**, down from
176ms when it was first measured. The work is linear in corpus size, so the
budget holds to roughly 300,000 documents — and to about three million for
every query that is not a phrase over two of the commonest words in the
language. [RESEARCH.md
§6b](RESEARCH.md#6b-fast-to-search-measured--and-the-half-of-principle-4-nobody-had-checked)
has the breakdown, the three fixes, and why the fourth one everybody names —
block-max pruning — turns out not to apply to what is left.

## Running your own

[`DEPLOYING.md`](DEPLOYING.md) is the operational guide: a hardened systemd
unit, a Dockerfile, disk sizing from the measured numbers, and the refresh
procedure.

Read the first section of it before anything else. The single most likely way
to break the privacy promise is not in this code — it is the reverse proxy in
front of it, whose default access log records the request URI, which for this
server is the query somebody typed. `deploy/proxy/` has configurations that do
not.

## Licence

[AGPL-3.0-or-later](LICENSE).

The reasoning, including what the licence does *not* protect against, is in
[RESEARCH.md](RESEARCH.md#8-licence-and-the-limits-of-what-it-protects). The
short version: the AGPL is the only common licence whose copyleft survives
being run as a network service, so a company cannot take this engine, improve
it privately, and offer a closed hosted competitor. It does not, and cannot,
prohibit ads — that promise is kept by us, not by the licence.

The licence covers the software. The crawled corpus and the built index are
separate questions, still open.
