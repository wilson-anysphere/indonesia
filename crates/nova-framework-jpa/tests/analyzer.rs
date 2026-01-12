use nova_framework::{AnalyzerRegistry, CompletionContext, FrameworkAnalyzer, MemoryDatabase};
use nova_framework_jpa::{JpaAnalyzer, JPA_MISSING_ID};

#[test]
fn applies_to_turns_on_with_jpa_marker() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    db.add_classpath_class(project, "jakarta.persistence.Entity");

    let analyzer = JpaAnalyzer::new();
    assert!(analyzer.applies_to(&db, project));
}

#[test]
fn diagnostics_report_missing_id() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    db.add_classpath_class(project, "jakarta.persistence.Entity");

    let src = r#"
        import jakarta.persistence.Entity;

        @Entity
        public class User {
            private String name;
        }
    "#;

    let file = db.add_file_with_path_and_text(project, "src/User.java", src);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(JpaAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == JPA_MISSING_ID),
        "expected {JPA_MISSING_ID} diagnostic, got: {diags:#?}"
    );
}

#[test]
fn completions_offer_entity_fields_inside_query_strings() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    db.add_classpath_class(project, "jakarta.persistence.Entity");

    let entity_src = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id private Long id;
            private String name;
        }
    "#;
    db.add_file_with_path_and_text(project, "src/User.java", entity_src);

    let repo_src = r#"
        import org.springframework.data.jpa.repository.Query;

        public interface UserRepo {
            @Query("SELECT u FROM User u WHERE u.")
            void load();
        }
    "#;
    let repo_file = db.add_file_with_path_and_text(project, "src/UserRepo.java", repo_src);

    let cursor = repo_src.find("u.").unwrap() + 2;
    let ctx = CompletionContext {
        project,
        file: repo_file,
        offset: cursor,
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(JpaAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|i| i.label == "id"),
        "expected `id` completion, got: {items:#?}"
    );
    assert!(
        items.iter().any(|i| i.label == "name"),
        "expected `name` completion, got: {items:#?}"
    );
}

