//! Fast fuzzy matching primitives used throughout Nova.
//!
//! The design combines a trigram index for candidate filtering with a fuzzy
//! subsequence scorer for ranking.
//!
//! ## Unicode support
//!
//! By default, `nova-fuzzy` performs **ASCII-only** case-insensitive matching.
//! Inputs are treated as raw UTF-8 bytes and only `A-Z`/`a-z` are folded; any
//! non-ASCII bytes must match exactly.
//!
//! Enable the `unicode` feature to make matching **Unicode-aware**. In this
//! mode, both the query and candidate are:
//!
//! 1. Normalized to Unicode **NFKC** (compatibility decomposition + canonical
//!    composition).
//! 2. Unicode **case folded** (not just `to_lowercase`), so that case-insensitive
//!    matches work across scripts and handle expansions like `ß → ss`.
//!
//! This makes matches like `"strasse"` ⇔ `"Straße"` and composed/decomposed
//! accent forms behave as users expect.
//!
//! Even with `unicode` enabled, purely ASCII inputs take a fast path that avoids
//! any Unicode normalization/case folding.

#![forbid(unsafe_code)]

mod scoring;
mod trigram;

pub use scoring::{fuzzy_match, FuzzyMatcher, MatchKind, MatchScore, RankKey};
pub use trigram::{Trigram, TrigramCandidateScratch, TrigramIndex, TrigramIndexBuilder};
