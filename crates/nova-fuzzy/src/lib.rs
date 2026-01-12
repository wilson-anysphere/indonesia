//! Fast fuzzy matching primitives used throughout Nova.
//!
//! The design combines a trigram index for candidate filtering with a fuzzy
//! subsequence scorer for ranking.
//!
//! ## Unicode support (`feature = "unicode"`)
//!
//! By default, `nova-fuzzy` performs **ASCII-only** case-insensitive matching.
//! Inputs are treated as raw UTF-8 bytes and only `A-Z`/`a-z` are folded; any
//! non-ASCII bytes must match exactly.
//!
//! Enable the `unicode` Cargo feature to make matching **Unicode-aware** for
//! non-ASCII inputs. In this mode:
//!
//! - **Normalization + case folding:** both the query and candidate are first
//!   normalized with Unicode **NFKC** and then **case folded** (not just
//!   `to_lowercase`), so case-insensitive matches work across scripts and handle
//!   expansions like `ß → ss`. This makes matches like `"strasse"` ⇔ `"Straße"` and
//!   composed/decomposed accent forms behave as users expect.
//! - **Scoring unit:** the fuzzy scorer operates on **extended grapheme clusters**
//!   (via the `unicode-segmentation` crate), not bytes. This keeps multi-codepoint emoji
//!   sequences stable as a single unit.
//! - **ASCII fast path:** even with `unicode` enabled, if both `query` and
//!   `candidate` are ASCII, the existing byte-based implementation is used
//!   unchanged.
//! - **Trigram preprocessing:** trigram extraction applies the same
//!   NFKC+casefold transform. ASCII trigrams keep the existing packed 3-byte
//!   representation; non-ASCII trigrams are hashed into a `u32` (with the high bit
//!   set to avoid collisions with packed ASCII trigrams).

#![forbid(unsafe_code)]

mod scoring;
mod trigram;

pub use scoring::{fuzzy_match, FuzzyMatcher, MatchKind, MatchScore, RankKey};
pub use trigram::{Trigram, TrigramCandidateScratch, TrigramIndex, TrigramIndexBuilder};
