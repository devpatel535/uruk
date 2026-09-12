//! Uruk's ranking evaluation.
//!
//! `RESEARCH.md` §5.4 makes the argument this crate exists to answer. The
//! engine refuses click-through data, dwell time and personalisation, which
//! removes the strongest relevance signal anybody has. What replaces it is a
//! curated corpus and **a way to measure a ranking change rather than argue
//! about it** — and the brief put that in Phase 7, which is too late. It
//! should have existed before the first BM25 constant was chosen.
//!
//! It did not, so several decisions in this repository are currently marked
//! "starting value, not tuned": the field weights, `k1`, the per-field `b`,
//! the proximity and quality weights, and — the one that has been deferred
//! twice now — whether in-degree or `TrustRank` is the better authority
//! signal. Every one of those is a question this crate can answer as soon as
//! somebody writes the judgments.
//!
//! Four parts:
//!
//! 1. [`judgments`] is the file format: queries, URLs, grades 0 to 3.
//! 2. [`metrics`] is nDCG and friends, plus the coverage number that says
//!    whether to believe them.
//! 3. [`run`] executes a judged set against a real index under a named
//!    configuration.
//! 4. [`compare`] decides whether two configurations actually differ, by a
//!    paired randomisation test rather than by looking at which mean is
//!    bigger.
//!
//! # What this crate cannot do
//!
//! It cannot write the judgments. Deciding what a good answer to a query looks
//! like is a human judgement about a specific corpus, and it is the part of
//! this engine that cannot be automated — which §5.4 argues is not a
//! limitation but the product.

pub mod compare;
pub mod judgments;
pub mod metrics;
pub mod run;

pub use compare::Comparison;
pub use judgments::Judgments;
pub use metrics::{Scored, Summary};
pub use run::{Configuration, Signal, evaluate};
