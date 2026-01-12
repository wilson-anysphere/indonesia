use std::path::PathBuf;
use std::sync::Arc;

use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::{ExtensionRegistry, ProjectId, Span};
use nova_framework::{AnalyzerRegistry, Database as FrameworkDatabase, FrameworkAnalyzer};
use nova_scheduler::CancellationToken;

use nova_ide::extensions::{
    FrameworkAnalyzerRegistryProvider, IdeExtensions, FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID,
};

struct TestFrameworkAnalyzer;

impl FrameworkAnalyzer for TestFrameworkAnalyzer {
    fn applies_to(&self, _db: &dyn FrameworkDatabase, _project: ProjectId) -> bool {
        true
    }

    fn diagnostics(
        &self,
        _db: &dyn FrameworkDatabase,
        _file: nova_ext::FileId,
    ) -> Vec<nova_ext::Diagnostic> {
        vec![nova_ext::Diagnostic::warning(
            "FW_REGISTRY_TEST",
            "framework registry diagnostic",
            Some(Span::new(0, 1)),
        )]
    }

    fn completions(
        &self,
        _db: &dyn FrameworkDatabase,
        _ctx: &nova_framework::CompletionContext,
    ) -> Vec<nova_ext::CompletionItem> {
        vec![nova_ext::CompletionItem::new("frameworkRegistryCompletion")]
    }
}

#[test]
fn analyzer_registry_provider_surfaces_diagnostics_and_completions() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/fw-registry/src/Main.java"));
    db.set_file_text(file, "class Main {}".to_string());
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);

    let mut fw_registry = AnalyzerRegistry::new();
    fw_registry.register(Box::new(TestFrameworkAnalyzer));
    let provider = FrameworkAnalyzerRegistryProvider::new(Arc::new(fw_registry)).into_arc();

    let mut registry: ExtensionRegistry<dyn nova_db::Database + Send + Sync> =
        ExtensionRegistry::default();
    registry
        .register_diagnostic_provider(provider.clone())
        .expect("register diagnostic provider");
    registry
        .register_completion_provider(provider)
        .expect("register completion provider");

    let ide = IdeExtensions::with_registry(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
        registry,
    );

    let diags = ide.diagnostics(CancellationToken::new(), file);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code.as_ref(), "FW_REGISTRY_TEST");

    let completions = ide.completions(CancellationToken::new(), file, 0);
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "frameworkRegistryCompletion");
}

#[test]
fn default_registry_registers_builtin_analyzers_as_individual_providers() {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(PathBuf::from("/fw-default/src/Main.java"));
    db.set_file_text(file, "class Main {}".to_string());
    let db_concrete = Arc::new(db);
    let db_dyn: Arc<dyn nova_db::Database + Send + Sync> = db_concrete.clone();

    let config = Arc::new(NovaConfig::default());

    let ide_generic = IdeExtensions::<InMemoryFileStore>::with_default_registry(
        db_concrete,
        Arc::clone(&config),
        ProjectId::new(0),
    );
    let stats_generic = ide_generic.registry().stats();
    let builtin_ids: Vec<&'static str> = nova_framework_builtins::builtin_analyzers_with_ids()
        .into_iter()
        .map(|desc| desc.id)
        .collect();
    for id in &builtin_ids {
        assert!(
            stats_generic.diagnostic.contains_key(*id),
            "expected default registry to register builtin analyzer diagnostic provider: {id}"
        );
        assert!(
            stats_generic.completion.contains_key(*id),
            "expected default registry to register builtin analyzer completion provider: {id}"
        );
        assert!(
            stats_generic.navigation.contains_key(*id),
            "expected default registry to register builtin analyzer navigation provider: {id}"
        );
        assert!(
            stats_generic.inlay_hint.contains_key(*id),
            "expected default registry to register builtin analyzer inlay hint provider: {id}"
        );
    }
    assert!(
        !stats_generic
            .diagnostic
            .contains_key(FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID),
        "default registry should no longer register the aggregated framework analyzer registry provider"
    );

    let ide_dyn = IdeExtensions::<dyn nova_db::Database + Send + Sync>::with_default_registry(
        db_dyn,
        config,
        ProjectId::new(0),
    );
    let stats_dyn = ide_dyn.registry().stats();
    for id in &builtin_ids {
        assert!(
            stats_dyn.diagnostic.contains_key(*id),
            "expected default registry to register builtin analyzer diagnostic provider: {id}"
        );
        assert!(
            stats_dyn.completion.contains_key(*id),
            "expected default registry to register builtin analyzer completion provider: {id}"
        );
        assert!(
            stats_dyn.navigation.contains_key(*id),
            "expected default registry to register builtin analyzer navigation provider: {id}"
        );
        assert!(
            stats_dyn.inlay_hint.contains_key(*id),
            "expected default registry to register builtin analyzer inlay hint provider: {id}"
        );
    }
    assert!(
        !stats_dyn
            .diagnostic
            .contains_key(FRAMEWORK_ANALYZER_REGISTRY_PROVIDER_ID),
        "default registry should no longer register the aggregated framework analyzer registry provider"
    );

    // Sanity check: the file id is still usable in the concrete store (guards against accidental
    // test flakiness due to unused fixture).
    assert_eq!(ide_dyn.db().file_content(file), "class Main {}");
}
