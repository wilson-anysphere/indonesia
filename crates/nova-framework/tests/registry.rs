use std::path::Path;

use nova_core::ProjectId;
use nova_framework::{
    AnalyzerRegistry, CompletionContext, Database as FrameworkDatabase, FrameworkAnalyzer,
    FrameworkData, InlayHint, MemoryDatabase, NavigationTarget, OtherFrameworkData, Symbol,
    VirtualField, VirtualMember,
};
use nova_hir::framework::ClassData;
use nova_types::{CompletionItem, Diagnostic, Span, Type};

#[derive(Clone, Copy)]
struct FakeAnalyzer {
    applicable_to: ProjectId,
    diag_message: &'static str,
    completion_label: &'static str,
    member_name: &'static str,
}

impl FrameworkAnalyzer for FakeAnalyzer {
    fn applies_to(&self, _db: &dyn nova_framework::Database, project: ProjectId) -> bool {
        project == self.applicable_to
    }

    fn analyze_file(
        &self,
        _db: &dyn nova_framework::Database,
        _file: nova_vfs::FileId,
    ) -> Option<FrameworkData> {
        Some(FrameworkData::Other(OtherFrameworkData {
            kind: "fake".to_string(),
        }))
    }

    fn diagnostics(
        &self,
        _db: &dyn nova_framework::Database,
        _file: nova_vfs::FileId,
    ) -> Vec<Diagnostic> {
        vec![Diagnostic::warning(
            "FAKE_DIAG",
            self.diag_message,
            Some(Span::new(0, 1)),
        )]
    }

    fn completions(
        &self,
        _db: &dyn nova_framework::Database,
        _ctx: &CompletionContext,
    ) -> Vec<CompletionItem> {
        vec![CompletionItem::new(self.completion_label)]
    }

    fn navigation(
        &self,
        _db: &dyn nova_framework::Database,
        symbol: &Symbol,
    ) -> Vec<NavigationTarget> {
        let file = match *symbol {
            Symbol::File(file) => file,
            Symbol::Class(_) => nova_vfs::FileId::from_raw(0),
        };

        vec![NavigationTarget {
            file,
            span: Some(Span::new(5, 10)),
            label: self.member_name.to_string(),
        }]
    }

    fn virtual_members(
        &self,
        _db: &dyn nova_framework::Database,
        _class: nova_types::ClassId,
    ) -> Vec<VirtualMember> {
        vec![VirtualMember::Field(VirtualField {
            name: self.member_name.to_string(),
            ty: Type::Named("java.lang.String".into()),
            is_static: false,
            is_final: false,
        })]
    }

    fn inlay_hints(
        &self,
        _db: &dyn nova_framework::Database,
        _file: nova_vfs::FileId,
    ) -> Vec<InlayHint> {
        vec![InlayHint {
            span: Some(Span::new(0, 0)),
            label: "hint".to_string(),
        }]
    }
}

#[test]
fn aggregates_only_applicable_analyzers() {
    let mut db = MemoryDatabase::new();
    let project_a = db.add_project();
    let project_b = db.add_project();

    let file_a = db.add_file(project_a);
    let class_a = db.add_class(project_a, ClassData::default());

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(FakeAnalyzer {
        applicable_to: project_a,
        diag_message: "diag-a",
        completion_label: "comp-a",
        member_name: "m-a",
    }));
    registry.register(Box::new(FakeAnalyzer {
        applicable_to: project_a,
        diag_message: "diag-b",
        completion_label: "comp-b",
        member_name: "m-b",
    }));
    registry.register(Box::new(FakeAnalyzer {
        applicable_to: project_b,
        diag_message: "diag-c",
        completion_label: "comp-c",
        member_name: "m-c",
    }));

    let diags = registry.framework_diagnostics(&db, file_a);
    assert_eq!(diags.len(), 2);
    assert!(diags.iter().any(|d| d.message == "diag-a"));
    assert!(diags.iter().any(|d| d.message == "diag-b"));

    let ctx = CompletionContext {
        project: project_a,
        file: file_a,
        offset: 0,
    };
    let completions = registry.framework_completions(&db, &ctx);
    assert_eq!(completions.len(), 2);
    assert!(completions.iter().any(|c| c.label == "comp-a"));
    assert!(completions.iter().any(|c| c.label == "comp-b"));

    let members = registry.framework_virtual_members(&db, class_a);
    assert_eq!(members.len(), 2);

    let hints = registry.framework_inlay_hints(&db, file_a);
    assert_eq!(hints.len(), 2);

    let nav = registry.framework_navigation_targets(&db, &Symbol::File(file_a));
    assert_eq!(nav.len(), 2);

    let data = registry.framework_data(&db, file_a);
    assert_eq!(data.len(), 2);
}

#[test]
fn classpath_queries_accept_internal_and_binary_names() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    db.add_classpath_class(project, "org.example.Foo");

    assert!(FrameworkDatabase::has_class_on_classpath(
        &db,
        project,
        "org.example.Foo"
    ));
    assert!(FrameworkDatabase::has_class_on_classpath(
        &db,
        project,
        "org/example/Foo"
    ));

    assert!(FrameworkDatabase::has_class_on_classpath_prefix(
        &db,
        project,
        "org.example."
    ));
    assert!(FrameworkDatabase::has_class_on_classpath_prefix(
        &db,
        project,
        "org/example/"
    ));
}

#[test]
fn analyzers_can_read_file_text_via_database() {
    #[derive(Clone, Copy)]
    struct TextLenAnalyzer;

    impl FrameworkAnalyzer for TextLenAnalyzer {
        fn applies_to(&self, _db: &dyn nova_framework::Database, _project: ProjectId) -> bool {
            true
        }

        fn diagnostics(
            &self,
            db: &dyn nova_framework::Database,
            file: nova_vfs::FileId,
        ) -> Vec<Diagnostic> {
            let len = db.file_text(file).map_or(0, |t| t.len());
            vec![Diagnostic::warning(
                "TEST_FILE_TEXT",
                format!("len={len}"),
                None,
            )]
        }
    }

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    let file = db.add_file_with_text(project, "abc");

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(TextLenAnalyzer));

    let diags = registry.framework_diagnostics(&db, file);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].message, "len=3");
}

#[test]
fn memory_database_tracks_file_paths_and_all_files() {
    let mut db = MemoryDatabase::new();
    let project_a = db.add_project();
    let project_b = db.add_project();

    let file_a1 = db.add_file_with_path(project_a, "src/A.java");
    let file_a2 = db.add_file(project_a);
    let file_b1 = db.add_file_with_path(project_b, "src/B.java");

    assert_eq!(db.file_path(file_a1), Some(Path::new("src/A.java")));
    assert_eq!(db.file_id(Path::new("src/A.java")), Some(file_a1));

    // Files without paths should behave like "no path info available".
    assert_eq!(db.file_path(file_a2), None);

    let files_a = db.all_files(project_a);
    assert_eq!(files_a, vec![file_a1, file_a2]);

    let files_b = db.all_files(project_b);
    assert_eq!(files_b, vec![file_b1]);
}
