#[cfg(feature = "unicode")]
use unicode_normalization::UnicodeNormalization;

/// Apply NFKC normalization and a Unicode-aware case folding.
///
/// `unicode-normalization` provides NFKC, but does not expose an `nfkc_casefold()` helper
/// (UAX#15 NFKC_Casefold). To avoid an extra optional dependency, we approximate Unicode
/// case folding by composing Rust's built-in Unicode case mappings:
///
/// `ch.to_uppercase().to_lowercase()`
///
/// This is locale-independent, supports multi-character expansions (e.g. `ß → ss`), and
/// avoids context-sensitive lowercasing behaviors like Greek final sigma.
///
/// Known special cases:
/// - U+0131 (LATIN SMALL LETTER DOTLESS I) must remain distinct from ASCII `i`.
/// - U+1E9E (LATIN CAPITAL LETTER SHARP S) should fold to `ss`.
pub(crate) fn fold_nfkc_casefold(input: &str, out: &mut String) {
    out.clear();
    out.reserve(input.len());

    for ch in input.nfkc() {
        match ch {
            // Keep dotless i distinct; naive upper->lower mapping turns it into 'i'.
            '\u{0131}' => out.push('\u{0131}'),
            // Ensure capital sharp s folds like ß: `ẞ` → `ss`.
            '\u{1E9E}' => {
                out.push('s');
                out.push('s');
            }
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
