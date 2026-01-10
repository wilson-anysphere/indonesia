//! Utilities shared by Nova tests.

use nova_index::TextRange;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Extracts a byte range selection from a fixture containing `/*start*/` and
/// `/*end*/` markers.
///
/// Returns the fixture with markers removed and the selection `TextRange`
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
    assert!(
        end >= after_start,
        "/*end*/ must come after /*start*/"
    );

    let mut text = String::with_capacity(fixture.len());
    text.push_str(&fixture[..start]);
    text.push_str(&fixture[after_start..end]);
    text.push_str(&fixture[end + end_marker.len()..]);

    // Range in the marker-stripped text: the start position stays the same;
    // the end shrinks by the length of the start marker.
    let range = TextRange::new(start, end - start_marker.len());
    (text, range)
}

pub fn load_fixture_dir(dir: &Path) -> BTreeMap<PathBuf, String> {
    fn visit_dir(
        root: &Path,
        dir: &Path,
        out: &mut BTreeMap<PathBuf, String>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit_dir(root, &path, out)?;
            } else {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let text = fs::read_to_string(&path)?;
                out.insert(rel, text);
            }
        }
        Ok(())
    }

    let mut out = BTreeMap::new();
    visit_dir(dir, dir, &mut out).expect("fixture dir readable");
    out
}

pub fn assert_fixture_transformed(
    before: &Path,
    after: &Path,
    mut transform: impl FnMut(&mut BTreeMap<PathBuf, String>),
) {
    let mut files = load_fixture_dir(before);
    transform(&mut files);
    let expected = load_fixture_dir(after);
    assert_eq!(files, expected);
}

