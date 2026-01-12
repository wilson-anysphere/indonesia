use std::path::PathBuf;

use nova_ai::{DbProjectDatabase, SemanticSearch, TrigramSemanticSearch, VirtualWorkspace};
use nova_core::FileId;
use nova_db::{Database, InMemoryFileStore};

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

    let db = DbProjectDatabase::new(&store);
    let mut search = TrigramSemanticSearch::new();
    search.index_project(&db);

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
