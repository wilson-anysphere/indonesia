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
//!   representation. In Unicode mode, trigrams are taken over the normalized +
//!   casefolded Unicode scalar values; non-ASCII trigrams are hashed into a `u32`
//!   (with the high bit set to avoid collisions with packed ASCII trigrams), so
//!   collisions only introduce false positives during candidate filtering.

#![forbid(unsafe_code)]

mod scoring;
mod trigram;
#[cfg(feature = "unicode")]
mod unicode_folding;

pub use scoring::{fuzzy_match, FuzzyMatcher, MatchKind, MatchScore, RankKey};
pub use trigram::{Trigram, TrigramCandidateScratch, TrigramIndex, TrigramIndexBuilder};

/// Case-insensitive prefix match.
///
/// - Without `feature = "unicode"`, this is ASCII-only case-insensitive matching over UTF-8 bytes.
/// - With `unicode` enabled, this uses NFKC normalization + Unicode case folding and compares
///   by extended grapheme clusters (so expansions like `ß → ss` work as expected).
#[inline]
pub fn prefix_match(query: &str, candidate: &str) -> bool {
    fuzzy_match(query, candidate).is_some_and(|s| s.kind == MatchKind::Prefix)
}

#[cfg(feature = "unicode")]
use unicode_casefold::UnicodeCaseFold;
#[cfg(feature = "unicode")]
use unicode_normalization::UnicodeNormalization;
#[cfg(feature = "unicode")]
use unicode_segmentation::UnicodeSegmentation;

/// Returns the first Unicode scalar value of `input` after NFKC normalization and
/// Unicode case folding.
///
/// This is a small helper for higher-level indexing layers (e.g. prefix buckets)
/// that need to stay consistent with `nova-fuzzy`'s Unicode matching semantics.
#[cfg(feature = "unicode")]
pub fn nfkc_casefold_first_char(input: &str) -> Option<char> {
    if input.is_ascii() {
        return input
            .as_bytes()
            .first()
            .map(|&b0| b0.to_ascii_lowercase() as char);
    }

    input.nfkc().case_fold().next()
}

/// Returns `(first_char, grapheme_len)` for `input` after NFKC normalization and
/// Unicode case folding.
///
/// The grapheme count is computed over the folded string using extended grapheme
/// clusters (the same scoring unit used by the Unicode fuzzy matcher).
#[cfg(feature = "unicode")]
pub fn nfkc_casefold_first_char_and_grapheme_len(
    input: &str,
    buf: &mut String,
) -> (Option<char>, usize) {
    if input.is_ascii() {
        let bytes = input.as_bytes();
        let first = bytes.first().map(|&b0| b0.to_ascii_lowercase() as char);
        return (first, bytes.len());
    }

    unicode_folding::fold_nfkc_casefold(input, buf);
    let first = buf.chars().next();
    let grapheme_len = buf.graphemes(true).count();
    (first, grapheme_len)
}
