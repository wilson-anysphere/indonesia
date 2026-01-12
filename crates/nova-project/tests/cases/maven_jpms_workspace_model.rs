use std::fs;
use std::io::Write;
use std::path::Path;

use nova_project::{
    load_workspace_model_with_options, BuildSystem, ClasspathEntryKind, LoadOptions,
};
use tempfile::tempdir;
use zip::write::FileOptions;

fn write_fake_jar_with_automatic_module_name(jar_path: &Path, module_name: &str) {
    if let Some(parent) = jar_path.parent() {
        fs::create_dir_all(parent).expect("mkdir jar parent");
    }

    let manifest = format!("Manifest-Version: 1.0\r\nAutomatic-Module-Name: {module_name}\r\n\r\n");
    let mut jar = zip::ZipWriter::new(std::fs::File::create(jar_path).expect("create jar"));
    let options = FileOptions::<()>::default();
    jar.start_file("META-INF/MANIFEST.MF", options)
        .expect("start manifest entry");
    jar.write_all(manifest.as_bytes())
        .expect("write manifest contents");
    jar.finish().expect("finish jar");
}

fn write_exploded_jar_with_automatic_module_name(jar_dir: &Path, module_name: &str) {
    fs::create_dir_all(jar_dir.join("META-INF")).expect("mkdir exploded jar META-INF");
    let manifest = format!("Manifest-Version: 1.0\r\nAutomatic-Module-Name: {module_name}\r\n\r\n");
    fs::write(jar_dir.join("META-INF/MANIFEST.MF"), manifest).expect("write manifest");
}

#[test]
fn maven_workspace_model_populates_module_path_for_jpms_projects() {
    let tmp = tempdir().expect("tempdir");
    let workspace_root = tmp.path().join("workspace");
    let maven_repo = tmp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("mkdir workspace");
    fs::create_dir_all(&maven_repo).expect("mkdir repo");

    let guava_jar = maven_repo.join("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar");
    write_fake_jar_with_automatic_module_name(&guava_jar, "com.google.common");

    fs::write(
        workspace_root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0-SNAPSHOT</version>

  <properties>
    <maven.compiler.source>17</maven.compiler.source>
    <maven.compiler.target>17</maven.compiler.target>
  </properties>

  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>33.0.0-jre</version>
    </dependency>
  </dependencies>
 </project>
"#,
    )
    .expect("write pom.xml");

    let src_dir = workspace_root.join("src/main/java");
    fs::create_dir_all(&src_dir).expect("mkdir src/main/java");
    fs::write(
        src_dir.join("module-info.java"),
        "module com.example.app { requires com.google.common; }",
    )
    .expect("write module-info.java");

    let options = LoadOptions {
        maven_repo: Some(maven_repo),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&workspace_root, &options).expect("load workspace model");
    assert_eq!(model.build_system, BuildSystem::Maven);
    assert_eq!(model.modules.len(), 1);

    let module = &model.modules[0];

    let has_guava_on_module_path = module
        .module_path
        .iter()
        .filter(|e| e.kind == ClasspathEntryKind::Jar)
        .any(|e| {
            e.path
                .to_string_lossy()
                .replace('\\', "/")
                .contains("com/google/guava/guava/33.0.0-jre")
        });
    assert!(
        has_guava_on_module_path,
        "expected Guava jar entry to be on module-path for JPMS workspaces"
    );

    let has_guava_on_classpath = module
        .classpath
        .iter()
        .filter(|e| e.kind == ClasspathEntryKind::Jar)
        .any(|e| {
            e.path
                .to_string_lossy()
                .replace('\\', "/")
                .contains("com/google/guava/guava/33.0.0-jre")
        });
    assert!(
        !has_guava_on_classpath,
        "expected Guava jar entry to be removed from classpath for JPMS workspaces"
    );

    // Output directories should remain on the classpath.
    assert!(
        module.classpath.iter().any(|e| {
            e.kind == ClasspathEntryKind::Directory && e.path.ends_with("target/classes")
        }),
        "expected target/classes to remain on module classpath"
    );
    assert!(
        module.classpath.iter().any(|e| {
            e.kind == ClasspathEntryKind::Directory && e.path.ends_with("target/test-classes")
        }),
        "expected target/test-classes to remain on module classpath"
    );

    // Ensure model is deterministic (important for cache keys and downstream indexing).
    let model2 = load_workspace_model_with_options(&workspace_root, &options)
        .expect("reload workspace model");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}

