use std::path::PathBuf;

use nova_ai::{DbProjectDatabase, SemanticSearch, TrigramSemanticSearch, VirtualWorkspace};
use nova_core::{FileId, ProjectDatabase};
use nova_db::{Database, InMemoryFileStore, SalsaDatabase};

#[test]
fn trigram_semantic_search_indexes_virtual_workspace() {
    let workspace = VirtualWorkspace::new([
        (
            "src/Hello.java".to_string(),
            r#"
                public class Hello {
                    public String helloWorld() {
                        return "hello world";
                    }
                }
            "#
            .to_string(),
        ),
        (
            "src/Other.java".to_string(),
            r#"
                public class Other {
                    public String goodbyeWorld() {
                        return "goodbye world";
                    }
                }
            "#
            .to_string(),
        ),
    ]);

    let mut search = TrigramSemanticSearch::new();
    search.index_project(&workspace);

    let results = search.search("helloWorld");
    assert!(!results.is_empty(), "expected at least one search result");
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));
}

#[test]
fn db_project_database_indexes_in_memory_file_store() {
    let mut store = InMemoryFileStore::new();
    let file_id = store.file_id_for_path("src/Main.java");
    store.set_file_text(
        file_id,
        r#"
            public class Main {
                public static void main(String[] args) {
                    System.out.println("hello from db");
                }
            }
        "#
        .to_string(),
    );

    let mut search = TrigramSemanticSearch::new();
    search.index_database(&store);

    let results = search.search("hello from db");
    assert!(!results.is_empty(), "expected at least one search result");
    assert_eq!(results[0].path, PathBuf::from("src/Main.java"));
}

#[test]
fn db_project_database_indexes_db_without_file_id_lookup() {
    #[derive(Debug)]
    struct PathOnlyDb {
        path: PathBuf,
        text: String,
    }

    impl Database for PathOnlyDb {
        fn file_content(&self, file_id: FileId) -> &str {
            if file_id == FileId::from_raw(0) {
                &self.text
            } else {
                ""
            }
        }

        fn file_path(&self, file_id: FileId) -> Option<&std::path::Path> {
            if file_id == FileId::from_raw(0) {
                Some(self.path.as_path())
            } else {
                None
            }
        }

        fn all_file_ids(&self) -> Vec<FileId> {
            vec![FileId::from_raw(0)]
        }
    }

    let db = PathOnlyDb {
        path: PathBuf::from("src/OnlyPath.java"),
        text: r#"
            public class OnlyPath {
                public String message() {
                    return "hello from db";
                }
            }
        "#
        .to_string(),
    };

    let db = DbProjectDatabase::new(&db);
    let mut search = TrigramSemanticSearch::new();
    search.index_project(&db);

    let results = search.search("hello from db");
    assert!(!results.is_empty(), "expected at least one search result");
    assert_eq!(results[0].path, PathBuf::from("src/OnlyPath.java"));
}

#[test]
fn virtual_workspace_project_files_are_sorted() {
    let workspace = VirtualWorkspace::new([
        ("b.txt".to_string(), "b".to_string()),
        ("a.txt".to_string(), "a".to_string()),
    ]);

    let files = ProjectDatabase::project_files(&workspace);
    assert_eq!(files, vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]);
}

#[test]
fn db_project_database_project_files_are_sorted_and_deduped() {
    #[derive(Debug)]
    struct TwoFileDb {
        a: PathBuf,
        b: PathBuf,
    }

    impl Database for TwoFileDb {
        fn file_content(&self, _file_id: FileId) -> &str {
            ""
        }

        fn file_path(&self, file_id: FileId) -> Option<&std::path::Path> {
            match file_id.to_raw() {
                0 => Some(self.a.as_path()),
                1 => Some(self.b.as_path()),
                _ => None,
            }
        }

        fn all_file_ids(&self) -> Vec<FileId> {
            vec![FileId::from_raw(1), FileId::from_raw(0), FileId::from_raw(1)]
        }
    }

    let db = TwoFileDb {
        a: PathBuf::from("a.txt"),
        b: PathBuf::from("b.txt"),
    };

    let db = DbProjectDatabase::new(&db);
    let files = ProjectDatabase::project_files(&db);
    assert_eq!(files, vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]);
}

#[test]
fn source_db_project_database_indexes_salsa_snapshot() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class Main { String hello() { return \"hello\"; } }".to_string());
    db.set_file_path(file, "src/Main.java");

    let snap = db.snapshot();

    let mut search = TrigramSemanticSearch::new();
    search.index_source_database(&snap);

    let results = search.search("hello");
    assert!(!results.is_empty(), "expected at least one search result");
    assert_eq!(results[0].path, PathBuf::from("src/Main.java"));
}
