#[cfg(feature = "jpa")]
use nova_framework::MemoryDatabase;

/// Smoke test: when the `jpa` feature is enabled, the built-in analyzer list
/// should include the JPA analyzer and it should apply to JPA projects.
#[test]
#[cfg(feature = "jpa")]
fn jpa_builtin_analyzer_applies() {
    let analyzers = nova_framework_builtins::builtin_analyzers();

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "jakarta.persistence", "jakarta.persistence-api");

    let any_applies = analyzers.iter().any(|a| a.applies_to(&db, project));
    assert!(
        any_applies,
        "expected JPA builtin analyzer to apply for jakarta.persistence:jakarta.persistence-api"
    );
}

