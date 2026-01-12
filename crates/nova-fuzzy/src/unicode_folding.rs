use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

/// Normalize (NFKC) and then apply full Unicode case folding.
///
/// This is shared between the Unicode scorer and trigram preprocessing.
///
/// Note: `unicode-normalization` does not currently expose an `nfkc_casefold()` helper
/// (UAX#15 NFKC_Casefold) nor a streaming case-fold iterator, so we compose `nfkc()`
/// with `unicode-casefold`'s `case_fold()` adaptor.
pub(crate) fn fold_nfkc_casefold(input: &str, out: &mut String) {
    out.clear();
    out.extend(input.nfkc().case_fold());
}
