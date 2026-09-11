# Uruk

> Named for the city where writing began. It doesn't tell you what the record
> says. It tells you where the record is.

**A search engine that returns links, not answers.** Independent crawler and
index. No ads, no tracking, no AI summaries.

---

## Status

**Phase 0 — research.** This repository currently contains the scaffold, the
licence, and [`RESEARCH.md`](RESEARCH.md). There is no engine code yet, and
that is deliberate: the research document has to be read and argued with
before the first component is designed.

`RESEARCH.md` is the thing to read. It covers what search engines of the
2003–2014 era actually did, which of their failures we are designing around,
where each of our ideas is borrowed from, and — the part that matters most —
the places where this project's stated plan does not survive contact with the
2026 web.

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
4. **Small on disk, fast to search.** The index stays compressed at rest and
   is decompressed per query, only for the blocks a query actually touches.
5. **Relevance beats popularity.** A page ranks because it matches the query
   and is linked to by credible pages — not because it is commercially large,
   recent, or frequently clicked.
6. **Plain output.** HTML and CSS. No JavaScript required to see results. It
   should work in a text browser.

## Planned shape

Component names are fixed; the components themselves are built one phase at a
time, and no crate exists here until it has been designed.

| Component | Name | What it does |
|---|---|---|
| CLI | `uruk` | One binary, one subcommand per component |
| Crawler | `uruk-crawl` | Fetches pages politely, extracts text and links |
| Indexer | `uruk-index` | Turns crawled text into an inverted index |
| Query engine | `uruk-query` | Matches and ranks, with a per-signal score breakdown |
| Web front end | `uruk-serve` | Server-rendered HTML, under 20 KB a page |
| Index format | `.uruk` segments | Immutable, compressed, merged in the background |

## Order of work

0. Scaffold — **done**
1. Phase 0 research → `RESEARCH.md` → **here now; waiting on review**
2. Crawler that fetches, parses and stores 1,000 pages politely
3. Indexer that turns those into a searchable inverted index
4. Query engine with BM25 and a command-line search interface
5. Compression, benchmarked against alternatives
6. Link graph and authority scoring
7. Web front end
8. Privacy hardening and self-host packaging
9. Scale the crawl, tune ranking against a held-out query set
10. Release quietly. Listen. Iterate.
11. Browser, eventually

## Building

Requires Rust 1.94 or newer; `rust-toolchain.toml` pins the version and
installs the components CI uses.

```sh
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets   # lint set lives in the workspace manifest
cargo fmt --all --check
```

The binary currently answers for itself and nothing else:

```sh
$ uruk version
uruk 0.0.0
```

## Licence

[AGPL-3.0-or-later](LICENSE).

The reasoning, including what the licence does *not* protect against, is in
[RESEARCH.md](RESEARCH.md#8-licence-and-the-limits-of-what-it-protects). The
short version: the AGPL is the only common licence whose copyleft survives
being run as a network service, so a company cannot take this engine, improve
it privately, and offer a closed hosted competitor. It does not, and cannot,
prohibit ads — that promise is kept by us, not by the licence.

Note that the licence covers the software. The crawled corpus and the built
index are separate questions, still open.
