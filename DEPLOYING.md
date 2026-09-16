# Running your own

Uruk is meant to be self-hosted. That is not a feature added for enthusiasts —
it is the only thing that makes the privacy promise checkable. A promise not to
track you, made by someone else's server, is a promise you cannot verify. Run
it yourself and you do not have to.

This file is the operational half. The design reasoning is in
[`RESEARCH.md`](RESEARCH.md).

---

## The one thing most likely to go wrong

**Your reverse proxy will log everybody's searches, by default, and nothing in
this codebase can stop it.**

`uruk serve` writes two lines at start-up and nothing per request. No access
log, no query log, no request IDs. That is deliberate and it is tested by the
absence of any logging call in the request path.

But almost nobody runs a web service without a proxy in front of it for TLS,
and every proxy's default access log records the request URI. For this server
the request URI is:

```
/search?q=whatever+the+person+typed
```

So an ordinary, entirely sensible default — the same one you would want on any
other site — turns into a file containing every search anybody made, with their
IP address and the time, kept for however long log rotation is set to keep it.
The `/privacy` page says a search is not stored against an IP address. With a
default nginx or Caddy config in front, that sentence is false, and it is false
in a file nobody reads.

`deploy/proxy/nginx.conf` and `deploy/proxy/Caddyfile` are configurations that
turn it off, with the reasoning in comments. Start from those.

If you need to know the service is up, monitor it from outside — a request
every minute to `/` from a checker you control tells you the same thing without
writing down what anybody asked.

---

## What this costs to run

**The software costs nothing and has no way to start costing something.** Every
dependency is permissive open source; there are no API keys anywhere, no
managed database, no CDN, no analytics service, no email sender, no auth
provider. That is not thrift, it is the principles: no ads means no ad
server, no tracking means no analytics subscription, no JavaScript means no
CDN, and no accounts means nothing to send a password reset from. A normal web
product's recurring bill is mostly things this engine is not allowed to have.

What is left is a computer, some disk, and some bandwidth.

