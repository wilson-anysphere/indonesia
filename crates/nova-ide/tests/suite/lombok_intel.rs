use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::{Position, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{FileId, InMemoryFileStore};
use nova_ide::{completions, implementation};
use tempfile::TempDir;

use crate::framework_harness::{offset_to_position, CARET};

fn fixture(text_with_caret: &str) -> (InMemoryFileStore, FileId, Position) {
    let caret_offset = text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let text = text_with_caret.replace(CARET, "");
    let pos = offset_to_position(&text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, text);
    (db, file, pos)
}

#[test]
fn completion_includes_lombok_getters() {
    let (db, file, pos) = fixture(
        r#"
import lombok.Getter;

class Foo {
  @Getter int x;
  @Getter boolean active;
}

class Use {
  void m() {
    Foo f = new Foo();
    f.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"getX"),
        "expected completion list to contain Lombok getX; got {labels:?}"
    );
    assert!(
        labels.contains(&"isActive"),
        "expected completion list to contain Lombok isActive; got {labels:?}"
    );
}

#[test]
fn completion_includes_lombok_withers() {
    let (db, file, pos) = fixture(
        r#"
import lombok.With;

class Foo {
  @With int x;
}

class Use {
  void m() {
    Foo f = new Foo();
    f.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"withX"),
        "expected completion list to contain Lombok withX; got {labels:?}"
    );
}

#[test]
fn completion_includes_lombok_log_field() {
    let (db, file, pos) = fixture(
        r#"
import lombok.extern.java.Log;

@Log
class Foo {
}

class Use {
  void m() {
    Foo f = new Foo();
    f.<|>
  }
}
"#,
    );

    let items = completions(&db, file, pos);
    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"log"),
        "expected completion list to contain Lombok log; got {labels:?}"
    );
}

struct FileIdFixture {
    _temp_dir: TempDir,
    db: InMemoryFileStore,
    files: HashMap<FileId, String>,
    paths: HashMap<FileId, PathBuf>,
    markers: HashMap<u32, (FileId, usize)>,
}

impl FileIdFixture {
    fn parse(fixture: &str) -> Self {
        let temp_dir = TempDir::new().expect("tempdir");
        let root = temp_dir.path();

        let mut current_path: Option<PathBuf> = None;
        let mut current_text = String::new();
        let mut files: Vec<(PathBuf, String)> = Vec::new();

        for line in fixture.lines() {
            if let Some(rest) = line.strip_prefix("//-") {
                if let Some(path) = current_path.take() {
                    files.push((path, current_text));
                    current_text = String::new();
                }

                let rel = rest.trim().trim_start_matches('/');
                current_path = Some(root.join(rel));
                continue;
            }

            if !current_text.is_empty() {
                current_text.push('\n');
            }
            current_text.push_str(line);
        }

        if let Some(path) = current_path.take() {
            files.push((path, current_text));
        }

        let mut db = InMemoryFileStore::new();
        let mut file_texts: HashMap<FileId, String> = HashMap::new();
        let mut file_paths: HashMap<FileId, PathBuf> = HashMap::new();
        let mut markers: HashMap<u32, (FileId, usize)> = HashMap::new();

        for (path, text) in files {
            let (text, file_markers) = strip_markers(&text);
            let file_id = db.file_id_for_path(&path);
            db.set_file_text(file_id, text.clone());

            file_texts.insert(file_id, text);
            file_paths.insert(file_id, path);
            for (id, offset) in file_markers {
                markers.insert(id, (file_id, offset));
            }
        }

        Self {
            _temp_dir: temp_dir,
            db,
            files: file_texts,
            paths: file_paths,
            markers,
        }
    }

    fn marker_file(&self, id: u32) -> FileId {
        self.markers.get(&id).unwrap().0
    }

    fn marker_position(&self, id: u32) -> Position {
        let (file_id, offset) = self.markers.get(&id).unwrap();
        let text = self.files.get(file_id).unwrap();
        offset_to_position(text, *offset)
    }

    fn marker_uri(&self, id: u32) -> Uri {
        let (file_id, _) = self.markers.get(&id).unwrap();
        let path = self.paths.get(file_id).unwrap();
        uri_for_path(path)
    }
}

#[test]
fn go_to_implementation_on_lombok_getter_navigates_to_annotation() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
import lombok.Getter;

class Foo {
  $1@Getter int x;
}

//- /Use.java
class Use {
  void m() {
    Foo f = new Foo();
    f.$0getX();
  }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_implementation_on_lombok_wither_navigates_to_annotation() {
    let fixture = FileIdFixture::parse(
        r#"
//- /Foo.java
import lombok.With;

class Foo {
  $1@With int x;
}

//- /Use.java
class Use {
  void m() {
    Foo f = new Foo();
    f.$0withX(1);
  }
}
"#,
    );

    let file = fixture.marker_file(0);
    let pos = fixture.marker_position(0);
    let got = implementation(&fixture.db, file, pos);

    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::new(path.to_path_buf()).expect("fixture paths should be absolute");
    let uri = path_to_file_uri(&abs).expect("path should convert to a file URI");
    Uri::from_str(&uri).expect("URI should parse")
}

fn strip_markers(text: &str) -> (String, Vec<(u32, usize)>) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut markers = Vec::new();

    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
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
