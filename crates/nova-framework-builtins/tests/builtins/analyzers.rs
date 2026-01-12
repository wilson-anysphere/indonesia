use nova_framework::MemoryDatabase;

/// Baseline analyzers should include the common framework set used throughout
/// Nova today.
#[test]
fn builtin_analyzers_include_baseline_set() {
    let analyzers = nova_framework_builtins::builtin_analyzers();

    assert!(
        analyzers.len() >= 5,
        "expected at least the baseline analyzers (lombok/dagger/mapstruct/micronaut/quarkus)"
    );

    let cases = [
        ("org.projectlombok", "lombok"),
        ("com.google.dagger", "dagger"),
        ("org.mapstruct", "mapstruct"),
        ("io.micronaut", "micronaut-runtime"),
        ("io.quarkus", "quarkus-arc"),
    ];

    for (group, artifact) in cases {
        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        db.add_dependency(project, group, artifact);

        let any_applies = analyzers.iter().any(|a| a.applies_to(&db, project));
        assert!(
            any_applies,
            "expected at least one builtin analyzer to apply for {group}:{artifact}"
        );
    }
}

