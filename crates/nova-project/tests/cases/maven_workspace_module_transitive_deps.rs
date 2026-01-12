use std::fs;
use std::path::Path;

use nova_project::{load_workspace_model_with_options, ClasspathEntryKind, LoadOptions};

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, contents).expect("write file");
}

#[test]
fn maven_workspace_model_includes_transitive_external_deps_of_workspace_module_deps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let maven_repo = root.join("m2");
    fs::create_dir_all(&maven_repo).expect("mkdir m2");
    // `nova-project` is intentionally offline (it does not invoke Maven), so dependency jars must
    // already exist in the configured local repository for them to appear on classpaths.
    //
    // Create a placeholder Guava jar so this test is deterministic and doesn't rely on the host
    // machine's `~/.m2/repository`.
    let guava_jar = maven_repo.join("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar");
    fs::create_dir_all(guava_jar.parent().expect("guava jar parent"))
        .expect("mkdir guava jar parent");
    fs::write(&guava_jar, b"").expect("write guava jar placeholder");

    // Root aggregator with two workspace modules.
    write_file(
        &root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>root</artifactId>
  <version>1.0.0</version>
  <packaging>pom</packaging>

  <modules>
    <module>lib</module>
    <module>app</module>
  </modules>
</project>
"#,
    );

    // `lib` exposes Guava types in its API; `app` depends only on `lib` (no direct Guava dep).
    write_file(
        &root.join("lib/pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent>
    <groupId>com.example</groupId>
    <artifactId>root</artifactId>
    <version>1.0.0</version>
  </parent>
  <artifactId>lib</artifactId>

  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>33.0.0-jre</version>
    </dependency>
  </dependencies>
</project>
"#,
    );

    write_file(
        &root.join("app/pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent>
    <groupId>com.example</groupId>
    <artifactId>root</artifactId>
    <version>1.0.0</version>
  </parent>
  <artifactId>app</artifactId>

  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>lib</artifactId>
      <version>${project.version}</version>
    </dependency>
  </dependencies>
 </project>
"#,
    );

    let options = LoadOptions {
        maven_repo: Some(maven_repo),
        ..LoadOptions::default()
    };
    let model = load_workspace_model_with_options(root, &options).expect("load workspace model");

    let app_module = model
        .modules
        .iter()
        .find(|m| m.name == "app")
        .expect("app module");

    let has_guava_jar = app_module
        .module_path
        .iter()
        .chain(app_module.classpath.iter())
        .filter(|e| e.kind == ClasspathEntryKind::Jar)
        .any(|e| {
            e.path
                .to_string_lossy()
                .replace('\\', "/")
                .contains("com/google/guava/guava/33.0.0-jre")
        });

    assert!(
        has_guava_jar,
        "expected app module classpath/module-path to include Guava jar from transitive workspace module dependency"
    );

    // Ensure deterministic output (no dependence on host ~/.m2).
    let model2 = load_workspace_model_with_options(root, &options).expect("reload workspace model");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}
