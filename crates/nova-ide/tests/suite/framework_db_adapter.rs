use std::path::PathBuf;
use std::sync::Arc;

use nova_db::InMemoryFileStore;
use nova_ext::ProjectId;
use nova_framework::Database as FrameworkDatabase;
use nova_ide::extensions::FrameworkIdeDatabase;

#[test]
fn file_text_returns_underlying_text() {
    let mut db = InMemoryFileStore::new();
    let file_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let file = db.file_id_for_path(&file_path);
    db.set_file_text(file, "class Main {}".to_string());

    let inner: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let adapter = FrameworkIdeDatabase::new(inner, ProjectId::new(0));

    assert_eq!(adapter.file_text(file), Some("class Main {}"));
}

#[test]
fn class_parsing_extracts_annotations_and_fields() {
    let mut db = InMemoryFileStore::new();
    let file_path = PathBuf::from("/workspace/src/main/java/Foo.java");
    let file = db.file_id_for_path(&file_path);
    db.set_file_text(
        file,
        r#"import lombok.Getter;

@Getter
class Foo {
  @Deprecated
  private static final int X = 1;
}
"#
        .to_string(),
    );

    let inner: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let adapter = FrameworkIdeDatabase::new(inner, ProjectId::new(0));
    let project = ProjectId::new(0);

    let classes = adapter.all_classes(project);
    assert!(
        !classes.is_empty(),
        "expected at least one extracted class; got {classes:?}"
    );

    let class = adapter.class(classes[0]);
    assert_eq!(class.name, "Foo");
    assert!(
        class.annotations.iter().any(|a| a.matches("Getter")),
        "expected Foo to have @Getter; got {:?}",
        class.annotations
    );

    let field = class
        .fields
        .iter()
        .find(|f| f.name == "X")
        .expect("expected field X");
    assert!(field.is_static, "expected X to be static");
    assert!(field.is_final, "expected X to be final");
    assert!(
        field.has_annotation("Deprecated"),
        "expected X to have @Deprecated; got {:?}",
        field.annotations
    );
}

#[test]
fn dependency_detection_falls_back_to_pom_xml_scan() {
    let mut db = InMemoryFileStore::new();
    let pom_path = PathBuf::from("/workspace/pom.xml");
    let pom_file = db.file_id_for_path(&pom_path);
    db.set_file_text(
        pom_file,
        r#"<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter</artifactId>
    </dependency>
  </dependencies>
</project>
"#
        .to_string(),
    );

    let inner: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let adapter = FrameworkIdeDatabase::new(inner, ProjectId::new(0));
    let project = ProjectId::new(0);

    assert!(
        adapter.has_dependency(project, "org.springframework.boot", "spring-boot-starter"),
        "expected pom.xml fallback dependency detection to return true"
    );
}

#[test]
fn classpath_queries_return_false_without_project_config() {
    let mut db = InMemoryFileStore::new();
    let file_path = PathBuf::from("/virtual/src/main/java/Main.java");
    let file = db.file_id_for_path(&file_path);
    db.set_file_text(file, "class Main {}".to_string());

    let inner: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let adapter = FrameworkIdeDatabase::new(inner, ProjectId::new(0));
    let project = ProjectId::new(0);

    assert!(!adapter.has_class_on_classpath(project, "java.lang.String"));
    assert!(!adapter.has_class_on_classpath_prefix(project, "java.lang"));
}

#[test]
fn delegates_salsa_db_to_inner_database() {
    use std::path::Path;

    use nova_db::{Database as TextDatabase, FileId, SalsaDatabase};

    struct DbWithSalsa {
        inner: InMemoryFileStore,
        salsa: SalsaDatabase,
    }

    impl TextDatabase for DbWithSalsa {
        fn file_content(&self, file_id: FileId) -> &str {
            self.inner.file_content(file_id)
        }

        fn file_path(&self, file_id: FileId) -> Option<&Path> {
            self.inner.file_path(file_id)
        }

        fn all_file_ids(&self) -> Vec<FileId> {
            self.inner.all_file_ids()
        }

        fn file_id(&self, path: &Path) -> Option<FileId> {
            self.inner.file_id(path)
        }

        fn salsa_db(&self) -> Option<SalsaDatabase> {
            Some(self.salsa.clone())
        }
    }

    let mut inner = InMemoryFileStore::new();
    let file_path = PathBuf::from("/workspace/src/main/java/Main.java");
    let file = inner.file_id_for_path(&file_path);
    inner.set_file_text(file, "class Main {}".to_string());

    let db: Arc<dyn TextDatabase + Send + Sync> = Arc::new(DbWithSalsa {
        inner,
        salsa: SalsaDatabase::new(),
    });

    let adapter = FrameworkIdeDatabase::new(db, ProjectId::new(0));
    assert!(adapter.salsa_db().is_some());
}
