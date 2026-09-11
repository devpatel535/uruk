#!/usr/bin/env python3
"""Crawlability probe — measures whether a subject area is reachable by a new crawler.

Phase 0 instrument. This is not engine code and is not part of the Cargo
workspace; it is a throwaway measurement tool whose output decides which
subject area the first crawl should cover.

For each candidate domain it answers three questions:

  1. Would a brand-new crawler be allowed in?  (robots.txt posture, and
     whether the edge blocks us before robots.txt is even consulted)
  2. Is the text there without JavaScript?     (server-rendered text volume
     and text-to-markup ratio)
  3. Do these sites link to each other?        (in-set link density, which is
     what makes crawling outward from seeds actually work)

Politeness: exactly two requests per domain (robots.txt, then the homepage),
an honest user-agent with a contact URL, no retries, and concurrency only
ever across different domains — never within one. This is our first contact
with these sites and it should be unobjectionable.

Standard library only. No pip install.

Usage:
    ./probe.py candidates/*.txt --out results
    ./probe.py candidates/science.txt --workers 8
"""

from __future__ import annotations

import argparse
import concurrent.futures
import gzip
import io
import json
import pathlib
import re
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import urllib.robotparser

# Identify honestly. If this ever runs against real sites, the contact URL
# must resolve to a page explaining who we are and how to block us.
USER_AGENT = "uruk-crawl/0.1 (+https://github.com/devpatel535/uruk; crawlability survey)"

# Token a robots.txt would use to name us specifically.
URUK_TOKEN = "uruk-crawl"

TIMEOUT = 15
MAX_BYTES = 2 * 1024 * 1024  # One oversized page must not stall the run.

# Signatures of an edge that challenges or blocks rather than serving content.
CHALLENGE_PATTERNS = [
    (re.compile(rb"cf[-_]chl|challenge-platform|Just a moment|Checking your browser"
                rb"|Enable JavaScript and cookies to continue", re.I), "cloudflare-challenge"),
    (re.compile(rb"anubis|proof[- ]of[- ]work|Making sure you're not a bot", re.I), "proof-of-work"),
    (re.compile(rb"Attention Required|Access denied|Request blocked", re.I), "edge-block"),
]

# A near-empty body plus a framework mount point means the text is in JS.
JS_SHELL_ROOT = re.compile(
    rb"""<div[^>]+id=["'](root|__next|app|__nuxt|svelte)["']""", re.I)

SCRIPT_STYLE = re.compile(rb"<(script|style|template)\b.*?</\1>", re.I | re.S)
NOSCRIPT = re.compile(rb"<noscript\b", re.I)
TAG = re.compile(rb"<[^>]+>")
HREF = re.compile(rb"""<a\s[^>]*href=["']([^"'#][^"']*)["']""", re.I)
WS = re.compile(r"\s+")


def registrable(host: str) -> str:
    """Close enough to a registrable domain for link-density counting."""
    host = host.lower().split(":")[0]
    return host[4:] if host.startswith("www.") else host


def fetch(url: str) -> dict:
    """One GET. Never raises; failures come back as a status of 0 and an error."""
    req = urllib.request.Request(url, headers={
        "User-Agent": USER_AGENT,
        "Accept": "text/html,text/plain,*/*",
        # No Accept-Encoding: keep the bytes simple. Some servers compress
        # regardless, which is handled below.
    })
    started = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            body, status, headers = resp.read(MAX_BYTES), resp.status, dict(resp.headers)
    except urllib.error.HTTPError as exc:  # 4xx/5xx still carry a useful body
        body, status, headers = exc.read(MAX_BYTES) if exc.fp else b"", exc.code, dict(exc.headers or {})
    except Exception as exc:  # DNS, TLS, timeout, connection reset
        return {"status": 0, "error": f"{type(exc).__name__}: {exc}",
                "headers": {}, "body": b"", "ms": int((time.monotonic() - started) * 1000)}

    if headers.get("Content-Encoding", "").lower() == "gzip":
        try:
            body = gzip.GzipFile(fileobj=io.BytesIO(body)).read(MAX_BYTES)
        except OSError:
            pass
    return {"status": status, "error": None, "headers": headers, "body": body,
            "ms": int((time.monotonic() - started) * 1000)}


