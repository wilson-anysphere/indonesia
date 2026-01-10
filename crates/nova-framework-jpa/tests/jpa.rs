use nova_framework_jpa::{
    analyze_java_sources, is_jpa_applicable_with_classpath, jpql_completions,
    jpql_completions_in_java_source,
};
use pretty_assertions::assert_eq;

#[test]
fn entity_detection_extracts_table_and_fields() {
    let src = r#"
        package demo;

        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.Table;
        import jakarta.persistence.Transient;

        @Entity
        @Table(name = "users")
        public class User {
            @Id
            private Long id;

            private String name;

            @Transient
            private String scratch;
        }
    "#;

    let analysis = analyze_java_sources(&[src]);
    let user = analysis.model.entity("User").expect("User entity missing");

    assert_eq!(user.table, "users");
    let field_names: Vec<_> = user.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(field_names, vec!["id", "name"]);
}

#[test]
fn missing_id_emits_diagnostic() {
    let src = r#"
        import jakarta.persistence.Entity;

        @Entity
        public class NoId {
            private String name;
        }
    "#;

    let analysis = analyze_java_sources(&[src]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code == "JPA_MISSING_ID"),
        "expected JPA_MISSING_ID diagnostic, got: {:#?}",
        analysis.diagnostics
    );
}

#[test]
fn relationship_parsing_and_mappedby_validation() {
    let user = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.OneToMany;
        import java.util.List;

        @Entity
        public class User {
            @Id
            private Long id;

            @OneToMany(mappedBy = "user")
            private List<Post> posts;
        }
    "#;

    let post = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.ManyToOne;

        @Entity
        public class Post {
            @Id
            private Long id;

            @ManyToOne
            private User user;
        }
    "#;

    let analysis = analyze_java_sources(&[user, post]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| d.severity != nova_framework_jpa::Severity::Error),
        "unexpected error diagnostics: {:#?}",
        analysis.diagnostics
    );

    let user_entity = analysis.model.entity("User").unwrap();
    let posts = user_entity.field_named("posts").unwrap();
    let rel = posts
        .relationship
        .as_ref()
        .expect("posts should be relationship");

    assert_eq!(format!("{:?}", rel.kind), "OneToMany");
    assert_eq!(rel.target_entity.as_deref(), Some("Post"));
    assert_eq!(rel.mapped_by.as_deref(), Some("user"));
}

#[test]
fn relationship_target_entity_attribute_is_respected() {
    let user = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.OneToMany;
        import java.util.List;

        @Entity
        public class User {
            @Id
            private Long id;

            @OneToMany(targetEntity = Post.class, mappedBy = "user")
            private List posts;
        }
    "#;

    let post = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.ManyToOne;

        @Entity
        public class Post {
            @Id
            private Long id;

            @ManyToOne
            private User user;
        }
    "#;

    let analysis = analyze_java_sources(&[user, post]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| d.severity != nova_framework_jpa::Severity::Error),
        "unexpected error diagnostics: {:#?}",
        analysis.diagnostics
    );

    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.code == "JPA_REL_TARGET_UNKNOWN"),
        "unexpected JPA_REL_TARGET_UNKNOWN diagnostics: {:#?}",
        analysis.diagnostics
    );

    let user_entity = analysis.model.entity("User").unwrap();
    let posts = user_entity.field_named("posts").unwrap();
    let rel = posts
        .relationship
        .as_ref()
        .expect("posts should be relationship");

    assert_eq!(rel.target_entity.as_deref(), Some("Post"));
}

#[test]
fn jpql_completion_suggests_entities_and_fields() {
    let src = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id private Long id;
            private String name;
        }
    "#;
    let analysis = analyze_java_sources(&[src]);

    // Entity completion after FROM
    let q1 = "SELECT u FROM ";
    let items = jpql_completions(q1, q1.len(), &analysis.model);
    assert!(items.iter().any(|i| i.label == "User"));

    // Field completion after alias.
    let q2 = "SELECT u FROM User u WHERE u.";
    let items = jpql_completions(q2, q2.len(), &analysis.model);
    assert!(items.iter().any(|i| i.label == "name"));
    assert!(items.iter().any(|i| i.label == "id"));
}

#[test]
fn jpql_completion_handles_as_alias() {
    let src = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id private Long id;
            private String name;
        }
    "#;
    let analysis = analyze_java_sources(&[src]);

    let query = "SELECT u FROM User AS u WHERE u.";
    let items = jpql_completions(query, query.len(), &analysis.model);
    assert!(items.iter().any(|i| i.label == "id"));
    assert!(items.iter().any(|i| i.label == "name"));
}

#[test]
fn jpql_completion_handles_join_alias() {
    let user = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.OneToMany;
        import java.util.List;

        @Entity
        public class User {
            @Id private Long id;

            @OneToMany(mappedBy = "user")
            private List<Post> posts;
        }
    "#;

    let post = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.ManyToOne;

        @Entity
        public class Post {
            @Id private Long id;

            private String title;

            @ManyToOne
            private User user;
        }
    "#;

    let analysis = analyze_java_sources(&[user, post]);

    let query = "SELECT p FROM User u JOIN u.posts AS p WHERE p.";
    let items = jpql_completions(query, query.len(), &analysis.model);
    assert!(items.iter().any(|i| i.label == "id"));
    assert!(items.iter().any(|i| i.label == "title"));
}

