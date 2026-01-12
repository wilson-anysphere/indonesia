use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{
    CodeAction, CodeActionParams, CodeActionProvider, ExtensionContext, ProjectId, Span,
};
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn code_actions_lsp_offers_create_method_quick_fix_for_unresolved_method() {
    let source = "class A { void m() { foo(1, 2); } }";

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/Test.java"));
    db.set_file_text(file, source.to_string());

    let start = source.find("foo").expect("foo start");
    let selection = Span::new(start, start + "foo".len());

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let action = actions
        .iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create method 'foo'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected create method quick fix");

    assert_eq!(action.kind, Some(lsp_types::CodeActionKind::QUICKFIX));
}

#[test]
fn code_actions_lsp_offers_create_field_quick_fix_for_unresolved_field() {
    let source = "class A { void m() { System.out.println(this.bar); } }";

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/Test.java"));
    db.set_file_text(file, source.to_string());

    let start = source.find("bar").expect("bar start");
    let selection = Span::new(start, start + "bar".len());

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let action = actions
        .iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create field 'bar'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected create field quick fix");

    assert_eq!(action.kind, Some(lsp_types::CodeActionKind::QUICKFIX));
}

#[test]
fn code_actions_lsp_with_context_offers_create_method_quick_fix_for_unresolved_method() {
    let source = "class A { void m() { foo(1, 2); } }";

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/Test.java"));
    db.set_file_text(file, source.to_string());

    let start = source.find("foo").expect("foo start");
    let selection = Span::new(start, start + "foo".len());

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp_with_context(CancellationToken::new(), file, Some(selection), &[]);

    let action = actions
        .iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create method 'foo'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected create method quick fix");

    assert_eq!(action.kind, Some(lsp_types::CodeActionKind::QUICKFIX));
}

#[test]
fn code_actions_lsp_with_context_offers_create_field_quick_fix_for_unresolved_field() {
    let source = "class A { void m() { System.out.println(this.bar); } }";

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/Test.java"));
    db.set_file_text(file, source.to_string());

    let start = source.find("bar").expect("bar start");
    let selection = Span::new(start, start + "bar".len());

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    let actions = ide.code_actions_lsp_with_context(CancellationToken::new(), file, Some(selection), &[]);

    let action = actions
        .iter()
        .find_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create field 'bar'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected create field quick fix");

    assert_eq!(action.kind, Some(lsp_types::CodeActionKind::QUICKFIX));
}

#[test]
fn code_actions_lsp_dedupes_actions_by_kind_and_title() {
    struct DuplicateActionProvider;

    impl CodeActionProvider<InMemoryFileStore> for DuplicateActionProvider {
        fn id(&self) -> &str {
            "duplicate.create_symbol"
        }

        fn provide_code_actions(
            &self,
            _ctx: ExtensionContext<InMemoryFileStore>,
            _params: CodeActionParams,
        ) -> Vec<CodeAction> {
            vec![CodeAction {
                title: "Create method 'foo'".to_string(),
                kind: Some("quickfix".to_string()),
            }]
        }
    }

    let source = "class A { void m() { foo(1, 2); } }";

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/Test.java"));
    db.set_file_text(file, source.to_string());

    let start = source.find("foo").expect("foo start");
    let selection = Span::new(start, start + "foo".len());

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    ide.registry_mut()
        .register_code_action_provider(Arc::new(DuplicateActionProvider))
        .expect("register provider");

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));

    let matching: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create method 'foo'"
                    && action.kind == Some(lsp_types::CodeActionKind::QUICKFIX) =>
            {
                Some(action)
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        matching.len(),
        1,
        "expected actions to be deduped; got {matching:#?}"
    );
    assert!(
        matching[0].edit.is_some(),
        "expected the retained action to be the built-in quick fix (with an edit)"
    );
}

#[test]
fn code_actions_lsp_with_context_dedupes_actions_by_kind_and_title() {
    struct DuplicateActionProvider;

    impl CodeActionProvider<InMemoryFileStore> for DuplicateActionProvider {
        fn id(&self) -> &str {
            "duplicate.create_symbol"
        }

        fn provide_code_actions(
            &self,
            _ctx: ExtensionContext<InMemoryFileStore>,
            _params: CodeActionParams,
        ) -> Vec<CodeAction> {
            vec![CodeAction {
                title: "Create method 'foo'".to_string(),
                kind: Some("quickfix".to_string()),
            }]
        }
    }

    let source = "class A { void m() { foo(1, 2); } }";

    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/Test.java"));
    db.set_file_text(file, source.to_string());

    let start = source.find("foo").expect("foo start");
    let selection = Span::new(start, start + "foo".len());

    let db: Arc<InMemoryFileStore> = Arc::new(db);
    let mut ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));
    ide.registry_mut()
        .register_code_action_provider(Arc::new(DuplicateActionProvider))
        .expect("register provider");

    let actions = ide.code_actions_lsp_with_context(CancellationToken::new(), file, Some(selection), &[]);

    let matching: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create method 'foo'"
                    && action.kind == Some(lsp_types::CodeActionKind::QUICKFIX) =>
            {
                Some(action)
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        matching.len(),
        1,
        "expected actions to be deduped; got {matching:#?}"
    );
    assert!(
        matching[0].edit.is_some(),
        "expected the retained action to be the built-in quick fix (with an edit)"
    );
}