def robots_posture(domain: str) -> dict:
    """What robots.txt says about us, and about Googlebot, and the gap between them.

    Status handling follows RFC 9309: 404 means unrestricted, 401/403 means
    fully disallowed, 5xx means treat the whole site as disallowed for now.
    """
    r = fetch(f"https://{domain}/robots.txt")
    out = {"robots_status": r["status"], "robots_error": r["error"]}

    if r["status"] == 0:
        out.update(allows_us=None, allows_googlebot=None, googlebot_favoured=None,
                   crawl_delay=None, names_us=False, robots_verdict="unreachable")
        return out
    if r["status"] in (401, 403):
        out.update(allows_us=False, allows_googlebot=False, googlebot_favoured=False,
                   crawl_delay=None, names_us=False, robots_verdict="robots-forbidden")
        return out
    if r["status"] >= 500:
        out.update(allows_us=False, allows_googlebot=False, googlebot_favoured=False,
                   crawl_delay=None, names_us=False, robots_verdict="server-error")
        return out
    if r["status"] == 404:
        out.update(allows_us=True, allows_googlebot=True, googlebot_favoured=False,
                   crawl_delay=None, names_us=False, robots_verdict="absent-so-allowed")
        return out

    text = r["body"].decode("utf-8", errors="replace")
    parser = urllib.robotparser.RobotFileParser()
    parser.parse(text.splitlines())
    root = f"https://{domain}/"

    allows_us = parser.can_fetch(URUK_TOKEN, root)
    allows_googlebot = parser.can_fetch("Googlebot", root)
    try:
        delay = parser.crawl_delay(URUK_TOKEN) or parser.crawl_delay("*")
    except (AttributeError, TypeError):
        delay = None

    out.update(
        allows_us=allows_us,
        allows_googlebot=allows_googlebot,
        # The barrier-to-entry signal: Google gets in, we do not.
        googlebot_favoured=bool(allows_googlebot and not allows_us),
        crawl_delay=delay,
        names_us=URUK_TOKEN in text.lower(),
        robots_verdict="allowed" if allows_us else "disallowed",
    )
    return out


def page_signals(body: bytes, domain: str, candidates: set[str]) -> dict:
    """Text volume, markup ratio and link structure from server-rendered HTML."""
    html_len = len(body)
    stripped = SCRIPT_STYLE.sub(b" ", body)
    text = WS.sub(" ", TAG.sub(b" ", stripped).decode("utf-8", errors="replace")).strip()

    hosts = []
    for raw in HREF.findall(body):
        try:
            netloc = urllib.parse.urlparse(
                urllib.parse.urljoin(f"https://{domain}/", raw.decode("utf-8", "replace"))).netloc
        except ValueError:
            continue
        if netloc:
            hosts.append(registrable(netloc))

    me = registrable(domain)
    external = {h for h in hosts if h and h != me}
    return {
        "html_bytes": html_len,
        "text_chars": len(text),
        # How much of what we downloaded was actually words.
        "text_ratio": round(len(text) / html_len, 4) if html_len else 0.0,
        "internal_links": sum(1 for h in hosts if h == me),
        "external_domains": len(external),
        # Interlink density within the candidate set: does crawling outward work?
        "links_into_set": len(external & candidates),
        "has_noscript": bool(NOSCRIPT.search(body)),
        "js_shell": bool(JS_SHELL_ROOT.search(body)) and len(text) < 500,
    }


def classify(status: int, body: bytes, headers: dict) -> str:
    """What actually happened, from a crawler's point of view."""
    for pattern, label in CHALLENGE_PATTERNS:
        if pattern.search(body[:200_000]):
            return label
    if status == 0:
        return "unreachable"
    if status == 429:
        return "rate-limited"
    if status in (401, 403):
        return "forbidden"
    if status == 503:
        return "unavailable"
    if status >= 500:
        return "server-error"
    if status >= 400:
        return "client-error"
    if "html" not in headers.get("Content-Type", "").lower():
        return "not-html"
    return "ok"


def probe(domain: str, candidates: set[str], delay: float) -> dict:
    """Two requests, robots first — the same order a real crawl would use."""
    record = {"domain": domain}
    record.update(robots_posture(domain))

    # Respect our own finding. If robots.txt disallows us, we do not fetch the
    # page; we record that and move on. That is the whole point of the file.
    if record["allows_us"] is False:
        record.update(outcome="robots-disallowed", status=None,
                      server=None, cloudflare=None)
        return record

    time.sleep(delay)
    r = fetch(f"https://{domain}/")
    server = r["headers"].get("Server", "")
    record.update(
        status=r["status"],
        fetch_error=r["error"],
        ms=r["ms"],
        server=server or None,
        cloudflare="cloudflare" in server.lower() or "CF-RAY" in r["headers"],
        outcome=classify(r["status"], r["body"], r["headers"]),
    )
    if record["outcome"] == "ok":
        record.update(page_signals(r["body"], domain, candidates))
    return record


