use nova_index::TextRange;

/// Extracts a byte range selection from a fixture containing `/*start*/` and
/// `/*end*/` markers.
///
/// Returns the fixture with markers removed and the selection [`TextRange`]
/// pointing at the extracted region.
pub fn extract_range(fixture: &str) -> (String, TextRange) {
    let start_marker = "/*start*/";
    let end_marker = "/*end*/";

    let start = fixture
        .find(start_marker)
        .expect("fixture missing /*start*/ marker");
    let after_start = start + start_marker.len();
    let end = fixture
        .find(end_marker)
        .expect("fixture missing /*end*/ marker");
    assert!(end >= after_start, "/*end*/ must come after /*start*/");

    let mut text = String::with_capacity(fixture.len());
    text.push_str(&fixture[..start]);
    text.push_str(&fixture[after_start..end]);
    text.push_str(&fixture[end + end_marker.len()..]);

    // Range in the marker-stripped text: the start position stays the same;
    // the end shrinks by the length of the start marker.
    let range = TextRange::new(start, end - start_marker.len());
    (text, range)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_range_handles_multibyte_chars() {
        let input = "a/*start*/α😃β/*end*/c";
        let (text, range) = extract_range(input);

        assert_eq!(text, "aα😃βc");
        assert_eq!(&text[range.start..range.end], "α😃β");
    }
}
