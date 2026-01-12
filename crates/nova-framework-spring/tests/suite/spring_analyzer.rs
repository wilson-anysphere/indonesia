use std::path::{Path, PathBuf};

use nova_core::FileId;
use nova_framework::Database;
use nova_hir::framework::ClassData;
use nova_types::ClassId;

use nova_framework::FrameworkData;
use nova_framework::{AnalyzerRegistry, CompletionContext, MemoryDatabase};
use nova_framework_spring::{
    completion_span_for_value_placeholder, SpringAnalyzer, SPRING_NO_BEAN,
};
use tempfile::TempDir;

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Database wrapper used to verify analyzer fallbacks when the database does not
/// support project-wide file enumeration.
struct NoAllFilesDb {
    inner: MemoryDatabase,
}

impl Database for NoAllFilesDb {
    fn class(&self, class: ClassId) -> &ClassData {
        self.inner.class(class)
    }

    fn project_of_class(&self, class: ClassId) -> nova_core::ProjectId {
        self.inner.project_of_class(class)
    }

    fn project_of_file(&self, file: FileId) -> nova_core::ProjectId {
        self.inner.project_of_file(file)
    }

    fn file_text(&self, file: FileId) -> Option<&str> {
        self.inner.file_text(file)
    }

    fn file_path(&self, file: FileId) -> Option<&Path> {
        self.inner.file_path(file)
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.inner.file_id(path)
    }

    fn all_files(&self, _project: nova_core::ProjectId) -> Vec<FileId> {
        Vec::new()
    }

    fn all_classes(&self, _project: nova_core::ProjectId) -> Vec<ClassId> {
        Vec::new()
    }

    fn has_dependency(&self, project: nova_core::ProjectId, group: &str, artifact: &str) -> bool {
        self.inner.has_dependency(project, group, artifact)
    }

    fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
        self.inner.has_class_on_classpath(project, binary_name)
    }

    fn has_class_on_classpath_prefix(&self, project: nova_core::ProjectId, prefix: &str) -> bool {
        self.inner.has_class_on_classpath_prefix(project, prefix)
    }
}

#[test]
fn applies_to_turns_on_with_spring_marker() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();

    let java = r#"
        import org.springframework.beans.factory.annotation.Autowired;
        import org.springframework.stereotype.Component;

        @Component
        class Main {
            @Autowired Missing missing;
        }

        class Missing {}
    "#;
    let file = db.add_file_with_path_and_text(project, "src/Main.java", java);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    // Without a Spring marker the analyzer should not apply.
    assert!(registry.framework_diagnostics(&db, file).is_empty());

    // Classpath marker should activate the analyzer.
    db.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let diags = registry.framework_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == SPRING_NO_BEAN),
        "expected SPRING_NO_BEAN; got {diags:#?}"
    );
}

#[test]
fn di_diagnostics_are_filtered_per_file() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let foo = r#"
        import org.springframework.stereotype.Component;

        @Component
        class Foo {}
    "#;
    let foo_file = db.add_file_with_path_and_text(project, "src/Foo.java", foo);

    let bar = r#"
        import org.springframework.beans.factory.annotation.Autowired;
        import org.springframework.stereotype.Component;

        @Component
        class Bar {
            @Autowired Foo foo;
            @Autowired Missing missing;
        }

        class Missing {}
    "#;
    let bar_file = db.add_file_with_path_and_text(project, "src/Bar.java", bar);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let foo_diags = registry.framework_diagnostics(&db, foo_file);
    assert!(
        foo_diags.is_empty(),
        "expected Foo.java to have no diagnostics; got {foo_diags:#?}"
    );

    let bar_diags = registry.framework_diagnostics(&db, bar_file);
    assert_eq!(
        bar_diags.len(),
        1,
        "expected exactly one diagnostic in Bar.java (Missing), got {bar_diags:#?}"
    );
    assert_eq!(bar_diags[0].code.as_ref(), SPRING_NO_BEAN);
    assert!(
        bar_diags[0].message.contains("Missing"),
        "expected Missing to be mentioned; got {:?}",
        bar_diags[0].message
    );
    assert!(
        !bar_diags[0].message.contains("Foo"),
        "expected Foo injection to resolve via project-wide analysis; got {:?}",
        bar_diags[0].message
    );
}

