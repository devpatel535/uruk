//! Uruk's query engine.
//!
//! Someone types words; we look each one up in the index, keep the pages that
//! have all of them, and put those in order. Every result can say why it
//! ranked where it did, because tuning a search engine from user feedback is
//! guesswork otherwise.

pub mod parse;
pub mod score;
pub mod search;
pub mod snippet;
