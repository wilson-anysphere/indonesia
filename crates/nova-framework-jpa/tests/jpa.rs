use nova_framework_jpa::{analyze_java_sources, jpql_completions};
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