#[test]
fn jpql_completion_handles_nested_paths() {
    let user = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id private Long id;
            private String name;
        }
    "#;

    let post = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.ManyToOne;

        @Entity
        public class Post {
            @Id private Long id;

            @ManyToOne
            private User user;
        }
    "#;

    let analysis = analyze_java_sources(&[user, post]);

    let query = "SELECT p FROM Post p WHERE p.user.";
    let items = jpql_completions(query, query.len(), &analysis.model);
    assert!(items.iter().any(|i| i.label == "id"));
    assert!(items.iter().any(|i| i.label == "name"));

    let diags = nova_framework_jpa::jpql_diagnostics(
        "SELECT p FROM Post p WHERE p.user.name = 'x'",
        &analysis.model,
    );
    assert!(
        !diags.iter().any(|d| d.code == "JPQL_UNKNOWN_ALIAS"),
        "unexpected alias diagnostics: {diags:#?}"
    );
}

#[test]
fn jpql_diagnostics_are_mapped_to_java_source_spans() {
    let entity = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id private Long id;
        }
    "#;

    let repo = r#"
        import org.springframework.data.jpa.repository.Query;

        public interface UserRepo {
            @Query("SELECT u FROM Unknown u")
            void load();
        }
    "#;

    let analysis = analyze_java_sources(&[entity, repo]);
    let diag = analysis
        .diagnostics
        .iter()
        .find(|d| d.code == "JPQL_UNKNOWN_ENTITY")
        .expect("expected JPQL_UNKNOWN_ENTITY diagnostic");

    let span = diag.span.expect("expected diagnostic span");
    assert_eq!(&repo[span.start..span.end], "Unknown");
}

#[test]
fn jpql_completion_works_inside_java_source_strings() {
    let entity = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id private Long id;
            private String name;
        }
    "#;

    let repo = r#"
        import org.springframework.data.jpa.repository.Query;

        public interface UserRepo {
            @Query("SELECT u FROM User u WHERE u.")
            void load();
        }
    "#;

    let analysis = analyze_java_sources(&[entity, repo]);
    let cursor = repo.find("u.").unwrap() + 2;
    let items = jpql_completions_in_java_source(repo, cursor, &analysis.model);

    assert!(items.iter().any(|i| i.label == "id"));
    assert!(items.iter().any(|i| i.label == "name"));
}

#[test]
fn jpql_completion_supports_query_value_parameter() {
    let entity = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class User {
            @Id private Long id;
            private String name;
        }
    "#;

    let repo = r#"
        import org.springframework.data.jpa.repository.Query;

        public interface UserRepo {
            @Query(nativeQuery = false, value = "SELECT u FROM User u WHERE u.")
            void load();
        }
    "#;

    let analysis = analyze_java_sources(&[entity, repo]);
    let cursor = repo.find("u.").unwrap() + 2;
    let items = jpql_completions_in_java_source(repo, cursor, &analysis.model);

    assert!(items.iter().any(|i| i.label == "id"));
    assert!(items.iter().any(|i| i.label == "name"));
}

#[test]
fn jpql_support_respects_entity_name_override() {
    let entity = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity(name = "Accounts")
        public class User {
            @Id private Long id;
            private String name;
        }
    "#;

    let analysis = analyze_java_sources(&[entity]);

    // Entity completion should use the JPQL entity name.
    let q1 = "SELECT a FROM ";
    let items = jpql_completions(q1, q1.len(), &analysis.model);
    assert!(items.iter().any(|i| i.label == "Accounts"));

    // Field completion should still work via alias->class resolution.
    let q2 = "SELECT a FROM Accounts a WHERE a.";
    let items = jpql_completions(q2, q2.len(), &analysis.model);
    assert!(items.iter().any(|i| i.label == "id"));
    assert!(items.iter().any(|i| i.label == "name"));
}

#[test]
fn invalid_relationship_target_type_emits_diagnostic() {
    let user = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;
        import jakarta.persistence.OneToMany;

        @Entity
        public class User {
            @Id private Long id;

            // @OneToMany should be a collection type.
            @OneToMany
            private Post posts;
        }
    "#;

    let post = r#"
        import jakarta.persistence.Entity;
        import jakarta.persistence.Id;

        @Entity
        public class Post {
            @Id private Long id;
        }
    "#;

    let analysis = analyze_java_sources(&[user, post]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code == "JPA_REL_INVALID_TARGET_TYPE"),
        "expected JPA_REL_INVALID_TARGET_TYPE diagnostic, got: {:#?}",
        analysis.diagnostics
    );
}

#[test]
fn applicability_detects_jpa_on_classpath_directory() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir
        .path()
        .join("jakarta")
        .join("persistence")
        .join("Entity.class");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, b"").unwrap();

    let classpath = vec![dir.path()];
    assert!(is_jpa_applicable_with_classpath(&[], &classpath, &[]));
}

#[test]
fn applicability_detects_jpa_in_jar_classpath_entry() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let jar_path = dir.path().join("jpa.jar");

    let file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file(
        "jakarta/persistence/Entity.class",
        zip::write::FileOptions::default(),
    )
    .unwrap();
    zip.write_all(b"").unwrap();
    zip.finish().unwrap();

    let classpath = vec![jar_path.as_path()];
    assert!(is_jpa_applicable_with_classpath(&[], &classpath, &[]));
}
