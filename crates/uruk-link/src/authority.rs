//! How much a host's links are worth, by two methods that disagree.
//!
//! `RESEARCH.md` §5.3 sets the rule this module follows: **in-degree is the
//! baseline, and anything fancier has to beat it on a judged query set before
//! it ships.** That is not scepticism for its own sake — Najork, Zaragoza and
//! Taylor (2007) found BM25F plus simple in-degree outperformed BM25F plus
//! `PageRank` or HITS on a full web graph, which is the opposite of what the
//! folklore says.
//!
//! So both are computed, both are stored, and **in-degree is the default**.
//! The judged query set that would settle it is Phase 9 and does not exist
//! yet; until it does, the burden of proof sits with the expensive method.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::graph::HostGraph;

/// The file an authority table is written to, inside a crawl directory.
///
/// `search` and `serve` look here by default, so computing authority is a
/// matter of running `uruk link` rather than of remembering a path.
pub const FILE_NAME: &str = "authority.json";

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("could not read or write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a valid authority table: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Damping: the probability a random surfer follows a link rather than
/// jumping back to a trusted seed. 0.85 is Page and Brin's figure and the one
/// every subsequent paper compares against.
pub const DEFAULT_DAMPING: f64 = 0.85;

/// Hard ceiling on power iterations, so a pathological graph cannot hang a
/// build. This is a backstop, not the operating limit: [`iterations_needed`]
/// works out how many steps the chosen damping actually requires, and that is
/// always the smaller number for any sane damping.
pub const MAX_ITERATIONS: usize = 2_000;

/// Stop when the total absolute change across all hosts falls below this.
pub const CONVERGENCE: f64 = 1e-9;

/// How many iterations reaching [`CONVERGENCE`] takes at a given damping.
///
/// Power iteration on a stochastic matrix contracts by a factor of the damping
/// each step, so the residual after *n* steps is about `damping^n`, and reaching
/// `ε` needs `ln(ε) / ln(damping)` steps. At the default 0.85 that is 128.
///
/// This is arithmetic rather than a guess, and it is written down because
/// getting it wrong is silent: an iteration cap below this number produces
/// scores that are close enough to look right while `converged` is never true.
/// That happened here — the cap was 100 against a requirement of 128 — and the
/// only reason it was caught is that a test asserted convergence rather than
/// assuming it.
pub fn iterations_needed(damping: f64) -> usize {
    if !(0.0..1.0).contains(&damping) || damping == 0.0 {
        return 1;
    }
    // The first step can move the residual by up to 2 (the whole distribution
    // moving), so start the contraction from there rather than from 1.
    let steps = (CONVERGENCE / 2.0).ln() / damping.ln();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "steps is positive and far below usize::MAX for any damping < 1"
    )]
    let needed = steps.ceil() as usize;
    needed.saturating_add(1).min(MAX_ITERATIONS)
}

/// Which signal the ranker should use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    /// Distinct hosts linking here. The baseline, and the default: see the
    /// module docs for why the default is the cheap one.
    #[default]
    InDegree,
    /// Trust propagated from the crawl's seeds, `TrustRank`-style.
    TrustRank,
}

/// What one host scored, under both methods.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct HostScore {
    /// Pages crawled from this host.
    pub pages: u32,
    /// Distinct hosts linking here.
    pub in_degree: u32,
    /// Trust propagated from the seeds. Sums to 1 across all hosts.
    pub trust: f64,
}

/// How the power iteration went. Reported rather than assumed.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Convergence {
    pub iterations: usize,
    pub residual: f64,
    pub converged: bool,
    /// True when the crawl had no seed hosts, so trust had to start uniform.
    /// `TrustRank` without a trust root is just `PageRank`, and saying so is more
    /// useful than silently producing numbers that look the same.
    pub trust_root_was_empty: bool,
}

/// The authority table: what gets written next to a crawl and read by the
/// ranker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Authority {
    pub method: Method,
    pub hosts: BTreeMap<String, HostScore>,
    pub convergence: Convergence,
    /// Largest in-degree in the crawl, kept so the ranker can normalise
    /// without re-reading every host.
    pub max_in_degree: u32,
}

