use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::{Position, Uri};
use nova_ide::Database;
use nova_index::TextRange;

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
                    let uri = Uri::from_str(&format!("file://{}", path)).unwrap();
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
            let uri = Uri::from_str(&format!("file://{}", path)).unwrap();
            files.push((uri, current_text));
        }

        let mut markers: HashMap<u32, (Uri, usize)> = HashMap::new();
        let mut file_texts: HashMap<Uri, String> = HashMap::new();
        let mut db = Database::new();

        for (uri, text) in files {
            let (text, file_markers) = strip_markers(&text);
            file_texts.insert(uri.clone(), text.clone());
            for (id, offset) in file_markers {
                markers.insert(id, (uri.clone(), offset));
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
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                j += 1;
            }

            if j > i + 1 {
                let id: u32 = text[i + 1..j].parse().unwrap();
                markers.push((id, out.len()));
                i = j;
                continue;
            }
        }

        out.push(bytes[i] as char);
        i += 1;
    }

    (out, markers)
}
