use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use tempfile::tempdir;
use zip::write::FileOptions;

use nova_ide::framework_cache::{project_config, project_root_for_path, spring_metadata_index};

#[test]
fn discovers_project_root_for_maven_layout() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("pom.xml"), "<project></project>").unwrap();

    let java_file = dir.path().join("src/main/java/com/example/App.java");
    std::fs::create_dir_all(java_file.parent().unwrap()).unwrap();
    std::fs::write(&java_file, "class App {}").unwrap();

    assert_eq!(project_root_for_path(&java_file), dir.path());
}

#[test]
fn discovers_project_root_for_gradle_layout() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.gradle"),
        "rootProject.name = 'demo'",
    )
    .unwrap();

    let java_file = dir.path().join("src/main/java/com/example/App.java");
    std::fs::create_dir_all(java_file.parent().unwrap()).unwrap();
    std::fs::write(&java_file, "class App {}").unwrap();

    assert_eq!(project_root_for_path(&java_file), dir.path());
}

#[test]
fn caches_project_config() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
</project>"#,
    )
    .unwrap();

    let first = project_config(dir.path()).unwrap();
    let second = project_config(dir.path()).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn spring_metadata_cache_ingests_metadata_from_dependency_jar() {
    let dir = tempdir().unwrap();

    // Set up a fake dependency jar in the default Maven local repo location.
    let maven_repo = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(".m2/repository"))
        .unwrap_or_else(|| PathBuf::from(".m2/repository"));

    let jar_path = maven_repo
        .join("com/example")
        .join("test-metadata")
        .join("1.0.0")
        .join("test-metadata-1.0.0.jar");
    std::fs::create_dir_all(jar_path.parent().unwrap()).unwrap();

    let mut jar = zip::ZipWriter::new(std::fs::File::create(&jar_path).unwrap());
    jar.start_file(
        "META-INF/spring-configuration-metadata.json",
        FileOptions::<()>::default(),
    )
    .unwrap();
    write!(
        jar,
        r#"{{
  "properties": [ {{
    "name": "server.port",
    "type": "java.lang.Integer",
    "defaultValue": 8080
  }} ]
}}"#
    )
    .unwrap();
    jar.finish().unwrap();

    // Maven project that depends on the fake jar.
    std::fs::write(
        dir.path().join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>test-metadata</artifactId>
      <version>1.0.0</version>
    </dependency>
  </dependencies>
</project>"#,
    )
    .unwrap();

    let first = spring_metadata_index(dir.path());
    assert!(
        !first.is_empty(),
        "expected spring metadata index to ingest metadata from {}",
        jar_path.display()
    );
    assert!(first.property_meta("server.port").is_some());

    let second = spring_metadata_index(dir.path());
    assert!(Arc::ptr_eq(&first, &second));
}