def pct(n: int, total: int) -> str:
    return f"{100 * n / total:5.1f}%" if total else "    -"


def summarise(area: str, rows: list[dict]) -> str:
    n = len(rows)
    ok = [r for r in rows if r.get("outcome") == "ok"]
    text_ok = [r for r in ok if not r.get("js_shell") and r.get("text_chars", 0) >= 500]

    def count(fn) -> int:
        return sum(1 for r in rows if fn(r))

    lines = [
        f"\n=== {area} ({n} domains) ===",
        f"  crawlable (robots allows + 200 HTML)   {pct(len(ok), n)}  ({len(ok)}/{n})",
        f"  real text without JS                   {pct(len(text_ok), n)}  ({len(text_ok)}/{n})",
        "  --- why the rest failed ---",
        f"  robots.txt disallows us                {pct(count(lambda r: r.get('outcome') == 'robots-disallowed'), n)}",
        f"  Googlebot allowed but we are not       {pct(count(lambda r: r.get('googlebot_favoured')), n)}",
        f"  challenged (Cloudflare / proof-of-work){pct(count(lambda r: r.get('outcome') in ('cloudflare-challenge', 'proof-of-work')), n)}",
        f"  forbidden / rate-limited               {pct(count(lambda r: r.get('outcome') in ('forbidden', 'rate-limited', 'edge-block')), n)}",
        f"  unreachable                            {pct(count(lambda r: r.get('outcome') == 'unreachable'), n)}",
        f"  JavaScript shell (no server text)      {pct(count(lambda r: r.get('js_shell')), n)}",
        "  --- shape of what we did get ---",
        f"  behind Cloudflare (served anyway)      {pct(count(lambda r: r.get('cloudflare')), n)}",
        f"  declares Crawl-delay                   {pct(count(lambda r: r.get('crawl_delay')), n)}",
    ]
    if ok:
        ratios = [r["text_ratio"] for r in ok]
        chars = [r["text_chars"] for r in ok]
        into_set = [r["links_into_set"] for r in ok]
        lines += [
            f"  median text:markup ratio               {statistics.median(ratios):.3f}",
            f"  median text on homepage                {int(statistics.median(chars)):,} chars",
            f"  mean links into the candidate set      {statistics.mean(into_set):.1f} per homepage",
            f"  domains linking to >=1 other candidate  "
            f"{pct(sum(1 for v in into_set if v), len(ok))}  (of crawlable)",
        ]
    return "\n".join(lines)


def load(path: pathlib.Path) -> tuple[str, list[str]]:
    domains = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            domains.append(registrable(line))
    return path.stem, domains


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("files", nargs="+", type=pathlib.Path,
                    help="candidate lists, one domain per line; filename is the area name")
    ap.add_argument("--workers", type=int, default=8,
                    help="domains probed in parallel (never >1 request to the same host)")
    ap.add_argument("--delay", type=float, default=1.0,
                    help="seconds between robots.txt and homepage for a given host")
    ap.add_argument("--out", type=pathlib.Path, default=None,
                    help="directory for per-area JSONL results")
    args = ap.parse_args()

    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)

    summaries = []
    for path in args.files:
        area, domains = load(path)
        candidates = set(domains)
        print(f"probing {area}: {len(domains)} domains "
              f"({2 * len(domains)} requests total)...", file=sys.stderr)

        rows: list[dict] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
            futures = {pool.submit(probe, d, candidates, args.delay): d for d in domains}
            for future in concurrent.futures.as_completed(futures):
                domain = futures[future]
                try:
                    rows.append(future.result())
                except Exception as exc:  # a probe bug must not lose the run
                    rows.append({"domain": domain, "outcome": "probe-error",
                                 "error": f"{type(exc).__name__}: {exc}"})
        rows.sort(key=lambda r: r["domain"])

        if args.out:
            target = args.out / f"{area}.jsonl"
            with target.open("w", encoding="utf-8") as fh:
                for row in rows:
                    fh.write(json.dumps(row, default=str) + "\n")
            print(f"  wrote {target}", file=sys.stderr)

        summaries.append(summarise(area, rows))

    print("\n".join(summaries))
    return 0


if __name__ == "__main__":
    sys.exit(main())
