//! Uruk's web crawler.
//!
//! A robot that visits pages, copies down the readable words, notes the links,
//! and queues those to visit next — politely enough that no site operator has
//! cause to complain.

pub mod frontier;
pub mod parse;
pub mod robots;
pub mod simhash;
pub mod traps;
pub mod url;
