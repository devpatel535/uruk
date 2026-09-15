//! Uruk's link graph and host authority.
//!
//! A link is a person choosing to point at someone else's page. Counted
//! carefully it is the only evidence this engine has about a site's standing —
//! there is no click data, no dwell time and no personalisation to fall back
//! on, by design (`RESEARCH.md` §5.4).
//!
//! Counted carelessly it is the easiest signal on the web to fake, which is
//! why most of the thinking in this crate is about which links *don't* count.
//!
//! Three steps, in three modules:
//!
//! 1. [`host`] decides when two URLs belong to the same site, so a site cannot
//!    vote for itself.
//! 2. [`graph`] turns a crawl into one edge per ordered pair of hosts,
//!    discarding `nofollow`, self-links and repeats.
//! 3. [`authority`] scores hosts two ways — in-degree and `TrustRank` — and
//!    defaults to the cheaper one until there is evidence for the other.

pub mod authority;
pub mod graph;
pub mod host;
pub mod suffix;

pub use authority::{Authority, Method};
pub use graph::HostGraph;