#[test]
fn di_diagnostics_read_unopened_sources_from_disk_when_file_text_is_unavailable() {
    struct MissingTextDb {
        inner: MemoryDatabase,
        missing: FileId,
    }

    impl Database for MissingTextDb {
        fn class(&self, class: ClassId) -> &ClassData {
            self.inner.class(class)
        }

        fn project_of_class(&self, class: ClassId) -> nova_core::ProjectId {
            self.inner.project_of_class(class)
        }

        fn project_of_file(&self, file: FileId) -> nova_core::ProjectId {
            self.inner.project_of_file(file)
        }

        fn file_text(&self, file: FileId) -> Option<&str> {
            if file == self.missing {
                return None;
            }
            self.inner.file_text(file)
        }

        fn file_path(&self, file: FileId) -> Option<&Path> {
            self.inner.file_path(file)
        }

        fn file_id(&self, path: &Path) -> Option<FileId> {
            self.inner.file_id(path)
        }

        fn all_files(&self, project: nova_core::ProjectId) -> Vec<FileId> {
            self.inner.all_files(project)
        }

        fn all_classes(&self, project: nova_core::ProjectId) -> Vec<ClassId> {
            self.inner.all_classes(project)
        }

        fn has_dependency(&self, project: nova_core::ProjectId, group: &str, artifact: &str) -> bool {
            self.inner.has_dependency(project, group, artifact)
        }

        fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
            self.inner.has_class_on_classpath(project, binary_name)
        }

        fn has_class_on_classpath_prefix(&self, project: nova_core::ProjectId, prefix: &str) -> bool {
            self.inner.has_class_on_classpath_prefix(project, prefix)
        }
    }

    let temp = TempDir::new().unwrap();
    let root: PathBuf = temp.path().canonicalize().unwrap();

    // Ensure files exist on disk so the analyzer can fall back to filesystem reads.
    let foo_src = r#"
        import org.springframework.stereotype.Component;

        @Component
        class Foo {}
    "#;
    let bar_src = r#"
        import org.springframework.beans.factory.annotation.Autowired;
        import org.springframework.stereotype.Component;

        @Component
        class Bar {
            @Autowired Foo foo;
        }
    "#;

    let foo_path = root.join("src/main/java/demo/Foo.java");
    let bar_path = root.join("src/main/java/demo/Bar.java");
    write_file(&foo_path, foo_src);
    write_file(&bar_path, bar_src);

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let foo_file = inner.add_file_with_path_and_text(project, foo_path, foo_src);
    let bar_file = inner.add_file_with_path_and_text(project, bar_path, bar_src);

    let db = MissingTextDb {
        inner,
        missing: foo_file,
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let bar_diags = registry.framework_diagnostics(&db, bar_file);
    assert!(
        bar_diags.iter().all(|d| d.code.as_ref() != SPRING_NO_BEAN),
        "expected Foo bean to resolve via disk-backed analysis even when file_text is missing; got {bar_diags:#?}"
    );
}

#[test]
fn value_completions_include_application_properties_key_and_set_replace_span() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let config = "server.port=8080\n";
    db.add_file_with_path_and_text(project, "src/main/resources/application.properties", config);

    let java = r#"
        import org.springframework.beans.factory.annotation.Value;
        class App {
            @Value("${server.p}")
            String port;
        }
    "#;
    let java_file = db.add_file_with_path_and_text(project, "src/App.java", java);

    let offset = java.find("${server.p}").unwrap() + "${server.p".len();
    let expected_span = completion_span_for_value_placeholder(java, offset).expect("span");

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: java_file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    let item = items
        .iter()
        .find(|i| i.label == "server.port")
        .expect("expected server.port completion item");
    assert_eq!(item.replace_span, Some(expected_span));
}

