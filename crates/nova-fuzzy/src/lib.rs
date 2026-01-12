//! Fast fuzzy matching primitives used throughout Nova.
//!
//! The design combines a trigram index for candidate filtering with a fuzzy
//! subsequence scorer for ranking.

#![forbid(unsafe_code)]

mod scoring;
mod trigram;

pub use scoring::{fuzzy_match, FuzzyMatcher, MatchKind, MatchScore, RankKey};
pub use trigram::{Trigram, TrigramCandidateScratch, TrigramIndex, TrigramIndexBuilder};
