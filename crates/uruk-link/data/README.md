# Vendored data

## `public_suffix_list.dat`

The Public Suffix List: which domain suffixes are registries rather than sites.
It is what tells `uruk-link` that `example.co.uk` is a site while `co.uk` is
not, and that `alice.github.io` and `bob.github.io` are *different* sites
despite sharing two labels.

- **Licence:** Mozilla Public License 2.0, as stated in the file's own header,
  which is kept intact. MPL-2.0 is file-level copyleft and imposes nothing on
  the rest of this repository; the file must stay under MPL-2.0 and keep its
  notice, which is why it is vendored verbatim rather than trimmed.
- **Canonical source:** <https://publicsuffix.org/list/public_suffix_list.dat>.
  The list's own header asks that it be pulled from there and nowhere else.
- **Where this copy actually came from:**
  `raw.githubusercontent.com/publicsuffix/list/master/public_suffix_list.dat`,
  because publicsuffix.org is not reachable from the network this was built on
  — the proxy refuses it with a 403. That is a deviation from what the file
  asks for and it is written down rather than glossed over. Anyone refreshing
  this should use the canonical URL.

### Refreshing it

The list changes as registries do — a few times a month. A stale copy is not a
correctness disaster: it means a newly delegated suffix is treated as an
ordinary domain for a while, which makes the link graph slightly more
credulous about a handful of hosts, in the same direction the old
approximation erred.

```sh
curl -o crates/uruk-link/data/public_suffix_list.dat \
     https://publicsuffix.org/list/public_suffix_list.dat
cargo test -p uruk-link
```

The tests assert specific known suffixes (`co.uk`, `github.io`), so a refresh
that breaks the format fails loudly rather than silently parsing to nothing.
