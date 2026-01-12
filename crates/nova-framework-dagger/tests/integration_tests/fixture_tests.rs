use nova_framework::{FrameworkAnalyzer, MemoryDatabase};
use nova_framework_dagger::{analyze_java_files, DaggerAnalyzer, JavaSourceFile, NavigationKind};

use super::fixture_utils::load_fixture_sources;

fn load_fixture(name: &str) -> Vec<JavaSourceFile> {
    load_fixture_sources(name)
        .into_iter()
        .map(|(path, text)| JavaSourceFile { path, text })
        .collect()
}

fn slice_range(text: &str, range: nova_core::Range) -> String {
    let index = nova_core::LineIndex::new(text);
    let Some(byte_range) = index.text_range(text, range) else {
        return String::new();
    };
    let start = u32::from(byte_range.start()) as usize;
    let end = u32::from(byte_range.end()) as usize;
    text.get(start..end).unwrap_or("").to_string()
}

#[test]
fn analyzer_applies_to_projects_with_dagger_dependency() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "com.google.dagger", "dagger");

    let analyzer = DaggerAnalyzer::default();
    assert!(analyzer.applies_to(&db, project));
}

#[test]
fn missing_binding_is_reported_at_injection_site() {
    let files = load_fixture("missing_binding");
    let analysis = analyze_java_files(&files);

    let diag = analysis
        .diagnostics
        .iter()
        .find(|d| d.source.as_deref() == Some("DAGGER_MISSING_BINDING"))
        .expect("missing binding diagnostic");

    let foo_file = files
        .iter()
        .find(|f| f.path.ends_with("Foo.java"))
        .expect("Foo.java");

    assert_eq!(diag.file, foo_file.path);
    assert!(diag.message.contains("Bar"));
    assert_eq!(slice_range(&foo_file.text, diag.range), "Bar");
}

#[test]
fn duplicate_binding_is_reported_at_injection_site() {
    let files = load_fixture("duplicate_binding");
    let analysis = analyze_java_files(&files);

    let diag = analysis
        .diagnostics
        .iter()
        .find(|d| d.source.as_deref() == Some("DAGGER_DUPLICATE_BINDING"))
        .expect("duplicate binding diagnostic");

    let consumer_file = files
        .iter()
        .find(|f| f.path.ends_with("Consumer.java"))
        .expect("Consumer.java");

    assert_eq!(diag.file, consumer_file.path);
    assert!(diag.message.contains("Foo"));
    assert_eq!(slice_range(&consumer_file.text, diag.range), "Foo");
}

#[test]
fn successful_resolution_produces_navigation_links() {
    let files = load_fixture("navigation");
    let analysis = analyze_java_files(&files);

    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:#?}",
        analysis.diagnostics
    );

    let consumer_file = files
        .iter()
        .find(|f| f.path.ends_with("Consumer.java"))
        .expect("Consumer.java");
    let module_file = files
        .iter()
        .find(|f| f.path.ends_with("FooModule.java"))
        .expect("FooModule.java");

    let injection_to_provider = analysis.navigation.iter().any(|link| {
        link.kind == NavigationKind::InjectionToProvider
            && link.from.file == consumer_file.path
            && slice_range(&consumer_file.text, link.from.range) == "Foo"
            && link.to.file == module_file.path
            && slice_range(&module_file.text, link.to.range) == "provideFoo"
    });
    assert!(
        injection_to_provider,
        "expected injection -> provider navigation link"
    );

    let provider_to_injection = analysis.navigation.iter().any(|link| {
        link.kind == NavigationKind::ProviderToInjection
            && link.from.file == module_file.path
            && slice_range(&module_file.text, link.from.range) == "provideFoo"
            && link.to.file == consumer_file.path
            && slice_range(&consumer_file.text, link.to.range) == "Foo"
    });
    assert!(
        provider_to_injection,
        "expected provider -> injection navigation link"
    );
}

#[test]
fn multiline_provides_signature_produces_navigation_links() {
    let files = load_fixture("navigation_multiline");
    let analysis = analyze_java_files(&files);

    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:#?}",
        analysis.diagnostics
    );

    let consumer_file = files
        .iter()
        .find(|f| f.path.ends_with("Consumer.java"))
        .expect("Consumer.java");
    let module_file = files
        .iter()
        .find(|f| f.path.ends_with("FooModule.java"))
        .expect("FooModule.java");

    let injection_to_provider = analysis.navigation.iter().any(|link| {
        link.kind == NavigationKind::InjectionToProvider
            && link.from.file == consumer_file.path
            && slice_range(&consumer_file.text, link.from.range) == "Foo"
            && link.to.file == module_file.path
            && slice_range(&module_file.text, link.to.range) == "provideFoo"
    });
    assert!(
        injection_to_provider,
        "expected injection -> provider navigation link for multiline @Provides signature"
    );
}
