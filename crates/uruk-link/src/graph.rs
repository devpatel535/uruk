//! The host link graph, built from a crawl.
//!
//! # What counts as an edge
//!
//! An edge `from -> to` exists when some page on host `from` links to some
//! page on host `to`. Four kinds of link are dropped before they become edges,
//! and each exclusion is a decision rather than a tidy-up:
//!
//! - **`rel="nofollow"`, `ugc` and `sponsored`.** The author has explicitly
//!   declined to vouch. Honouring that is the entire reason the attribute
//!   exists, and a crawler that ignores it is asking every comment section on
//!   the web to be a vote.
//! - **Self-links**, by the [`host::same_site`] definition. A site linking to
//!   itself is a navigation menu, not evidence.
//! - **Links from pages we could not index** — a page carrying `noindex` still
//!   gets crawled for its links, but a page that failed to fetch has no links
//!   to give.
//! - **Repeats.** A host linking to another host a thousand times is one edge,
//!   not a thousand. This is the single most important line in the file:
//!   without it, authority is a function of how many pages a site has, and the
//!   cheapest attack on the whole signal is to generate pages.
//!
//! What survives is one number per ordered pair of hosts — how many *distinct
//! pages* on the source host linked to the target — kept only so that a human
//! reading the graph can see the difference between a single mention and a
//! sitewide footer link. The authority algorithms deliberately ignore it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use uruk_crawl::store::{StoreError, StoreReader};

use crate::host;

/// A host's position in the graph.
#[derive(Debug, Clone, Default)]
pub struct HostNode {
    /// Pages from this host that were crawled and stored.
    pub pages: u32,
    /// Hosts this one links to, and how many of its pages do so.
    pub out: BTreeMap<u32, u32>,
    /// Hosts linking here, and how many of their pages do so.
    pub incoming: BTreeMap<u32, u32>,
}

impl HostNode {
    /// Distinct hosts linking here.
    ///
    /// This, and not the number of links, is the in-degree every algorithm in
    /// this crate uses.
    pub fn in_degree(&self) -> usize {
        self.incoming.len()
    }

    /// Distinct hosts this one links to.
    pub fn out_degree(&self) -> usize {
        self.out.len()
    }
}

/// The crawl's hosts and the links between them.
#[derive(Debug, Default)]
pub struct HostGraph {
    names: Vec<String>,
    ids: HashMap<String, u32>,
    nodes: Vec<HostNode>,
    /// Hosts that were seeds, which `TrustRank` propagates from.
    seeds: BTreeSet<u32>,
    /// Links discarded, by reason, for the report.
    pub dropped: Dropped,
}

/// Why links did not become edges. Printed after a build, because a graph with
/// no edges should say why rather than look empty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dropped {
    pub nofollow: usize,
    pub same_site: usize,
    pub unparseable: usize,
    pub repeat: usize,
}

impl HostGraph {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    pub fn id(&self, host: &str) -> Option<u32> {
        self.ids.get(host).copied()
    }

    pub fn node(&self, id: u32) -> &HostNode {
        &self.nodes[id as usize]
    }

    pub fn nodes(&self) -> impl Iterator<Item = (u32, &HostNode)> {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "ids are assigned by pushing, so they fit u32 by construction"
        )]
        self.nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (index as u32, node))
    }

    /// Hosts a seed URL pointed at. `TrustRank`'s trust root.
    pub fn seeds(&self) -> impl Iterator<Item = u32> + '_ {
        self.seeds.iter().copied()
    }

    /// Total edges, counting each ordered host pair once.
    pub fn edges(&self) -> usize {
        self.nodes.iter().map(HostNode::out_degree).sum()
    }

    fn intern(&mut self, host: &str) -> u32 {
        if let Some(&id) = self.ids.get(host) {
            return id;
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a crawl cannot hold four billion distinct hosts"
        )]
        let id = self.nodes.len() as u32;
        self.names.push(host.to_string());
        self.nodes.push(HostNode::default());
        self.ids.insert(host.to_string(), id);
        id
    }

    /// Build the graph from a crawl store.
    ///
    /// `seed_urls` are the crawl's own seeds; their hosts become the trust
    /// root. Passing none is allowed and leaves `TrustRank` with nothing to
    /// propagate, which [`crate::authority`] reports rather than papers over.
    pub fn build(store: &mut StoreReader, seed_urls: &[String]) -> Result<Self, StoreError> {
        let mut graph = Self::default();

        for url in seed_urls {
            if let Some(host) = host::node(url) {
                let id = graph.intern(&host);
                graph.seeds.insert(id);
            }
        }

        for record in store.records()? {
            let record = record?;
            let Some(from_host) = host::node(&record.final_url) else {
                graph.dropped.unparseable += 1;
                continue;
            };
            let from = graph.intern(&from_host);
            graph.nodes[from as usize].pages += 1;

            // One page's links are deduplicated against each other first, so a
            // page with fifty links to the same host contributes one.
            let mut targets = BTreeSet::new();
            for link in &record.links {
                if link.nofollow {
                    graph.dropped.nofollow += 1;
                    continue;
                }
                let Some(to_host) = host::node(&link.url) else {
                    graph.dropped.unparseable += 1;
                    continue;
                };
                if host::same_site(&from_host, &to_host) {
                    graph.dropped.same_site += 1;
                    continue;
                }
                if !targets.insert(to_host) {
                    graph.dropped.repeat += 1;
                }
            }

            for to_host in targets {
                let to = graph.intern(&to_host);
                *graph.nodes[from as usize].out.entry(to).or_default() += 1;
                *graph.nodes[to as usize].incoming.entry(from).or_default() += 1;
            }
        }

        Ok(graph)
    }
}
