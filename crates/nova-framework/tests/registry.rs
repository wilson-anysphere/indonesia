use nova_framework::{
    AnalyzerRegistry, CompletionContext, FrameworkAnalyzer, FrameworkData, InlayHint,
    MemoryDatabase, NavigationTarget, OtherFrameworkData, Symbol, VirtualField, VirtualMember,
};
use nova_hir::framework::ClassData;
use nova_types::{CompletionItem, Diagnostic, ProjectId, Span, Type};

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
