# Crawlability probe

A Phase 0 measurement tool. It is **not** engine code and is not part of the
Cargo workspace — it is a throwaway instrument whose only job is to answer one
question with evidence instead of instinct:

> Which subject area should the first crawl cover?

[`RESEARCH.md`](../../RESEARCH.md) §5.1 and §5.2 argue that crawl *access*, not
crawler code, is the binding constraint on this project, and that the subject
area therefore decides how good the engine can possibly feel. This measures
that directly, before we commit.

## What it measures

For each candidate domain, two requests — `robots.txt`, then the homepage:

| Question | How it is answered |
|---|---|
| Would a new crawler be let in? | `robots.txt` posture for a `uruk-crawl` user-agent, plus whether the edge blocks or challenges us before robots.txt matters at all |
| Is Google privileged over us? | Whether `robots.txt` allows `Googlebot` while disallowing us — the barrier-to-entry signal |
| Is the text there without JavaScript? | Server-rendered text volume, text-to-markup ratio, and detection of framework shells that ship an empty `<div id="root">` |
| Do these sites link to each other? | How many homepage links point at *other domains in the same candidate list* — which is what makes crawling outward from seeds work |

It also records which sites sit behind Cloudflare even when they serve us, and
which declare a `Crawl-delay`.

## Running it

Standard library only — Python 3.10+, nothing to install.

```sh
./probe.py candidates/*.txt --out results
```

Roughly 250 requests total for the three supplied lists, two per domain, at
eight domains in parallel. It takes a few minutes.

Useful flags: `--workers` (domains probed in parallel; never more than one
request to the same host), `--delay` (seconds between the two requests to a
given host), `--out` (directory for per-area JSONL).

## Politeness

This is our first contact with these sites and it is designed to be
unobjectionable:

- exactly **two requests per domain**, ever;
- an honest user-agent naming the project with a contact URL;
- `robots.txt` is fetched first and **obeyed** — if it disallows us, the
  homepage is not fetched, and that is recorded as the result;
- RFC 9309 status handling: `404` means unrestricted, `401`/`403` means fully
  disallowed, `5xx` means treat as disallowed for now;
- no retries, a 2 MB response cap, and concurrency only ever across different
  domains.

## Reading the output

Per-area summary on stdout, and one JSON object per domain in
`results/<area>.jsonl` so any number in the summary can be traced back to the
domain that produced it.

The two headline numbers are **crawlable** (robots allows us *and* we got a
200 with HTML) and **real text without JS**. The second is the one that
matters: a site that lets us in but ships an empty shell is no more useful to
us than one that blocks us.

## Two caveats

**It will not run from behind a filtering proxy.** If outbound HTTPS is
restricted, the proxy's own `403` on a `CONNECT` is indistinguishable from the
site returning `403`, and every domain will be scored as blocked or
unreachable. The results are only meaningful from a connection with open
egress. (This is why the numbers were not produced in the session that wrote
this tool.)

**One homepage is not a site.** The probe measures the front door. Article
pages can differ — a homepage may be a JS shell while the posts are static, or
the reverse. Treat the output as a ranking signal between candidate areas, not
as a per-site verdict.
