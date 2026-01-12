use nova_framework::{AnalyzerRegistry, CompletionContext, FrameworkAnalyzer, MemoryDatabase};
use nova_framework_jpa::{JpaAnalyzer, JPA_MISSING_ID};
use std::path::Path;
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

#[test]
fn analyzer_applies_to_turns_on_with_jpa_marker() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    db.add_classpath_class(project, "jakarta.persistence.Entity");

    let analyzer = JpaAnalyzer::new();
    assert!(analyzer.applies_to(&db, project));
}

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
        diags_no_id
            .iter()
            .any(|d| d.code.as_ref() == JPA_MISSING_ID),
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
    assert!(
        labels.contains(&"id"),
        "expected `id` completion; got {labels:?}"
    );
    assert!(
        labels.contains(&"name"),
        "expected `name` completion; got {labels:?}"
    );
}

#[test]
fn analyzer_jpql_completions_include_entity_fields_when_entity_file_text_is_unavailable() {
    struct MissingTextDb {
        inner: MemoryDatabase,
        missing: nova_core::FileId,
    }

    impl nova_framework::Database for MissingTextDb {
        fn class(&self, class: nova_types::ClassId) -> &nova_hir::framework::ClassData {
            nova_framework::Database::class(&self.inner, class)
        }

        fn project_of_class(&self, class: nova_types::ClassId) -> nova_core::ProjectId {
            nova_framework::Database::project_of_class(&self.inner, class)
        }

        fn project_of_file(&self, file: nova_core::FileId) -> nova_core::ProjectId {
            nova_framework::Database::project_of_file(&self.inner, file)
        }

        fn file_text(&self, file: nova_core::FileId) -> Option<&str> {
            if file == self.missing {
                return None;
            }
            nova_framework::Database::file_text(&self.inner, file)
        }

        fn file_path(&self, file: nova_core::FileId) -> Option<&std::path::Path> {
            nova_framework::Database::file_path(&self.inner, file)
        }

        fn file_id(&self, path: &std::path::Path) -> Option<nova_core::FileId> {
            nova_framework::Database::file_id(&self.inner, path)
        }

        fn all_files(&self, project: nova_core::ProjectId) -> Vec<nova_core::FileId> {
            nova_framework::Database::all_files(&self.inner, project)
        }

        fn all_classes(&self, project: nova_core::ProjectId) -> Vec<nova_types::ClassId> {
            nova_framework::Database::all_classes(&self.inner, project)
        }

        fn has_dependency(
            &self,
            project: nova_core::ProjectId,
            group: &str,
            artifact: &str,
        ) -> bool {
            nova_framework::Database::has_dependency(&self.inner, project, group, artifact)
        }

        fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
            nova_framework::Database::has_class_on_classpath(&self.inner, project, binary_name)
        }

        fn has_class_on_classpath_prefix(
            &self,
            project: nova_core::ProjectId,
            prefix: &str,
        ) -> bool {
            nova_framework::Database::has_class_on_classpath_prefix(&self.inner, project, prefix)
        }
    }

    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

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

    let entity_path = root.join("src/main/java/demo/User.java");
    let repo_path = root.join("src/main/java/demo/UserRepo.java");
    write_file(&entity_path, entity_src);
    write_file(&repo_path, repo_src);

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_dependency(project, "jakarta.persistence", "jakarta.persistence-api");

    let entity_file = inner.add_file_with_path_and_text(project, entity_path, entity_src);
    let repo_file = inner.add_file_with_path_and_text(project, repo_path, repo_src);

    let cursor = repo_src
        .find("u.")
        .expect("expected `u.` in repository fixture")
        + 2;

    let db = MissingTextDb {
        inner,
        missing: entity_file,
    };

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
    assert!(
        labels.contains(&"id"),
        "expected `id` completion; got {labels:?}"
    );
    assert!(
        labels.contains(&"name"),
        "expected `name` completion; got {labels:?}"
    );
}