#[test]
fn value_completions_read_application_properties_from_disk_when_file_text_is_unavailable() {
    struct NoConfigTextDb {
        inner: MemoryDatabase,
        hidden: FileId,
    }

    impl Database for NoConfigTextDb {
        fn class(&self, class: ClassId) -> &ClassData {
            self.inner.class(class)
        }

        fn project_of_class(&self, class: ClassId) -> nova_core::ProjectId {
            self.inner.project_of_class(class)
        }

        fn project_of_file(&self, file: FileId) -> nova_core::ProjectId {
            self.inner.project_of_file(file)
        }

        fn file_text(&self, file: FileId) -> Option<&str> {
            if file == self.hidden {
                return None;
            }
            self.inner.file_text(file)
        }

        fn file_path(&self, file: FileId) -> Option<&Path> {
            self.inner.file_path(file)
        }

        fn file_id(&self, path: &Path) -> Option<FileId> {
            self.inner.file_id(path)
        }

        fn all_files(&self, project: nova_core::ProjectId) -> Vec<FileId> {
            self.inner.all_files(project)
        }

        fn all_classes(&self, project: nova_core::ProjectId) -> Vec<ClassId> {
            self.inner.all_classes(project)
        }

        fn has_dependency(
            &self,
            project: nova_core::ProjectId,
            group: &str,
            artifact: &str,
        ) -> bool {
            self.inner.has_dependency(project, group, artifact)
        }

        fn has_class_on_classpath(&self, project: nova_core::ProjectId, binary_name: &str) -> bool {
            self.inner.has_class_on_classpath(project, binary_name)
        }

        fn has_class_on_classpath_prefix(
            &self,
            project: nova_core::ProjectId,
            prefix: &str,
        ) -> bool {
            self.inner.has_class_on_classpath_prefix(project, prefix)
        }
    }

    let temp = TempDir::new().unwrap();
    let root: PathBuf = temp.path().canonicalize().unwrap();

    let config_path = root.join("src/main/resources/application.properties");
    write_file(&config_path, "server.port=8080\n");

    let java = r#"
        import org.springframework.beans.factory.annotation.Value;
        class App {
            @Value("${server.p}")
            String port;
        }
    "#;
    let java_path = root.join("src/App.java");
    write_file(&java_path, java);

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let config_file = inner.add_file_with_path_and_text(project, config_path, "server.port=8080\n");
    let java_file = inner.add_file_with_path_and_text(project, java_path, java);

    let offset = java.find("${server.p}").unwrap() + "${server.p".len();
    let expected_span = completion_span_for_value_placeholder(java, offset).expect("span");

    let db = NoConfigTextDb {
        inner,
        hidden: config_file,
    };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: java_file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    let item = items
        .iter()
        .find(|i| i.label == "server.port")
        .expect("expected server.port completion item");
    assert_eq!(item.replace_span, Some(expected_span));
}

#[test]
fn value_completions_scan_disk_configs_when_all_files_is_unavailable() {
    let temp = TempDir::new().unwrap();
    let root: PathBuf = temp.path().canonicalize().unwrap();

    // Ensure the analyzer's best-effort workspace root discovery can locate the workspace root from
    // the Java file path (so it can scan `application*.properties` on disk).
    std::fs::write(root.join("pom.xml"), "<project></project>").expect("write pom.xml");

    let config_path = root.join("src/main/resources/application.properties");
    write_file(&config_path, "server.port=8080\n");

    let java = r#"
        import org.springframework.beans.factory.annotation.Value;
        class App {
            @Value("${server.p}")
            String port;
        }
    "#;
    let java_path = root.join("src/App.java");
    write_file(&java_path, java);

    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let java_file = inner.add_file_with_path_and_text(project, java_path, java);

    let offset = java.find("${server.p}").unwrap() + "${server.p".len();
    let expected_span = completion_span_for_value_placeholder(java, offset).expect("span");

    let db = NoAllFilesDb { inner };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file: java_file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    let item = items
        .iter()
        .find(|i| i.label == "server.port")
        .expect("expected server.port completion item");
    assert_eq!(item.replace_span, Some(expected_span));
}

