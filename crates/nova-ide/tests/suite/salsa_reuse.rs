use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_db::{Database as LegacyDatabase, FileId, ProjectId, QueryStat, SalsaDatabase};
use nova_jdk::JdkIndex;
use nova_scheduler::CancellationToken;

fn stat(db: &SalsaDatabase, query: &str) -> QueryStat {
    *db.query_stats()
        .by_query
        .get(query)
        .unwrap_or_else(|| panic!("missing query stats entry for `{query}`"))
}

struct TestDb {
    file: FileId,
    path: PathBuf,
    text: String,
    salsa: SalsaDatabase,
}

impl LegacyDatabase for TestDb {
    fn file_content(&self, file_id: FileId) -> &str {
        if file_id == self.file {
            self.text.as_str()
        } else {
            ""
        }
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        if file_id == self.file {
            Some(self.path.as_path())
        } else {
            None
        }
    }

    fn salsa_db(&self) -> Option<SalsaDatabase> {
        Some(self.salsa.clone())
    }
}

#[test]
fn core_diagnostics_reuse_caller_provided_salsa_database() {
    let file = FileId::from_raw(1);
    let path = PathBuf::from("/test.java");
    let text = r#"
class A {
  void m() {
    int x = 0;
  }
}
"#
    .to_string();

    let project = ProjectId::from_raw(0);
    let salsa = SalsaDatabase::new();
    salsa.set_jdk_index(project, Arc::new(JdkIndex::new()));
    salsa.set_classpath_index(project, None);
    salsa.set_file_text(file, text.clone());
    salsa.clear_query_stats();

    let db = TestDb {
        file,
        path,
        text,
        salsa: salsa.clone(),
    };

    let cancel = CancellationToken::new();
    let _ = nova_ide::core_file_diagnostics(&db, file, &cancel);
    let type_before = stat(&salsa, "type_diagnostics");
    let flow_before = stat(&salsa, "flow_diagnostics_for_file");
    assert!(
        type_before.executions > 0,
        "expected type_diagnostics to execute at least once when using the caller-provided salsa db"
    );
    assert!(
        flow_before.executions > 0,
        "expected flow_diagnostics_for_file to execute at least once when using the caller-provided salsa db"
    );

    salsa.with_write(|db| ra_salsa::Database::synthetic_write(db, ra_salsa::Durability::LOW));

    let _ = nova_ide::core_file_diagnostics(&db, file, &cancel);
    let type_after = stat(&salsa, "type_diagnostics");
    let flow_after = stat(&salsa, "flow_diagnostics_for_file");

    assert_eq!(
        type_after.executions, type_before.executions,
        "expected type_diagnostics to be memoized across calls"
    );
    assert!(
        type_after.validated_memoized > type_before.validated_memoized,
        "expected type_diagnostics to validate memoized results on subsequent calls"
    );

    assert_eq!(
        flow_after.executions, flow_before.executions,
        "expected flow_diagnostics_for_file to be memoized across calls"
    );
    assert!(
        flow_after.validated_memoized > flow_before.validated_memoized,
        "expected flow_diagnostics_for_file to validate memoized results on subsequent calls"
    );
}

struct MultiFileDb {
    files: HashMap<FileId, (PathBuf, String)>,
    salsa: SalsaDatabase,
}

impl LegacyDatabase for MultiFileDb {
    fn file_content(&self, file_id: FileId) -> &str {
        self.files
            .get(&file_id)
            .map(|(_, text)| text.as_str())
            .unwrap_or("")
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        self.files.get(&file_id).map(|(path, _)| path.as_path())
    }

    fn salsa_db(&self) -> Option<SalsaDatabase> {
        Some(self.salsa.clone())
    }
}

#[test]
fn file_diagnostics_reuse_caller_provided_salsa_database_for_imports() {
    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);
    let path_a = PathBuf::from("/src/p/A.java");
    let path_b = PathBuf::from("/src/q/B.java");
    let text_a = "package p; public class A {}".to_string();
    let text_b = "package q; import p.A; class B { A a; }".to_string();

    let project = ProjectId::from_raw(0);
    let salsa = SalsaDatabase::new();
    salsa.set_jdk_index(project, Arc::new(JdkIndex::new()));
    salsa.set_classpath_index(project, None);
    salsa.set_file_text(file_a, text_a.clone());
    salsa.set_file_text(file_b, text_b.clone());
    salsa.set_file_rel_path(file_a, Arc::new("src/p/A.java".to_string()));
    salsa.set_file_rel_path(file_b, Arc::new("src/q/B.java".to_string()));
    salsa.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let mut files = HashMap::new();
    files.insert(file_a, (path_a, text_a));
    files.insert(file_b, (path_b, text_b));

    let db = MultiFileDb {
        files,
        salsa: salsa.clone(),
    };

    let diags = nova_ide::file_diagnostics(&db, file_b);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected import to resolve using caller-provided Salsa DB; got diagnostics: {diags:#?}"
    );
}