#[test]
fn maven_workspace_model_keeps_dependency_jars_on_classpath_when_jpms_disabled() {
    let tmp = tempdir().expect("tempdir");
    let workspace_root = tmp.path().join("workspace");
    let maven_repo = tmp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("mkdir workspace");
    fs::create_dir_all(&maven_repo).expect("mkdir repo");

    let guava_jar = maven_repo.join("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar");
    fs::create_dir_all(guava_jar.parent().expect("jar parent")).expect("mkdir jar parent");
    fs::write(&guava_jar, b"").expect("write jar placeholder");

    fs::write(
        workspace_root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0-SNAPSHOT</version>

  <properties>
    <maven.compiler.source>17</maven.compiler.source>
    <maven.compiler.target>17</maven.compiler.target>
  </properties>

  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>33.0.0-jre</version>
    </dependency>
  </dependencies>
 </project>
"#,
    )
    .expect("write pom.xml");

    let src_dir = workspace_root.join("src/main/java/com/example");
    fs::create_dir_all(&src_dir).expect("mkdir src/main/java");
    fs::write(
        src_dir.join("Main.java"),
        "package com.example; class Main {}",
    )
    .expect("write Main.java");

    let options = LoadOptions {
        maven_repo: Some(maven_repo),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&workspace_root, &options).expect("load workspace model");
    assert_eq!(model.build_system, BuildSystem::Maven);
    assert_eq!(model.modules.len(), 1);

    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .all(|e| e.kind != ClasspathEntryKind::Jar),
        "expected no jars on module-path when JPMS is disabled"
    );
    assert!(
        module
            .classpath
            .iter()
            .any(|e| e.kind == ClasspathEntryKind::Jar && e.path == guava_jar),
        "expected Guava jar entry to remain on classpath when JPMS is disabled"
    );
}

#[test]
fn maven_workspace_model_accepts_exploded_dependency_jars() {
    let tmp = tempdir().expect("tempdir");
    let workspace_root = tmp.path().join("workspace");
    let maven_repo = tmp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("mkdir workspace");
    fs::create_dir_all(&maven_repo).expect("mkdir repo");

    let guava_jar_dir = maven_repo.join("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar");
    write_exploded_jar_with_automatic_module_name(&guava_jar_dir, "com.google.common");

    fs::write(
        workspace_root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0-SNAPSHOT</version>

  <properties>
    <maven.compiler.source>17</maven.compiler.source>
    <maven.compiler.target>17</maven.compiler.target>
  </properties>

  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>33.0.0-jre</version>
    </dependency>
  </dependencies>
 </project>
"#,
    )
    .expect("write pom.xml");

    let src_dir = workspace_root.join("src/main/java");
    fs::create_dir_all(&src_dir).expect("mkdir src/main/java");
    fs::write(
        src_dir.join("module-info.java"),
        "module com.example.app { requires com.google.common; }",
    )
    .expect("write module-info.java");

    let options = LoadOptions {
        maven_repo: Some(maven_repo),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&workspace_root, &options).expect("load workspace model");
    assert_eq!(model.build_system, BuildSystem::Maven);
    assert_eq!(model.modules.len(), 1);

    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Directory && entry.path == guava_jar_dir),
        "expected exploded jar to be placed on module-path"
    );
    assert!(
        !module
            .classpath
            .iter()
            .any(|entry| entry.path == guava_jar_dir),
        "expected exploded jar to be removed from classpath"
    );
}
