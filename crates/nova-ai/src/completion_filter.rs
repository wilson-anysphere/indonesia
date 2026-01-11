use std::collections::HashSet;

/// Filter completion items whose `insert_text` duplicates a standard completion.
///
/// The caller provides an `insert_text` accessor because different completion
/// representations (Nova internal vs LSP vs AI model output) store the string in
/// different fields.
pub fn filter_duplicates_against_insert_text_set<T, F>(
    items: &mut Vec<T>,
    disallowed_insert_texts: &HashSet<String>,
    mut insert_text: F,
) where
    F: FnMut(&T) -> Option<&str>,
{
    items.retain(|item| {
        insert_text(item)
            .map(|text| !disallowed_insert_texts.contains(text))
            .unwrap_or(true)
    });
}

