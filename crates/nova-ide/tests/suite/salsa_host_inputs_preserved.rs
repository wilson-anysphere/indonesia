use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_classpath::ClasspathIndex;
use nova_db::{
    Database as LegacyDatabase, FileId, NovaInputs, ProjectId, SalsaDatabase, SourceRootId,
};
use nova_jdk::JdkIndex;
use nova_scheduler::CancellationToken;

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
fn core_diagnostics_preserve_host_provided_inputs() {
    let file = FileId::from_raw(1);
    let other_file = FileId::from_raw(2);
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
    let file_project = ProjectId::from_raw(42);
    let salsa = SalsaDatabase::new();

    // Reuse the default single-project config/class ids seeded by `SalsaDatabase::new()` so that
    // semantic queries for `file_project` do not panic.
    let (default_project_config, default_class_ids) = salsa.with_snapshot(|snap| {
        (
            snap.project_config(project),
            snap.project_class_ids(project),
        )
    });

    let custom_jdk = Arc::new(JdkIndex::new());
    salsa.set_jdk_index(project, Arc::clone(&custom_jdk));

    let classpath_index = Arc::new(ClasspathIndex::default());
    salsa.set_classpath_index(project, Some(Arc::clone(&classpath_index)));

    let project_files = Arc::new(vec![other_file, file]);
    salsa.set_project_files(project, Arc::clone(&project_files));

    // Seed host-managed file inputs.
    let file_rel_path = Arc::new("host/test.java".to_string());
    let source_root = SourceRootId::from_raw(123);
    salsa.set_file_project(file, file_project);
    salsa.set_source_root(file, source_root);
    salsa.set_file_rel_path(file, Arc::clone(&file_rel_path));
    salsa.set_file_exists(file, true);
    salsa.set_file_content(file, Arc::new(text.clone()));

    // Provide required inputs for the file's owning project so type diagnostics don't panic.
    salsa.set_project_config(file_project, Arc::clone(&default_project_config));
    salsa.set_project_class_ids(file_project, Arc::clone(&default_class_ids));
    salsa.set_jdk_index(file_project, Arc::new(JdkIndex::new()));
    salsa.set_classpath_index(file_project, None);
    salsa.set_project_files(file_project, Arc::new(vec![file]));

    let db = TestDb {
        file,
        path,
        text,
        salsa: salsa.clone(),
    };

    let cancel = CancellationToken::new();
    let _ = nova_ide::core_file_diagnostics(&db, file, &cancel);

    salsa.with_snapshot(|snap| {
        assert!(
            snap.classpath_index(project).is_some(),
            "expected `core_file_diagnostics` to preserve the host-provided classpath index when reusing a caller-provided Salsa database"
        );
        assert!(
            Arc::ptr_eq(&snap.jdk_index(project).0, &custom_jdk),
            "expected `core_file_diagnostics` to preserve the host-provided jdk index when reusing a caller-provided Salsa database"
        );
        assert_eq!(
            snap.file_project(file),
            file_project,
            "expected `core_file_diagnostics` to preserve the host-provided file project when reusing a caller-provided Salsa database"
        );
        assert_eq!(
            snap.source_root(file),
            source_root,
            "expected `core_file_diagnostics` to preserve the host-provided source root when reusing a caller-provided Salsa database"
        );
        assert!(
            Arc::ptr_eq(&snap.file_rel_path(file), &file_rel_path),
            "expected `core_file_diagnostics` to preserve the host-provided file_rel_path when reusing a caller-provided Salsa database"
        );
        assert!(
            Arc::ptr_eq(&snap.project_files(project), &project_files),
            "expected `core_file_diagnostics` to preserve the host-provided project_files ordering when reusing a caller-provided Salsa database"
        );
    });
}
