use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::{Position, Uri};
use nova_ide::Database;
use nova_index::TextRange;

fn file_uri_for_fixture_path(path: &str) -> Uri {
    let path = path.trim();

    // Fixtures typically use Rust Analyzer-style absolute paths like `/Main.java`.
    // Be lenient and treat relative paths as workspace-absolute as well.
    let mut normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    // Fixtures might use Windows-style separators even when tests run on Unix.
    normalized = normalized.replace('\\', "/");

    // Percent-encode the path so it can always be parsed as a `file:` URI.
    // (E.g. spaces and `#` must be encoded to avoid being treated as URL syntax.)
    let encoded = encode_uri_path(&normalized);
    Uri::from_str(&format!("file://{encoded}")).expect("fixture path should form a valid file URI")
}

fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.as_bytes() {
        let b = *b;
        if is_uri_unreserved(b) || b == b'/' || b == b':' {
            out.push(b as char);
        } else {
            push_pct_encoded(&mut out, b);
        }
    }
    out
}

fn is_uri_unreserved(b: u8) -> bool {
    matches!(
        b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~'
    )
}

fn push_pct_encoded(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0x0F) as usize] as char);
}

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

/// Load a fixture directory into a `(relative_path -> text)` map.
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

    if !after.exists() {
        if bless_enabled() {
            bless_fixture_dir(after, &files);
            return;
        }
        panic!(
            "missing expected fixture dir {} (run with `BLESS=1` to write it)",
            after.display()
        );
    }

    let expected = load_fixture_dir(after);
    if files != expected {
        if bless_enabled() {
            bless_fixture_dir(after, &files);
            return;
        }
        assert_eq!(files, expected);
    }
}

fn bless_enabled() -> bool {
    let Ok(val) = env::var("BLESS") else {
        return false;
    };
    let val = val.trim().to_ascii_lowercase();
    !(val.is_empty() || val == "0" || val == "false")
}

fn bless_fixture_dir(dir: &Path, files: &BTreeMap<PathBuf, String>) {
    if dir.exists() {
        fs::remove_dir_all(dir).unwrap_or_else(|err| {
            panic!(
                "failed to remove existing fixture dir {}: {err}",
                dir.display()
            )
        });
    }
    fs::create_dir_all(dir)
        .unwrap_or_else(|err| panic!("failed to create fixture dir {}: {err}", dir.display()));

    for (rel, text) in files {
        assert!(
            rel.components()
                .all(|c| !matches!(c, std::path::Component::ParentDir)),
            "fixture paths must not contain '..': {}",
            rel.display()
        );
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|err| {
                panic!("failed to create fixture dir {}: {err}", parent.display())
            });
        }
        fs::write(&path, text)
            .unwrap_or_else(|err| panic!("failed to write fixture {}: {err}", path.display()));
    }
}

/// A minimal multi-file fixture with `$0`, `$1`, ... markers.
///
/// Marker IDs must be unique across the entire fixture; duplicate IDs will
/// panic during parsing.
pub struct Fixture {
    pub db: Database,
    files: HashMap<Uri, String>,
    markers: HashMap<u32, (Uri, usize)>,
}

impl Fixture {
    #[must_use]
    pub fn parse(fixture: &str) -> Self {
        let mut current_path: Option<String> = None;
        let mut current_text = String::new();
        let mut files: Vec<(Uri, String)> = Vec::new();

        for line in fixture.lines() {
            if let Some(rest) = line.strip_prefix("//-") {
                if let Some(path) = current_path.take() {
                    let uri = file_uri_for_fixture_path(&path);
                    files.push((uri, current_text));
                    current_text = String::new();
                }

                current_path = Some(rest.trim().to_string());
                continue;
            }

            if !current_text.is_empty() {
                current_text.push('\n');
            }
            current_text.push_str(line);
        }

        if let Some(path) = current_path.take() {
            let uri = file_uri_for_fixture_path(&path);
            files.push((uri, current_text));
        }

        let mut markers: HashMap<u32, (Uri, usize)> = HashMap::new();
        let mut file_texts: HashMap<Uri, String> = HashMap::new();
        let mut db = Database::new();

        for (uri, text) in files {
            let (text, file_markers) = strip_markers(&text);
            file_texts.insert(uri.clone(), text.clone());
            for (id, offset) in file_markers {
                if let Some((prev_uri, prev_offset)) = markers.insert(id, (uri.clone(), offset)) {
                    panic!(
                        "duplicate fixture marker ${id} (first at {prev_uri:?}:{prev_offset}, again at {uri:?}:{offset})"
                    );
                }
            }
            db.set_file_content(uri, text);
        }

        Self {
            db,
            files: file_texts,
            markers,
        }
    }

    #[must_use]
    pub fn marker_uri(&self, id: u32) -> Uri {
        self.markers.get(&id).unwrap().0.clone()
    }

