use nova_framework::{AnalyzerRegistry, CompletionContext, MemoryDatabase};
use nova_framework_quarkus::{QuarkusAnalyzer, CDI_UNSATISFIED_CODE};

use std::path::Path;
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Adapter around `MemoryDatabase` that simulates a host that cannot enumerate
/// `Database::all_files(project)` (which is optional per the framework DB contract).
struct NoAllFilesDb {
    inner: MemoryDatabase,
}

impl nova_framework::Database for NoAllFilesDb {
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
        nova_framework::Database::file_text(&self.inner, file)
    }

    fn file_path(&self, file: nova_core::FileId) -> Option<&std::path::Path> {
        nova_framework::Database::file_path(&self.inner, file)
    }

    fn file_id(&self, path: &std::path::Path) -> Option<nova_core::FileId> {
        nova_framework::Database::file_id(&self.inner, path)
    }

    fn all_files(&self, _project: nova_core::ProjectId) -> Vec<nova_core::FileId> {
        Vec::new()
    }

    fn has_dependency(&self, project: nova_core::ProjectId, group: &str, artifact: &str) -> bool {
        nova_framework::Database::has_dependency(&self.inner, project, group, artifact)
    }

    fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
        nova_framework::Database::has_class_on_classpath(&self.inner, project, binary_name)
    }

    fn has_class_on_classpath_prefix(&self, project: nova_core::ProjectId, prefix: &str) -> bool {
        nova_framework::Database::has_class_on_classpath_prefix(&self.inner, project, prefix)
    }
}

#[test]
fn registry_reports_cdi_diagnostics_for_the_correct_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-arc");

    let file_with_issue = db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/ServiceA.java",
        r#"
            import jakarta.enterprise.context.ApplicationScoped;
            import jakarta.inject.Inject;

            @ApplicationScoped
            public class ServiceA {
              @Inject ServiceB missing;
            }
        "#,
    );

    let other_file = db.add_file_with_path_and_text(
        project,
        "src/main/java/com/example/Other.java",
        r#"
            public class Other {}
        "#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let file_diags = registry.framework_diagnostics(&db, file_with_issue);
    assert!(
        file_diags.iter().any(|d| d.code == CDI_UNSATISFIED_CODE),
        "expected {CDI_UNSATISFIED_CODE} diagnostic, got: {file_diags:#?}",
    );

    let other_diags = registry.framework_diagnostics(&db, other_file);
    assert!(
        other_diags.iter().all(|d| d.code != CDI_UNSATISFIED_CODE),
        "did not expect {CDI_UNSATISFIED_CODE} diagnostic for other file, got: {other_diags:#?}",
    );
}

#[test]
fn registry_skips_cdi_diagnostics_for_non_java_files() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-arc");

    // Deliberately Java-like content in a non-Java file.
    let not_java = db.add_file_with_path_and_text(
        project,
        "src/main/resources/not-java.txt",
        r#"
            import jakarta.enterprise.context.ApplicationScoped;
            import jakarta.inject.Inject;

            @ApplicationScoped
            public class ServiceA {
              @Inject ServiceB missing;
            }
        "#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, not_java);
    assert!(diags.is_empty(), "expected no diagnostics, got: {diags:#?}");
}

#[test]
fn registry_skips_config_property_completions_for_non_java_files() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;

    let file = db.add_file_with_path_and_text(project, "src/main/resources/not-java.txt", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        "server.port=8080",
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file,
        offset: cursor_base + 3, // after `ser`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.is_empty(),
        "expected no completions for non-java file, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_from_application_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        r#"
            server.port=8080
        "#,
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 3, // after `ser`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port, got: {items:#?}",
    );

    let item = items
        .iter()
        .find(|c| c.label == "server.port")
        .expect("expected server.port completion item");
    assert_eq!(
        item.replace_span,
        Some(nova_types::Span::new(cursor_base, cursor_base + 3))
    );
}

#[test]
fn registry_completes_config_property_names_from_application_profile_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application-dev.properties",
        "server.port=8080",
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 3, // after `ser`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port from application-dev.properties, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_from_application_yaml() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.yaml",
        r#"
            server:
              port: 8080
        "#,
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 3, // after `ser`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port from application.yaml, got: {items:#?}",
    );
}

#[test]
fn registry_parses_properties_keys_with_colon_separator() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        "server.port: 8080",
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 3, // after `ser`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port, got: {items:#?}",
    );
    assert!(
        items.iter().all(|c| c.label != "server.port: 8080"),
        "did not expect raw key with value to be treated as a property name, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_from_microprofile_config_properties() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/META-INF/microprofile-config.properties",
        "server.port=8080",
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 3, // after `ser`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port from microprofile-config.properties, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_for_virtual_java_buffers() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    // Simulate an editor buffer without a known on-disk path.
    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_text(project, src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        r#"
            server.port=8080
        "#,
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 3, // after `ser`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_for_fully_qualified_annotation() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        public class MyConfig {
          @org.eclipse.microprofile.config.inject.ConfigProperty(name="qu")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        r#"
            quarkus.http.port=8080
        "#,
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 2, // after `qu`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "quarkus.http.port"),
        "expected completion for quarkus.http.port, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_when_name_is_not_first_argument() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          // Default value contains an `@` and there's also a `@` inside a comment to ensure
          // completion context detection ignores annotation-unrelated `@` chars while locating the
          // preceding `@ConfigProperty`.
          @ConfigProperty(defaultValue="@8080", /* @junk */ name="qu")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        r#"
            quarkus.http.port=8080
        "#,
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 2, // after `qu`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "quarkus.http.port"),
        "expected completion for quarkus.http.port, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_in_unterminated_string() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    // Note: missing closing quote after `qu` (common while typing).
    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="qu
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        r#"
            quarkus.http.port=8080
        "#,
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 2, // after `qu`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "quarkus.http.port"),
        "expected completion for quarkus.http.port, got: {items:#?}",
    );
}