impl Authority {
    /// Compute both signals over `graph`.
    pub fn compute(graph: &HostGraph, method: Method, damping: f64) -> Self {
        let trust = trust_rank(graph, damping);
        let mut hosts = BTreeMap::new();
        let mut max_in_degree = 0;
        for (id, node) in graph.nodes() {
            let in_degree = u32::try_from(node.in_degree()).unwrap_or(u32::MAX);
            max_in_degree = max_in_degree.max(in_degree);
            hosts.insert(
                graph.name(id).to_string(),
                HostScore {
                    pages: node.pages,
                    in_degree,
                    trust: trust.scores[id as usize],
                },
            );
        }
        Self {
            method,
            hosts,
            convergence: trust.convergence,
            max_in_degree,
        }
    }

    /// A host's authority in `0.0..=1.0`, by the configured method.
    ///
    /// The host is normalised with [`crate::host::node`] first. That matters:
    /// the index stores hosts as the crawler saw them, so it holds
    /// `www.example.com`, while the graph collapsed that to `example.com` when
    /// it was built. Looking up the raw string would miss every www host and
    /// score it zero, and the failure would be invisible — a plausible ranking
    /// with one signal quietly switched off for a subset of the corpus.
    ///
    /// An unknown host scores zero rather than an average: a host we have
    /// never seen linked has no evidence for it, and inventing a middling
    /// score for it would make "not crawled" indistinguishable from "crawled
    /// and ignored by everyone".
    pub fn score(&self, host: &str) -> f64 {
        let normalised = crate::host::node(host);
        let key = normalised.as_deref().unwrap_or(host);
        let Some(entry) = self.hosts.get(key) else {
            return 0.0;
        };
        match self.method {
            Method::InDegree => damp(f64::from(entry.in_degree), f64::from(self.max_in_degree)),
            Method::TrustRank => {
                let top = self
                    .hosts
                    .values()
                    .map(|score| score.trust)
                    .fold(0.0_f64, f64::max);
                damp(entry.trust, top)
            }
        }
    }

