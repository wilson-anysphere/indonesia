use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ProjectId, Span};
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn unresolved_reference_offers_create_method_quickfix() {
    let source = "class A { void m() { foo(); } }";

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let diagnostics = ide.all_diagnostics(CancellationToken::new(), file);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "UNRESOLVED_REFERENCE"),
        "expected an UNRESOLVED_REFERENCE diagnostic; got {diagnostics:#?}"
    );

    let foo_start = source.find("foo").expect("expected `foo` in fixture");
    let foo_end = foo_start + "foo".len();
    let selection = Span::new(foo_start, foo_end);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(selection));
    let titles: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            lsp_types::CodeActionOrCommand::CodeAction(action) => Some(action.title.as_str()),
            lsp_types::CodeActionOrCommand::Command(command) => Some(command.title.as_str()),
        })
        .collect();

    assert!(
        titles.iter().any(|t| *t == "Create method 'foo'"),
        "expected `Create method 'foo'` quickfix; got {titles:?}"
    );
}