#[test]
fn registry_does_not_complete_when_annotation_is_not_config_property_even_if_comment_mentions_it() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        public @interface Something {
          String name();
        }

        public class MyConfig {
          @Something(name="qu") /* @ConfigProperty(name="should-not-trigger") */
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        r#"
            quarkus.http.port=8080
        "#,
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find annotation name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 2, // after `qu`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.is_empty(),
        "expected no completions for non-ConfigProperty annotation, got: {items:#?}",
    );
}

#[test]
fn registry_skips_cdi_diagnostics_when_java_file_has_no_path() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-arc");

    let file_with_issue = db.add_file_with_text(
        project,
        r#"
            import jakarta.enterprise.context.ApplicationScoped;
            import jakarta.inject.Inject;

            @ApplicationScoped
            public class ServiceA {
              @Inject ServiceB missing;
            }
        "#,
    );

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, file_with_issue);
    assert!(diags.is_empty(), "expected no diagnostics, got: {diags:#?}");
}

#[test]
fn registry_completes_config_property_names_when_java_file_has_no_path() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="qu")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_text(project, src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        "quarkus.http.port=8080",
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 2, // after `qu`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "quarkus.http.port"),
        "expected completion for quarkus.http.port, got: {items:#?}",
    );

    let item = items
        .iter()
        .find(|c| c.label == "quarkus.http.port")
        .expect("expected quarkus.http.port completion item");
    assert_eq!(
        item.replace_span,
        Some(nova_types::Span::new(cursor_base, cursor_base + 2))
    );
}

#[test]
fn registry_completes_config_property_names_with_line_comment_inside_annotation_args() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(
            // @junk
            name="qu")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        "quarkus.http.port=8080",
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 2, // after `qu`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "quarkus.http.port"),
        "expected completion for quarkus.http.port, got: {items:#?}",
    );
}

#[test]
fn registry_updates_completions_after_config_file_changes() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;

    let java_file = db.add_file_with_path_and_text(project, "src/main/java/MyConfig.java", src);
    let config_file = db.add_file_with_path_and_text(
        project,
        "src/main/resources/application.properties",
        "server.port=8080",
    );

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 3, // after `ser`
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port, got: {items:#?}",
    );

    // Update the config file: cache should invalidate and completions should reflect the new key.
    db.set_file_text(config_file, "server.ssl.enabled=true");

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        !items.iter().any(|c| c.label == "server.port"),
        "did not expect server.port after config change, got: {items:#?}",
    );
    assert!(
        items.iter().any(|c| c.label == "server.ssl.enabled"),
        "expected completion for server.ssl.enabled after config change, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_without_db_file_enumeration() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();

    // Ensure `nova_project::workspace_root` can find the workspace root.
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    write_file(
        &root.join("src/main/resources/application.properties"),
        "server.port=8080\n",
    );

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="ser")
          String prop;
        }
    "#;
    let java_path = root.join("src/main/java/MyConfig.java");
    write_file(&java_path, src);

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let java_file = inner.add_file_with_path_and_text(project, java_path, src);

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + 3, // after `ser`
    };

    let db = NoAllFilesDb { inner };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port, got: {items:#?}",
    );
}

#[test]
fn registry_completes_config_property_names_when_some_config_file_text_is_unavailable() {
    struct MissingConfigTextDb {
        inner: MemoryDatabase,
        missing: nova_core::FileId,
    }

    impl nova_framework::Database for MissingConfigTextDb {
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

    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    let base_config_path = root.join("src/main/resources/application.properties");
    let dev_config_path = root.join("src/main/resources/application-dev.properties");
    write_file(&base_config_path, "server.port=8080\n");
    write_file(&dev_config_path, "server.ssl.enabled=true\n");

    let src = r#"
        import org.eclipse.microprofile.config.inject.ConfigProperty;

        public class MyConfig {
          @ConfigProperty(name="server.")
          String prop;
        }
    "#;
    let java_path = root.join("src/main/java/MyConfig.java");
    write_file(&java_path, src);

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_dependency(project, "io.quarkus", "quarkus-smallrye-config");

    let java_file = inner.add_file_with_path_and_text(project, java_path, src);
    let base_config_id =
        inner.add_file_with_path_and_text(project, base_config_path, "server.port=8080\n");
    inner.add_file_with_path_and_text(project, dev_config_path, "server.ssl.enabled=true\n");

    let cursor_base = src
        .find("name=\"")
        .expect("expected to find ConfigProperty name string")
        + "name=\"".len();
    let ctx = CompletionContext {
        project,
        file: java_file,
        offset: cursor_base + "server.".len(),
    };

    let db = MissingConfigTextDb {
        inner,
        missing: base_config_id,
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(QuarkusAnalyzer::new()));

    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|c| c.label == "server.port"),
        "expected completion for server.port from application.properties, got: {items:#?}",
    );
    assert!(
        items.iter().any(|c| c.label == "server.ssl.enabled"),
        "expected completion for server.ssl.enabled from application-dev.properties, got: {items:#?}",
    );
}
