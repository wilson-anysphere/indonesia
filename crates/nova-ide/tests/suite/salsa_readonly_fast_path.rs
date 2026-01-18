use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_classpath::ClasspathIndex;
use nova_db::{
    Database as LegacyDatabase, FileId, NovaInputs, ProjectId, QueryStat, SalsaDatabase,
};
use nova_jdk::JdkIndex;
use nova_scheduler::CancellationToken;

struct TestDb {
    salsa: SalsaDatabase,
    file_path: PathBuf,
    // Intentionally alternate between two stable strings so any accidental
    // `set_file_text(db.file_content(..).to_string())` in nova-ide will mutate
    // Salsa inputs and defeat memoization.
    texts: [String; 2],
    next_text: Cell<usize>,
}

impl LegacyDatabase for TestDb {
    fn file_content(&self, _file_id: FileId) -> &str {
        let idx = self.next_text.get();
        self.next_text.set((idx + 1) % self.texts.len());
        &self.texts[idx]
    }

    fn file_path(&self, _file_id: FileId) -> Option<&Path> {
        Some(&self.file_path)
    }

    fn salsa_db(&self) -> Option<SalsaDatabase> {
        Some(self.salsa.clone())
    }
}

fn stat(stats: &nova_db::QueryStats, name: &str) -> QueryStat {
    *stats
        .by_query
        .get(name)
        .unwrap_or_else(|| panic!("missing query stats entry for `{name}`"))
}

#[test]
fn salsa_fast_path_is_read_only() {
    let file = FileId::from_raw(0);
    let project = ProjectId::from_raw(0);

    let salsa = SalsaDatabase::new();
    salsa.set_jdk_index(project, Arc::new(JdkIndex::new()));
    salsa.set_classpath_index(project, None);

    let text_1 = "class Foo {}".to_string();
    let text_2 = "class Foo { int x; }".to_string();

    salsa.set_file_text(file, text_1.clone());
    salsa.clear_query_stats();

    let db = TestDb {
        salsa: salsa.clone(),
        file_path: PathBuf::from("src/Foo.java"),
        texts: [text_1, text_2],
        next_text: Cell::new(0),
    };

    let cancel = CancellationToken::new();
    let _ = nova_ide::core_file_diagnostics(&db, file, &cancel);
    let after_first = salsa.query_stats();

    // Ensure the queries we care about are present (otherwise the assertions
    // below would trivially pass).
    assert!(
        after_first.by_query.contains_key("type_diagnostics"),
        "expected nova_ide::core_file_diagnostics to execute type_diagnostics at least once"
    );
    assert!(
        after_first.by_query.contains_key("flow_diagnostics_for_file"),
        "expected nova_ide::core_file_diagnostics to execute flow_diagnostics_for_file at least once"
    );
    assert!(
        after_first.by_query.contains_key("import_diagnostics"),
        "expected nova_ide::core_file_diagnostics to execute import_diagnostics at least once"
    );

    let _ = nova_ide::core_file_diagnostics(&db, file, &cancel);
    let after_second = salsa.query_stats();

    let first_type = stat(&after_first, "type_diagnostics");
    let second_type = stat(&after_second, "type_diagnostics");
    assert_eq!(
        second_type.executions, first_type.executions,
        "type_diagnostics should not re-execute without any Salsa write"
    );
    assert_eq!(
        second_type.validated_memoized, first_type.validated_memoized,
        "type_diagnostics should not validate memoized results without any Salsa write"
    );

    let first_flow = stat(&after_first, "flow_diagnostics_for_file");
    let second_flow = stat(&after_second, "flow_diagnostics_for_file");
    assert_eq!(
        second_flow.executions, first_flow.executions,
        "flow_diagnostics_for_file should not re-execute without any Salsa write"
    );
    assert_eq!(
        second_flow.validated_memoized, first_flow.validated_memoized,
        "flow_diagnostics_for_file should not validate memoized results without any Salsa write"
    );

    let first_imports = stat(&after_first, "import_diagnostics");
    let second_imports = stat(&after_second, "import_diagnostics");
    assert_eq!(
        second_imports.executions, first_imports.executions,
        "import_diagnostics should not re-execute without any Salsa write"
    );
    assert_eq!(
        second_imports.validated_memoized, first_imports.validated_memoized,
        "import_diagnostics should not validate memoized results without any Salsa write"
    );
}

#[test]
fn salsa_fast_path_falls_back_when_file_text_is_out_of_sync() {
    let file = FileId::from_raw(0);
    let project = ProjectId::from_raw(0);

    // Host Salsa DB sees a stale on-disk snapshot.
    let salsa = SalsaDatabase::new();
    let host_jdk = Arc::new(JdkIndex::new());
    salsa.set_jdk_index(project, Arc::clone(&host_jdk));
    let host_classpath = Arc::new(ClasspathIndex::default());
    salsa.set_classpath_index(project, Some(Arc::clone(&host_classpath)));
    salsa.set_file_text(file, "class A {}".to_string());

    // The legacy `Database` implementation serves a different in-memory overlay that contains an
    // unresolved import. `with_salsa_snapshot_for_single_file` should detect the mismatch and
    // avoid running Salsa queries against the stale host DB.
    let overlay_text = r#"
import does.not.Exist;
class A {}
"#
    .to_string();

    let db = TestDb {
        salsa: salsa.clone(),
        file_path: PathBuf::from("src/A.java"),
        texts: [overlay_text.clone(), overlay_text],
        next_text: Cell::new(0),
    };

    let cancel = CancellationToken::new();
    let diags = nova_ide::core_file_diagnostics(&db, file, &cancel);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected unresolved-import diagnostic for overlay text; got {diags:#?}"
    );

    // Ensure we didn't \"fix\" the mismatch by mutating the host Salsa DB (which would clobber
    // host-managed inputs and invalidate memoized state).
    salsa.with_snapshot(|snap| {
        assert_eq!(
            snap.file_content(file).as_str(),
            "class A {}",
            "expected host Salsa DB file_content to remain unchanged when nova-ide falls back"
        );
        assert!(
            Arc::ptr_eq(&snap.jdk_index(project).0, &host_jdk),
            "expected host Salsa DB to preserve the provided jdk_index"
        );
        assert!(
            snap.classpath_index(project)
                .is_some_and(|cp| Arc::ptr_eq(&cp.0, &host_classpath)),
            "expected host Salsa DB to preserve the provided classpath_index"
        );
    });
}