    /// Write the table as JSON.
    ///
    /// JSON rather than a packed binary, and pretty-printed, for the same
    /// reason the index manifest is: somebody will one day want to know why a
    /// host ranked where it did, and `less` should be enough to find out. The
    /// file is a few dozen bytes per host, so the cost is real but small
    /// against a crawl store measured in gigabytes.
    pub fn write(&self, path: &Path) -> Result<(), AuthorityError> {
        let json =
            serde_json::to_string_pretty(self).map_err(|source| AuthorityError::Malformed {
                path: path.to_path_buf(),
                source,
            })?;
        std::fs::write(path, json).map_err(|source| AuthorityError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Read a table written by [`write`](Self::write).
    pub fn load(path: &Path) -> Result<Self, AuthorityError> {
        let raw = std::fs::read_to_string(path).map_err(|source| AuthorityError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&raw).map_err(|source| AuthorityError::Malformed {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Read the table from a crawl directory, if one has been computed.
    ///
    /// `Ok(None)` when the file simply is not there — searching an index
    /// before `uruk link` has ever run is a normal thing to do, not an error.
    /// A file that exists but will not parse *is* an error, because silently
    /// ranking without a signal the operator thinks is switched on is worse
    /// than stopping.
    pub fn beside_crawl(crawl_dir: &Path) -> Result<Option<Self>, AuthorityError> {
        let path = crawl_dir.join(FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        Self::load(&path).map(Some)
    }

    /// Hosts ranked by the configured method, best first.
    pub fn ranking(&self) -> Vec<(&str, f64)> {
        let mut ranked: Vec<(&str, f64)> = self
            .hosts
            .keys()
            .map(|host| (host.as_str(), self.score(host)))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        ranked
    }
}

/// Compress a raw count or mass into `0.0..=1.0`, logarithmically.
///
/// Linear normalisation would give the single most-linked host a score of 1
/// and everything else something near zero, which is not a ranking signal so
/// much as a single bit. Authority differences are multiplicative — the gap
/// between 1 and 10 in-links means far more than the gap between 991 and
/// 1000 — so the log is the shape that matches what the number means.
fn damp(value: f64, max: f64) -> f64 {
    if max <= 0.0 || value <= 0.0 {
        return 0.0;
    }
    (value.ln_1p() / max.ln_1p()).clamp(0.0, 1.0)
}

struct TrustResult {
    scores: Vec<f64>,
    convergence: Convergence,
}

/// `TrustRank`: `PageRank` with the random jump biased towards hand-picked seeds.
///
/// Gyöngyi, Garcia-Molina and Pedersen (2004). The only difference from
/// `PageRank` is the vector the surfer teleports to: uniform in `PageRank`, the
/// seed set here. That one change is what makes the score expensive to attack,
/// because a new host gains trust only by being reachable from something we
/// already chose to trust.
///
/// Dangling hosts — crawled, but with no outward links that survived the
/// filters in [`crate::graph`] — have their mass teleported back to the seed
/// vector rather than spread uniformly. Spreading it uniformly is the common
/// shortcut and it quietly turns `TrustRank` back into `PageRank`, because on a
/// real crawl most hosts are dangling: we saw them linked, but never fetched
/// them.
fn trust_rank(graph: &HostGraph, damping: f64) -> TrustResult {
    let n = graph.len();
    if n == 0 {
        return TrustResult {
            scores: Vec::new(),
            convergence: Convergence::default(),
        };
    }

    let seeds: Vec<u32> = graph.seeds().collect();
    let trust_root_was_empty = seeds.is_empty();
    let mut teleport = vec![0.0; n];
    if trust_root_was_empty {
        teleport.fill(1.0 / n as f64);
    } else {
        let share = 1.0 / seeds.len() as f64;
        for &seed in &seeds {
            teleport[seed as usize] = share;
        }
    }

    let mut scores = teleport.clone();
    let mut next = vec![0.0; n];
    let mut convergence = Convergence {
        trust_root_was_empty,
        ..Convergence::default()
    };

    for iteration in 1..=iterations_needed(damping) {
        next.copy_from_slice(&teleport);
        for slot in &mut next {
            *slot *= 1.0 - damping;
        }

        // Mass that has nowhere to go: hosts with no surviving out-edges.
        let mut dangling = 0.0;
        for (id, node) in graph.nodes() {
            let mass = scores[id as usize];
            let degree = node.out_degree();
            if degree == 0 {
                dangling += mass;
                continue;
            }
            // Every out-edge carries the same weight regardless of how many
            // pages made the link. A site cannot buy influence by repeating
            // itself; see `graph`.
            let share = damping * mass / degree as f64;
            for &target in node.out.keys() {
                next[target as usize] += share;
            }
        }
        for (slot, &share) in next.iter_mut().zip(&teleport) {
            *slot += damping * dangling * share;
        }

        let residual: f64 = next
            .iter()
            .zip(&scores)
            .map(|(after, before)| (after - before).abs())
            .sum();
        std::mem::swap(&mut scores, &mut next);
        convergence.iterations = iteration;
        convergence.residual = residual;
        if residual < CONVERGENCE {
            convergence.converged = true;
            break;
        }
    }

    TrustResult {
        scores,
        convergence,
    }
}

/// How much two rankings of the same hosts disagree.
///
/// Spearman's rank correlation over the hosts both rank: `1.0` is perfect
/// agreement, `0.0` none, `-1.0` exact opposition. This exists because
/// §5.3 says in-degree is the baseline to beat, and "beat" needs a way to see
/// whether the two methods are even producing different answers. If they agree
/// almost perfectly, the expensive one is not worth its iterations whatever a
/// judged query set later says.
pub fn rank_correlation(left: &[(&str, f64)], right: &[(&str, f64)]) -> Option<f64> {
    let n = left.len();
    if n != right.len() || n < 2 {
        return None;
    }
    let position: BTreeMap<&str, usize> = right
        .iter()
        .enumerate()
        .map(|(rank, (host, _))| (*host, rank))
        .collect();

    let mut sum_squared_difference = 0.0;
    for (rank, (host, _)) in left.iter().enumerate() {
        let other = *position.get(host)?;
        let difference = rank as f64 - other as f64;
        sum_squared_difference += difference * difference;
    }
    let n = n as f64;
    Some(1.0 - (6.0 * sum_squared_difference) / (n * (n * n - 1.0)))
}
