use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_db::{Database as LegacyDatabase, FileId, ProjectId, QueryStat, SalsaDatabase};
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
    stats.by_query.get(name).copied().unwrap_or_default()
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
