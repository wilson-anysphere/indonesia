#[cfg(feature = "unicode")]
use unicode_normalization::UnicodeNormalization;

/// Apply NFKC normalization and a Unicode-aware case folding.
///
/// `unicode-normalization` provides normalization but does not expose an `nfkc_casefold()`
/// helper (UAX#15 NFKC_Casefold). To keep the Unicode feature self-contained, we build the
/// folding step from Rust's built-in Unicode case mappings:
///
/// `ch.to_uppercase().to_lowercase()`
///
/// This supports multi-character expansions (e.g. `ß → ss`) and keeps folding locale
/// independent (e.g. dotless `ı` stays distinct from ASCII `i`).
pub(crate) fn fold_nfkc_casefold(input: &str, out: &mut String) {
    out.clear();
    out.reserve(input.len());

    for ch in input.nfkc() {
        match ch {
            // Keep dotless i distinct; naive upper->lower mapping turns it into ASCII 'i'.
            '\u{0131}' => out.push('\u{0131}'),
            // Ensure capital sharp s folds like ß: `ẞ` → `ss`.
            '\u{1E9E}' => out.push_str("ss"),
            _ => {
                for uc in ch.to_uppercase() {
                    for lc in uc.to_lowercase() {
                        out.push(lc);
                    }
                }
            }
        }
    }
}