#[test]
fn di_diagnostics_fallback_to_current_file_when_all_files_is_unavailable() {
    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let java = r#"
        import org.springframework.beans.factory.annotation.Autowired;
        import org.springframework.stereotype.Component;

        @Component
        class Main {
            @Autowired Missing missing;
        }

        class Missing {}
    "#;
    let file = inner.add_file_with_path_and_text(project, "src/Main.java", java);

    let db = NoAllFilesDb { inner };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let diags = registry.framework_diagnostics(&db, file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == SPRING_NO_BEAN),
        "expected SPRING_NO_BEAN; got {diags:#?}"
    );
}

#[test]
fn qualifier_completions_fallback_to_current_file_when_all_files_is_unavailable() {
    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let java = r#"
        import org.springframework.beans.factory.annotation.Qualifier;
        import org.springframework.stereotype.Component;

        @Component
        class Foo {}

        @Component
        class Bar {
            @Qualifier("f")
            Foo foo;
        }
    "#;
    let file = inner.add_file_with_path_and_text(project, "src/Main.java", java);

    let offset = java.find("@Qualifier(\"f\")").unwrap() + "@Qualifier(\"".len() + 1;
    let db = NoAllFilesDb { inner };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);
    assert!(
        items.iter().any(|i| i.label == "foo"),
        "expected qualifier completion for bean name 'foo'; got {items:?}"
    );
}

#[test]
fn analyze_file_returns_spring_framework_data_for_component_beans() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    let java = r#"
        import org.springframework.stereotype.Component;

        @Component
        class Foo {}
    "#;
    let file = db.add_file_with_path_and_text(project, "src/Foo.java", java);

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let data = registry.framework_data(&db, file);
    let spring = data
        .iter()
        .find_map(|d| match d {
            FrameworkData::Spring(s) => Some(s),
            _ => None,
        })
        .expect("expected FrameworkData::Spring");

    assert!(
        spring.beans.iter().any(|b| b.name == "foo"),
        "expected bean named 'foo'; got {:?}",
        spring.beans
    );
}

#[test]
fn profile_completions_include_application_profile_configs() {
    let mut db = MemoryDatabase::new();
    let project = db.add_project();
    db.add_dependency(project, "org.springframework", "spring-context");

    db.add_file_with_path_and_text(
        project,
        "src/main/resources/application-qa.properties",
        "server.port=8080\n",
    );

    let java = r#"
        import org.springframework.context.annotation.Profile;

        @Profile("q")
        class C {}
    "#;
    let file = db.add_file_with_path_and_text(project, "src/C.java", java);

    let offset = java.find("@Profile(\"q\")").unwrap() + "@Profile(\"".len() + 1;

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    assert!(
        items.iter().any(|i| i.label == "qa"),
        "expected profile completion for 'qa'; got {items:?}"
    );
}

#[test]
fn profile_completions_fallback_to_current_file_when_all_files_is_unavailable() {
    let mut inner = MemoryDatabase::new();
    let project = inner.add_project();
    inner.add_classpath_class(project, "org.springframework.context.ApplicationContext");

    let java = r#"
        import org.springframework.context.annotation.Profile;
        import org.springframework.stereotype.Component;

        @Component
        @Profile("qa")
        class Foo {}

        @Profile("q")
        class C {}
    "#;
    let file = inner.add_file_with_path_and_text(project, "src/C.java", java);

    let offset = java.find("@Profile(\"q\")").unwrap() + "@Profile(\"".len() + 1;
    let db = NoAllFilesDb { inner };

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(SpringAnalyzer::new()));

    let ctx = CompletionContext {
        project,
        file,
        offset,
    };
    let items = registry.framework_completions(&db, &ctx);

    assert!(
        items.iter().any(|i| i.label == "qa"),
        "expected profile completion for 'qa'; got {items:?}"
    );
}
