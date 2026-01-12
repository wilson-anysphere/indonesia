#[cfg(feature = "spring")]
use nova_framework::MemoryDatabase;

/// Smoke test: when the `spring` feature is enabled, the built-in analyzer list
/// should include the Spring analyzer and it should apply to Spring projects.
#[test]
#[cfg(feature = "spring")]
fn spring_builtin_analyzer_applies() {
    let analyzers = nova_framework_builtins::builtin_analyzers();

    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework.boot", "spring-boot");

    let any_applies = analyzers.iter().any(|a| a.applies_to(&db, project));
    assert!(
        any_applies,
        "expected Spring builtin analyzer to apply for org.springframework.boot:spring-boot"
    );
}
