use nova_framework::{AnalyzerRegistry, CompletionContext, MemoryDatabase};
use nova_framework_jpa::{JpaAnalyzer, JPA_MISSING_ID};

#[test]
fn analyzer_missing_id_diagnostic_is_scoped_to_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "jakarta.persistence", "jakarta.persistence-api");

    let no_id_src = r#"
        import jakarta.persistence.Entity;

        @Entity
        public class NoId {
            private String name;
        }
    "#;

    let ok_src = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id
            private Long id;
        }
    "#;

    let no_id_file =
        db.add_file_with_path_and_text(project, "src/main/java/demo/NoId.java", no_id_src);
    let ok_file = db.add_file_with_path_and_text(project, "src/main/java/demo/User.java", ok_src);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(JpaAnalyzer::new()));

    let diags_no_id = registry.framework_diagnostics(&db, no_id_file);
    assert!(
        diags_no_id.iter().any(|d| d.code.as_ref() == JPA_MISSING_ID),
        "expected JPA_MISSING_ID in diagnostics; got {diags_no_id:#?}"
    );

    let diags_ok = registry.framework_diagnostics(&db, ok_file);
    assert!(
        !diags_ok.iter().any(|d| d.code.as_ref() == JPA_MISSING_ID),
        "expected no JPA_MISSING_ID for ok file; got {diags_ok:#?}"
    );
}

#[test]
fn analyzer_jpql_unknown_entity_diagnostic_has_span_on_entity_ident() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "jakarta.persistence", "jakarta.persistence-api");

    let entity_src = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id
            private Long id;
        }
    "#;

    let repo_src = r#"
        import org.springframework.data.jpa.repository.Query;

        public interface UserRepo {
            @Query("SELECT u FROM Unknown u")
            void load();
        }
    "#;

    let _entity_file =
        db.add_file_with_path_and_text(project, "src/main/java/demo/User.java", entity_src);
    let repo_file =
        db.add_file_with_path_and_text(project, "src/main/java/demo/UserRepo.java", repo_src);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(JpaAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, repo_file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "JPQL_UNKNOWN_ENTITY")
        .expect("expected JPQL_UNKNOWN_ENTITY diagnostic");

    let span = diag.span.expect("expected diagnostic span");
    assert_eq!(
        &repo_src[span.start..span.end],
        "Unknown",
        "expected diagnostic span to cover Unknown identifier; got {span:?}"
    );
}

#[test]
fn analyzer_jpql_completions_include_entity_fields() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "jakarta.persistence", "jakarta.persistence-api");

    let entity_src = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id
            private Long id;
            private String name;
        }
    "#;

    let repo_src = r#"
        import org.springframework.data.jpa.repository.Query;

        public interface UserRepo {
            @Query("SELECT u FROM User u WHERE u.")
            void load();
        }
    "#;

    let _entity_file =
        db.add_file_with_path_and_text(project, "src/main/java/demo/User.java", entity_src);
    let repo_file =
        db.add_file_with_path_and_text(project, "src/main/java/demo/UserRepo.java", repo_src);

    let cursor = repo_src
        .find("u.")
        .expect("expected `u.` in repository fixture")
        + 2;

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(JpaAnalyzer::new()));

    let items = registry.framework_completions(
        &db,
        &CompletionContext {
            project,
            file: repo_file,
            offset: cursor,
        },
    );

    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"id"), "expected `id` completion; got {labels:?}");
    assert!(
        labels.contains(&"name"),
        "expected `name` completion; got {labels:?}"
    );
}

