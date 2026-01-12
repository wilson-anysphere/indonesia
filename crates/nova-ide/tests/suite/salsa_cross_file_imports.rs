use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_db::{Database as LegacyDatabase, FileId, ProjectId, SalsaDatabase};
use nova_jdk::JdkIndex;

struct TestDb {
    file_a: FileId,
    file_b: FileId,
    path_a: PathBuf,
    path_b: PathBuf,
    text_a: String,
    text_b: String,
    salsa: SalsaDatabase,
}

impl LegacyDatabase for TestDb {
    fn file_content(&self, file_id: FileId) -> &str {
        if file_id == self.file_a {
            self.text_a.as_str()
        } else if file_id == self.file_b {
            self.text_b.as_str()
        } else {
            ""
        }
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        if file_id == self.file_a {
            Some(self.path_a.as_path())
        } else if file_id == self.file_b {
            Some(self.path_b.as_path())
        } else {
            None
        }
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        vec![self.file_a, self.file_b]
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        if path == self.path_a.as_path() {
            Some(self.file_a)
        } else if path == self.path_b.as_path() {
            Some(self.file_b)
        } else {
            None
        }
    }

    fn salsa_db(&self) -> Option<SalsaDatabase> {
        Some(self.salsa.clone())
    }
}

#[test]
fn file_diagnostics_reuse_caller_salsa_db_for_cross_file_imports() {
    let dir = tempfile::tempdir().unwrap();
    // Canonicalize to avoid /var vs /private/var mismatches on macOS.
    let root = dir.path().canonicalize().unwrap();
    let file_a_path = root.join("src/foo/A.java");
    let file_b_path = root.join("src/bar/B.java");
    std::fs::create_dir_all(file_a_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(file_b_path.parent().unwrap()).unwrap();

    let text_a = "package foo;\npublic class A {}\n".to_string();
    let text_b = "package bar;\nimport foo.A;\npublic class B { A a; }\n".to_string();
    std::fs::write(&file_a_path, text_a.as_bytes()).unwrap();
    std::fs::write(&file_b_path, text_b.as_bytes()).unwrap();

    let file_a = FileId::from_raw(0);
    let file_b = FileId::from_raw(1);
    let project = ProjectId::from_raw(0);

    // Seed a multi-file Salsa database. The key behavior under test is that `nova_ide::file_diagnostics`
    // should reuse this DB instead of clobbering it with a single-file `project_files` input.
    let salsa = SalsaDatabase::new();
    salsa.set_jdk_index(project, Arc::new(JdkIndex::new()));
    salsa.set_classpath_index(project, None);
    salsa.set_file_path(file_a, file_a_path.to_string_lossy().to_string());
    salsa.set_file_path(file_b, file_b_path.to_string_lossy().to_string());
    salsa.set_file_text(file_a, text_a.clone());
    salsa.set_file_text(file_b, text_b.clone());

    let rel_a = Arc::new(
        file_a_path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/"),
    );
    let rel_b = Arc::new(
        file_b_path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/"),
    );
    salsa.set_file_rel_path(file_a, rel_a);
    salsa.set_file_rel_path(file_b, rel_b);
    salsa.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let db = TestDb {
        file_a,
        file_b,
        path_a: file_a_path,
        path_b: file_b_path,
        text_a,
        text_b,
        salsa,
    };

    let diagnostics = nova_ide::file_diagnostics(&db, file_b);
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-import"),
        "expected cross-file import to resolve when reusing caller salsa db, got: {diagnostics:?}"
    );
}
