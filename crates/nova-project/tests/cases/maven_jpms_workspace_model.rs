use std::fs;

use nova_project::{load_workspace_model_with_options, BuildSystem, ClasspathEntryKind, LoadOptions};
use tempfile::tempdir;

fn write_fake_jar_dir_with_automatic_module_name(jar_path: &std::path::Path, module_name: &str) {
    let manifest_path = jar_path.join("META-INF").join("MANIFEST.MF");
    fs::create_dir_all(manifest_path.parent().expect("manifest parent"))
        .expect("mkdir jar/META-INF");

    // `nova_project::jpms::is_stable_named_module` reads `META-INF/MANIFEST.MF` using
    // `nova_archive::Archive`, which supports both `.jar` files and exploded directories.
    //
    // We create an exploded jar directory to avoid needing zip/jar tooling in tests.
    fs::write(&manifest_path, format!("Manifest-Version: 1.0\r\nAutomatic-Module-Name: {module_name}\r\n\r\n"))
        .expect("write manifest");
}

#[test]
fn maven_workspace_model_populates_module_path_for_jpms_projects() {
    let tmp = tempdir().expect("tempdir");
    let root = tmp.path();

    // Use an isolated Maven repo so this test is hermetic (does not depend on a pre-populated
    // `~/.m2/repository`).
    let maven_repo = root.join("m2");
    let guava_dir = maven_repo.join("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar");
    write_fake_jar_dir_with_automatic_module_name(&guava_dir, "com.google.common");

    fs::write(
        root.join("pom.xml"),
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

    let src_dir = root.join("src/main/java");
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
    let model = load_workspace_model_with_options(root, &options).expect("load workspace model");
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
    let model2 = load_workspace_model_with_options(root, &options).expect("reload workspace model");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}