| Resource | Measured | For 100,000 pages | For 1,000,000 |
|---|---|---|---|
| Disk, store + index | 3,990 bytes/page | **0.4 GB** | 3.7 GB |
| Indexing | 1,040 pages/second | ~2 minutes | ~16 minutes |
| Query, worst case | 65ms at 100k docs | comfortable | see [§6b](RESEARCH.md#6b-fast-to-search-measured--and-the-half-of-principle-4-nobody-had-checked) |
| Binary | — | 7.6 MB | 7.6 MB |

Memory is not the constraint it looks like. Posting lists are read per query
rather than held resident, so serving wants very little. Indexing holds one
segment's postings — about 85 MB at the default 50,000 documents per segment —
plus the anchor-text map, which is bounded by the size of the crawl rather
than by the number of links in it (roughly 12 MB of URL hashes per million
pages, plus a few hundred bytes for each page other sites actually describe).

**A 100,000-page index runs on a Raspberry Pi.** It is 0.4 GB on disk and
answers every query shape in well under a tenth of a second.

### The one thing that is not free

**Bandwidth, while crawling.** Roughly 50 GB per million pages fetched. On
home broadband that is usually unremarkable; on a metered or capped
connection it is the whole budget. `--max-pages` bounds it directly, and a
100,000-page crawl is about 5 GB.

Two non-monetary costs worth pricing in the same breath. Crawling from a
residential address risks the address being blocked by sites that do not
expect a crawler there — the politeness defaults exist partly for that. And a
judged query set costs hours of somebody's attention rather than money, which
does not make it cheap.

### Not hosting it at all

The cheapest public deployment is not a deployment. A 100,000-page topical
index is **0.4 GB**, which is small enough to distribute as a file: people
download it, run `uruk serve`, and search it locally. No server, no domain, no
TLS certificate, no bandwidth beyond the download, and nothing to keep running
or patch.

It also happens to be the strongest version of the privacy promise. A search
that never leaves the machine cannot be logged by a reverse proxy, which is
the failure mode the top of this file is about.

[RESEARCH.md §6](RESEARCH.md#6-what-small-on-disk-actually-means-in-numbers)
argues this is realistic up to about 1–10 million pages and stops being
realistic above that. If you are starting with no budget, start here — a
public instance can come later and nothing about the corpus has to change.

### If you do want it on the public internet for nothing

Be careful with free hosting tiers, and not mainly because of the price:

- **Most forbid crawlers outright.** Running a crawler on a platform whose
  terms prohibit it is a way to lose the account and the corpus with it.
- **Most give no persistent disk.** An index is a file. A platform that
  resets the filesystem on every deploy will lose it, and rebuilding means
  re-crawling.
- **Egress is usually the metered thing**, and serving search results is
  egress.

A realistic split is to **crawl and index on a machine you own**, where none
of those apply, and put only the serving half somewhere else — that part needs
a few hundred megabytes of disk and almost no CPU. Whether any particular
free tier allows even that changes often enough that this file will not name
one; read the current terms rather than trusting a document.

**A domain is not required.** Nothing in this engine needs one. The crawler
identifies itself by the repository URL, which is what the `/crawler` page
needs to be true about. Registering a name is a decision to make when there is
something at the end of it, not a prerequisite.

---

## Layout

```
/usr/local/bin/uruk          the binary, one file
/var/lib/uruk/crawl/         pages.uruk, pages.idx, authority.json
/var/lib/uruk/index/         segment-*.uruk, index.json
```

About **4 KB of disk per indexed page**, crawl store and index together —
measured, not estimated; see [RESEARCH.md
§6](RESEARCH.md#6-what-small-on-disk-actually-means-in-numbers). So:

| Pages | Disk | Runs on |
|---|---|---|
| 100,000 | ~0.4 GB | Anything. A Raspberry Pi. |
| 1 million | ~3.7 GB | A small VPS. |
| 10 million | ~37 GB | A VPS with a real disk. |

Memory is not the constraint: posting lists are read per query rather than held
resident, so the working set is whatever the page cache decides to keep.

---

## Build and install

```sh
cargo build --release --locked
sudo install -m 0755 target/release/uruk /usr/local/bin/uruk

sudo useradd --system --no-create-home --shell /usr/sbin/nologin uruk
sudo mkdir -p /var/lib/uruk
sudo chown -R uruk:uruk /var/lib/uruk
```

## Build the corpus

This is the part that takes time and judgement. The four commands:

```sh
# 1. Crawl. Politely: the default is one request per host every 3 seconds,
#    and lowering it is the fastest way to get blocked.
uruk crawl --seeds seeds.txt --out /var/lib/uruk/crawl --max-pages 200000

# 2. Score the link graph. Do this before indexing so search can use it.
uruk link --crawl /var/lib/uruk/crawl --seeds seeds.txt

# 3. Index.
uruk index --crawl /var/lib/uruk/crawl --out /var/lib/uruk/index

# 3b. Merge the segments. Optional, and worth it: a query reads one posting
#     list per term *per segment*, so an index left in twenty segments makes a
#     three-word search sixty reads instead of three.
uruk merge --index /var/lib/uruk/index --out /var/lib/uruk/index-merged

# 4. Check the ranking against your judged queries, if you have written any.
uruk eval --judgments queries.txt \
          --index /var/lib/uruk/index --crawl /var/lib/uruk/crawl
```

Your seed list is the product. There is no click data and no personalisation to
fall back on, so what you choose to crawl *is* the quality of the engine — the
argument is in [RESEARCH.md
§5.4](RESEARCH.md#54-no-click-data-means-curation-is-the-product).

### Refreshing

Crawl into a **new** directory, index it, and swap:

```sh
uruk crawl --seeds seeds.txt --out /var/lib/uruk/crawl.new --max-pages 200000
uruk link  --crawl /var/lib/uruk/crawl.new --seeds seeds.txt
uruk index --crawl /var/lib/uruk/crawl.new --out /var/lib/uruk/index.new
uruk merge --index /var/lib/uruk/index.new --out /var/lib/uruk/index.merged
rm -rf /var/lib/uruk/index.new && mv /var/lib/uruk/index.merged /var/lib/uruk/index.new

sudo systemctl stop uruk
sudo mv /var/lib/uruk/crawl /var/lib/uruk/crawl.old
sudo mv /var/lib/uruk/index /var/lib/uruk/index.old
sudo mv /var/lib/uruk/crawl.new /var/lib/uruk/crawl
sudo mv /var/lib/uruk/index.new /var/lib/uruk/index
sudo systemctl start uruk
```

Building in place would leave the server reading a half-written index, and the
segment format is not designed to be read while it is being written. The swap
costs a few seconds of downtime and the `.old` directories are your rollback.

**An index and a crawl store have to match.** The index holds crawl document
ids; pointing `serve` at an index built from a different crawl gives wrong
titles and snippets. The segment format version guards against *stale formats*,
not against mismatched pairs, so move them together.

---

## Run it

### systemd

```sh
sudo cp deploy/uruk.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now uruk
```

The unit is hardened: read-only filesystem, no capabilities, no new
privileges, a syscall filter, and `AF_INET`/`AF_INET6` only. `uruk serve` only
ever reads, so `/var/lib/uruk` is mounted read-only to it — which means a
compromise of the server process cannot corrupt the index it is serving.

### Docker

```sh
docker build -f deploy/Dockerfile -t uruk .
docker run --rm -p 127.0.0.1:8080:8080 -v "$PWD/data:/data:ro" uruk
```

`:ro` for the same reason.

### Directly

```sh
uruk serve --index /var/lib/uruk/index --crawl /var/lib/uruk/crawl
```

Loopback by default. Putting a search engine on a public address should be a
decision, not an accident, so `--address` is how you say you meant it.

---

## What the server already does for you

Set on every response, no configuration required:

| Header | Value |
|---|---|
| `Content-Security-Policy` | `default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'` |
| `Referrer-Policy` | `no-referrer` |
| `X-Content-Type-Options` | `nosniff` |
| `Cache-Control` | `no-store` |

`default-src 'none'` means the page cannot load anything from anywhere,
including from itself — there is nothing to load. `no-referrer` means a site
you click through to is not told what you searched for. `no-store` means the
results page is not left in a shared cache.

No cookies are set. There is no JavaScript. Nothing is fetched from a third
party.

Leave `Strict-Transport-Security` to the proxy: only the proxy knows whether
TLS actually terminates there.

---

## Things to decide before you make it public

These are genuinely your decisions, and none of them has an obvious answer.

**The crawler's identity.** The default user-agent points at this repository.
If you are running your own crawl, point it at *your* contact address instead —
`uruk crawl --user-agent`. A site owner who wants your crawler to stop needs
somewhere to write to, and the `/crawler` page needs to be true about you.

**Your seed list, published or not.** Publishing it makes your engine's biases
inspectable, which is the main thing that separates a curated index from an
opaque one. It also tells spammers exactly which doors to knock on.

**Raising or lowering the search cap.** Built in, and on by default: the
server answers **8 searches at once** and turns the rest away with a `503` and
a `Retry-After`, rather than queueing work it cannot get to.

It counts *requests*, not *requesters*. The usual rate limiter is keyed by IP
address, which means a table of who is asking — precisely the thing `/privacy`
says does not exist. "We only keep it for sixty seconds" is a weaker promise
than "there is nowhere to put it".

The trade is real and you should know it before you deploy: **one heavy user
can consume the whole allowance**, where a per-IP limit would have contained
them. The engine cannot tell one impatient person from forty patient ones, and
it has been built so that it cannot.

```sh
uruk serve --max-concurrent-searches 16
```

Eight is a floor, not a tuned number: searches serialise on the index lock, so
the cap bounds how many requests *wait* for it. Raise it if you have fast
storage and see 503s under normal load; lower it if queries are slow and you
would rather shed early. If you need something sharper than this, it belongs
at the proxy, and putting it there is your decision about what you are willing
to record.

**Retention of nothing.** Worth checking, not assuming: no logs, no metrics
with URIs in them, no error tracker that captures the request, no backup of a
log you turned off after it had already been written.

---

---

## What in this file has actually been checked

Being specific, because deployment documentation is where confident prose most
often outruns evidence.

**Verified:**

- `uruk serve` writes nothing per request. There is a test
  (`crates/uruk/tests/no_query_logging.rs`) that starts the real binary,
  searches for a unique string, and fails if that string appears in anything
  the process wrote.
- `deploy/uruk.service` parses and every directive in it is accepted:
  `systemd-analyze verify` reports no problems.
- The whole pipeline — crawl, link, index, search, serve, eval — has been run
  end to end with release binaries against local fixture sites.
- The disk figures come from `cargo run --release --example index_size`, at
  20,000 and 100,000 documents.

**Not verified, and you should check it yourself:**

- `deploy/Dockerfile` has never been built. It is written from the standard
  two-stage pattern and it should work, but "should" is doing real work in that
  sentence — no container runtime was available where it was written.
- The proxy configurations have not been run against a live nginx or Caddy.
  The reasoning in them is the point; the syntax deserves a `nginx -t` or a
  `caddy validate` before you rely on it.
- `systemd-analyze security`, which scores the hardening, needs a booted
  systemd and could not be run. The directives are the well-known ones; the
  score is not claimed.

## Licence

AGPL-3.0-or-later. Running a modified version as a network service obliges you
to offer its source to its users — that is the clause the licence was chosen
for, and it applies to you as an operator, not only to redistributors. If you
change it and run it publicly, publish the change.
