//! Uruk's inverted index.
//!
//! Takes the text the crawler saved and flips it inside out. Instead of "page
//! 4 contains these words", the index stores "this word appears on pages 4, 9
//! and 12". That flip is the reason a search takes milliseconds instead of
//! reading every document.

pub mod build;
pub mod fields;
pub mod index;
pub mod postings;
pub mod segment;
pub mod tokenize;
