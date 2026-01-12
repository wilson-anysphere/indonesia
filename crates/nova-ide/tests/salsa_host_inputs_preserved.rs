use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_classpath::ClasspathIndex;
use nova_db::{Database as LegacyDatabase, FileId, NovaInputs, ProjectId, SalsaDatabase};
use nova_jdk::JdkIndex;

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
fn core_diagnostics_preserve_host_provided_jdk_and_classpath_inputs() {
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

    let custom_jdk = Arc::new(JdkIndex::new());
    salsa.set_jdk_index(project, Arc::clone(&custom_jdk));

    let classpath_index = Arc::new(ClasspathIndex::default());
    salsa.set_classpath_index(project, Some(classpath_index));

    // Seed file inputs.
    salsa.set_file_text(file, text.clone());

    let db = TestDb {
        file,
        path,
        text,
        salsa: salsa.clone(),
    };

    let _ = nova_ide::core_file_diagnostics(&db, file);

    salsa.with_snapshot(|snap| {
        assert!(
            snap.classpath_index(project).is_some(),
            "expected `ensure_salsa_inputs_for_single_file` to preserve the host-provided classpath index"
        );
        assert!(
            Arc::ptr_eq(&snap.jdk_index(project).0, &custom_jdk),
            "expected `ensure_salsa_inputs_for_single_file` to preserve the host-provided jdk index"
        );
    });
}