    #[must_use]
    pub fn marker_position(&self, id: u32) -> Position {
        let (uri, offset) = self.markers.get(&id).unwrap();
        let text = self.files.get(uri).unwrap();
        offset_to_position(text, *offset)
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn marker_offset(&self, id: u32) -> usize {
        self.markers.get(&id).unwrap().1
    }

    #[allow(dead_code)]
    pub fn offset_for_position(&self, uri: &Uri, position: Position) -> Option<usize> {
        let text = self.files.get(uri)?;
        position_to_offset(text, position)
    }
}

fn position_to_offset(text: &str, position: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut offset: usize = 0;

    for ch in text.chars() {
        if line == position.line && col_utf16 == position.character {
            return Some(offset);
        }

        offset += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    if line == position.line && col_utf16 == position.character {
        Some(offset)
    } else {
        None
    }
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut cur: usize = 0;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    Position {
        line,
        character: col_utf16,
    }
}

fn strip_markers(text: &str) -> (String, Vec<(u32, usize)>) {
    let mut out = String::with_capacity(text.len());
    let mut markers = Vec::new();

    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut last = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }

            if j > i + 1 {
                // Safe slicing: `$` and ASCII digits are always UTF-8 single-byte
                // codepoints, so `i` and `j` are valid UTF-8 boundaries.
                out.push_str(&text[last..i]);
                let id: u32 = text[i + 1..j].parse().unwrap();
                markers.push((id, out.len()));
                i = j;
                last = j;
                continue;
            }
        }

        i += 1;
    }

    out.push_str(&text[last..]);

    (out, markers)
}

#[cfg(test)]
mod tests {
    use super::*;

    use lsp_types::Position;
    use std::str::FromStr;

    #[test]
    fn strip_markers_preserves_unicode_and_offsets() {
        let input = "α$0😃β$10";
        let (text, markers) = strip_markers(input);

        assert_eq!(text, "α😃β");
        assert_eq!(markers, vec![(0, "α".len()), (10, "α😃β".len())]);
    }

    #[test]
    fn strip_markers_keeps_invalid_dollar_sequences() {
        // `$x` and `$` at EOF are not markers and should be preserved.
        let input = "a$x$0b$";
        let (text, markers) = strip_markers(input);

        assert_eq!(text, "a$xb$");
        assert_eq!(markers, vec![(0, 3)]);
    }

    #[test]
    fn fixture_marker_position_uses_utf16_columns() {
        let fixture = Fixture::parse("//- /main.txt\na\n😃$0b");
        let uri = fixture.marker_uri(0);

        assert_eq!(fixture.marker_offset(0), "a\n😃".len());
        assert_eq!(
            fixture.marker_position(0),
            Position {
                line: 1,
                character: 2
            }
        );

        let roundtrip = fixture
            .offset_for_position(&uri, fixture.marker_position(0))
            .unwrap();
        assert_eq!(roundtrip, fixture.marker_offset(0));
    }

    #[test]
    fn extract_range_handles_multibyte_chars() {
        let input = "a/*start*/α😃β/*end*/c";
        let (text, range) = extract_range(input);

        assert_eq!(text, "aα😃βc");
        assert_eq!(&text[range.start..range.end], "α😃β");
    }

    #[test]
    fn fixture_multi_file_multi_marker_unicode() {
        let fixture = Fixture::parse("//- /a.txt\nα$0β$10\n//- /b.txt\n$1😃$2");

        let a_uri = Uri::from_str("file:///a.txt").unwrap();
        let b_uri = Uri::from_str("file:///b.txt").unwrap();

        assert_eq!(fixture.files.get(&a_uri).unwrap(), "αβ");
        assert_eq!(fixture.files.get(&b_uri).unwrap(), "😃");

        assert_eq!(fixture.marker_uri(0), a_uri);
        assert_eq!(
            fixture.marker_uri(10),
            Uri::from_str("file:///a.txt").unwrap()
        );
        assert_eq!(fixture.marker_uri(1), b_uri);
        assert_eq!(
            fixture.marker_uri(2),
            Uri::from_str("file:///b.txt").unwrap()
        );

        assert_eq!(fixture.marker_offset(0), "α".len());
        assert_eq!(fixture.marker_offset(10), "αβ".len());
        assert_eq!(fixture.marker_offset(1), 0);
        assert_eq!(fixture.marker_offset(2), "😃".len());

        assert_eq!(
            fixture.marker_position(0),
            Position {
                line: 0,
                character: 1
            }
        );
        assert_eq!(
            fixture.marker_position(10),
            Position {
                line: 0,
                character: 2
            }
        );
        assert_eq!(
            fixture.marker_position(1),
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            fixture.marker_position(2),
            Position {
                line: 0,
                character: 2
            }
        );

        for id in [0u32, 10, 1, 2] {
            let uri = fixture.marker_uri(id);
            let pos = fixture.marker_position(id);
            let off = fixture.offset_for_position(&uri, pos).unwrap();
            assert_eq!(off, fixture.marker_offset(id));
        }
    }

    #[test]
    #[should_panic(expected = "duplicate fixture marker $0")]
    fn duplicate_marker_ids_panic() {
        let _ = Fixture::parse("//- /a.txt\n$0\n//- /b.txt\n$0");
    }

    #[test]
    fn fixture_paths_are_percent_encoded_in_file_uris() {
        let fixture = Fixture::parse("//- /a b#c.java\nclass $0C {}");
        assert_eq!(
            fixture.marker_uri(0),
            Uri::from_str("file:///a%20b%23c.java").unwrap()
        );
    }
}
