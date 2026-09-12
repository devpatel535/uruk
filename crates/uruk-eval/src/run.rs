//! Running a judged query set against a real index.
//!
//! Deliberately not a mock. The harness exists to catch ranking regressions,
//! and a regression lives in the interaction between the parser, the posting
//! lists, the scorer and the authority table. An evaluation that stubs any of
//! those out is evaluating the stub.

use uruk_crawl::store::StoreReader;
use uruk_index::index::Index;
use uruk_link::{Authority, Method};
use uruk_query::parse;
use uruk_query::score::Weights;
use uruk_query::search::{SearchOptions, search};

use crate::judgments::Judgments;
use crate::metrics::{DEFAULT_DEPTH, Scored, Summary, score};

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("search failed on {query:?}: {source}")]
    Search {
        query: String,
        #[source]
        source: uruk_query::search::QueryError,
    },
    #[error("could not read document {doc} from the crawl store: {source}")]
    Store {
        doc: u32,
        #[source]
        source: uruk_crawl::store::StoreError,
    },
}

/// One ranking configuration to evaluate.
///
/// This is what an A/B is *between*. Everything here is a starting value that
/// `RESEARCH.md` §5.4 says must not be tuned by eye — which is precisely what
/// this crate makes unnecessary.
#[derive(Debug, Clone)]
pub struct Configuration {
    pub name: String,
    pub weights: Weights,
    /// Authority table and the method to read it by. `None` ranks on text,
    /// proximity and quality alone.
    pub authority: Option<Authority>,
    pub depth: usize,
}

impl Configuration {
    /// The engine as it ships.
    pub fn baseline(authority: Option<Authority>) -> Self {
        Self {
            name: String::from("baseline"),
            weights: Weights::default(),
            authority,
            depth: DEFAULT_DEPTH,
        }
    }

    /// The same configuration with one signal switched off.
    ///
    /// This is the only kind of variant worth building in by name: "what does
    /// this signal actually buy?" is the question a harness answers well, and
    /// the answer is the difference between the engine with it and without.
    #[must_use]
    pub fn without(&self, signal: Signal) -> Self {
        let mut variant = self.clone();
        variant.name = format!("no {}", signal.name());
        match signal {
            Signal::Authority => variant.authority = None,
            Signal::Proximity => variant.weights.proximity = 0.0,
            Signal::Quality => variant.weights.quality = 0.0,
        }
        variant
    }

    /// The same configuration reading authority by a different method.
    #[must_use]
    pub fn using(&self, method: Method) -> Self {
        let mut variant = self.clone();
        variant.name = match method {
            Method::InDegree => String::from("authority by in-degree"),
            Method::TrustRank => String::from("authority by trustrank"),
        };
        if let Some(authority) = &mut variant.authority {
            authority.method = method;
        }
        variant
    }
}

/// A signal that can be switched off to see what it was worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Authority,
    Proximity,
    Quality,
}

impl Signal {
    pub fn name(self) -> &'static str {
        match self {
            Self::Authority => "authority",
            Self::Proximity => "proximity",
            Self::Quality => "quality",
        }
    }

    pub const ALL: [Self; 3] = [Self::Authority, Self::Proximity, Self::Quality];
}

/// Per-query scores plus their means.
#[derive(Debug, Clone)]
pub struct Run {
    pub configuration: String,
    pub scores: Vec<Scored>,
    pub summary: Summary,
}

/// Run every judged query and score the rankings.
pub fn evaluate(
    index: &mut Index,
    store: &mut StoreReader,
    judgments: &Judgments,
    configuration: &Configuration,
) -> Result<Run, EvalError> {
    let mut scores = Vec::with_capacity(judgments.len());

    for judged in &judgments.queries {
        let query = parse::parse(&judged.query);
        let options = SearchOptions {
            limit: configuration.depth,
            weights: configuration.weights,
            authority: configuration.authority.as_ref(),
        };
        let results = search(index, &query, &options).map_err(|source| EvalError::Search {
            query: judged.query.clone(),
            source,
        })?;

        // Judgments are written against URLs, because a URL is what a person
        // can look at. Document ids are an implementation detail that changes
        // whenever the corpus is recrawled, so a judged set keyed on them
        // would expire silently.
        let mut ranked = Vec::with_capacity(results.hits.len());
        for hit in &results.hits {
            let record = store
                .get(hit.crawl_doc)
                .map_err(|source| EvalError::Store {
                    doc: hit.crawl_doc,
                    source,
                })?;
            // A page can be judged under the URL that was linked or the one it
            // redirected to. Prefer whichever the judge actually wrote, so a
            // judged set does not silently stop matching when a site adds a
            // redirect.
            let url = if judged.grades.contains_key(&record.final_url) {
                record.final_url
            } else {
                record.url
            };
            ranked.push(url);
        }

        scores.push(score(judged, &ranked, configuration.depth));
    }

    let summary = Summary::of(&scores);
    Ok(Run {
        configuration: configuration.name.clone(),
        scores,
        summary,
    })
}
