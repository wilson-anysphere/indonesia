use std::fs;
use std::path::Path;

use nova_project::{load_workspace_model_with_options, ClasspathEntryKind, LoadOptions};

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, contents).expect("write file");
}

fn repo_pom_path(
    repo: &Path,
    group_id: &str,
    artifact_id: &str,
    version: &str,
) -> std::path::PathBuf {
    let group_path = group_id.replace('.', "/");
    repo.join(group_path)
        .join(artifact_id)
        .join(version)
        .join(format!("{artifact_id}-{version}.pom"))
}

fn repo_jar_path(
    repo: &Path,
    group_id: &str,
    artifact_id: &str,
    version: &str,
) -> std::path::PathBuf {
    let group_path = group_id.replace('.', "/");
    repo.join(group_path)
        .join(artifact_id)
        .join(version)
        .join(format!("{artifact_id}-{version}.jar"))
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

#[test]
fn maven_workspace_model_does_not_propagate_test_deps_of_workspace_module_deps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let maven_repo = root.join("m2");
    fs::create_dir_all(&maven_repo).expect("mkdir m2");

    // Create placeholder jars so this test is deterministic and doesn't rely on the host
    // machine's `~/.m2/repository`.
    let guava_jar = repo_jar_path(&maven_repo, "com.google.guava", "guava", "33.0.0-jre");
    fs::create_dir_all(guava_jar.parent().expect("guava jar parent"))
        .expect("mkdir guava jar parent");
    fs::write(&guava_jar, b"").expect("write guava jar placeholder");

    let junit_jar = repo_jar_path(
        &maven_repo,
        "org.junit.jupiter",
        "junit-jupiter-api",
        "5.10.0",
    );
    fs::create_dir_all(junit_jar.parent().expect("junit jar parent"))
        .expect("mkdir junit jar parent");
    fs::write(&junit_jar, b"").expect("write junit jar placeholder");

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

    // `lib` has a compile dependency (Guava) and a test dependency (JUnit).
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
    <dependency>
      <groupId>org.junit.jupiter</groupId>
      <artifactId>junit-jupiter-api</artifactId>
      <version>5.10.0</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
</project>
"#,
    );

    // `app` depends on `lib` only (no direct Guava/JUnit deps).
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

    let jar_entries: Vec<String> = app_module
        .module_path
        .iter()
        .chain(app_module.classpath.iter())
        .filter(|e| e.kind == ClasspathEntryKind::Jar)
        .map(|e| e.path.to_string_lossy().replace('\\', "/"))
        .collect();

    assert!(
        jar_entries
            .iter()
            .any(|p| p.contains("com/google/guava/guava/33.0.0-jre/guava-33.0.0-jre.jar")),
        "expected Guava jar (lib compile dep) to be present on app classpath/module-path; got: {jar_entries:?}"
    );
    assert!(
        !jar_entries.iter().any(|p| p
            .contains("org/junit/jupiter/junit-jupiter-api/5.10.0/junit-jupiter-api-5.10.0.jar")),
        "expected JUnit jar (lib test dep) to NOT be present on app classpath/module-path; got: {jar_entries:?}"
    );

    // Determinism.
    let model2 = load_workspace_model_with_options(root, &options).expect("reload workspace model");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}

#[test]
fn maven_workspace_model_includes_transitive_external_closure_of_workspace_module_deps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let maven_repo = root.join("m2");
    fs::create_dir_all(&maven_repo).expect("mkdir m2");

    // External dep graph:
    // dep-a:1.0.0 -> dep-b:2.0.0
    let dep_b_pom = repo_pom_path(&maven_repo, "com.dep", "dep-b", "2.0.0");
    write_file(
        &dep_b_pom,
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-b</artifactId>
  <version>2.0.0</version>
</project>
"#,
    );
    let dep_b_jar = repo_jar_path(&maven_repo, "com.dep", "dep-b", "2.0.0");
    fs::create_dir_all(dep_b_jar.parent().expect("dep-b jar parent")).expect("mkdir dep-b parent");
    fs::write(&dep_b_jar, b"").expect("write dep-b jar placeholder");

    let dep_a_pom = repo_pom_path(&maven_repo, "com.dep", "dep-a", "1.0.0");
    write_file(
        &dep_a_pom,
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-a</artifactId>
  <version>1.0.0</version>

  <dependencies>
    <dependency>
      <groupId>com.dep</groupId>
      <artifactId>dep-b</artifactId>
      <version>2.0.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );
    let dep_a_jar = repo_jar_path(&maven_repo, "com.dep", "dep-a", "1.0.0");
    fs::create_dir_all(dep_a_jar.parent().expect("dep-a jar parent")).expect("mkdir dep-a parent");
    fs::write(&dep_a_jar, b"").expect("write dep-a jar placeholder");

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

    // `lib` depends on dep-a (which depends on dep-b); `app` depends only on `lib`.
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
      <groupId>com.dep</groupId>
      <artifactId>dep-a</artifactId>
      <version>1.0.0</version>
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

    let jar_entries: Vec<String> = app_module
        .module_path
        .iter()
        .chain(app_module.classpath.iter())
        .filter(|e| e.kind == ClasspathEntryKind::Jar)
        .map(|e| e.path.to_string_lossy().replace('\\', "/"))
        .collect();

    assert!(
        jar_entries
            .iter()
            .any(|p| p.contains("com/dep/dep-a/1.0.0/dep-a-1.0.0.jar")),
        "expected dep-a jar to be present on app classpath/module-path; got: {jar_entries:?}"
    );
    assert!(
        jar_entries
            .iter()
            .any(|p| p.contains("com/dep/dep-b/2.0.0/dep-b-2.0.0.jar")),
        "expected dep-b jar (transitive via dep-a) to be present on app classpath/module-path; got: {jar_entries:?}"
    );

    // Determinism.
    let model2 = load_workspace_model_with_options(root, &options).expect("reload workspace model");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}

#[test]
fn maven_workspace_model_respects_exclusions_on_workspace_module_deps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let maven_repo = root.join("m2");
    fs::create_dir_all(&maven_repo).expect("mkdir m2");

    // External dep graph:
    // dep-a:1.0.0 -> dep-b:2.0.0
    let dep_b_pom = repo_pom_path(&maven_repo, "com.dep", "dep-b", "2.0.0");
    write_file(
        &dep_b_pom,
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-b</artifactId>
  <version>2.0.0</version>
</project>
"#,
    );
    let dep_b_jar = repo_jar_path(&maven_repo, "com.dep", "dep-b", "2.0.0");
    fs::create_dir_all(dep_b_jar.parent().expect("dep-b jar parent")).expect("mkdir dep-b parent");
    fs::write(&dep_b_jar, b"").expect("write dep-b jar placeholder");

    let dep_a_pom = repo_pom_path(&maven_repo, "com.dep", "dep-a", "1.0.0");
    write_file(
        &dep_a_pom,
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-a</artifactId>
  <version>1.0.0</version>

  <dependencies>
    <dependency>
      <groupId>com.dep</groupId>
      <artifactId>dep-b</artifactId>
      <version>2.0.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );
    let dep_a_jar = repo_jar_path(&maven_repo, "com.dep", "dep-a", "1.0.0");
    fs::create_dir_all(dep_a_jar.parent().expect("dep-a jar parent")).expect("mkdir dep-a parent");
    fs::write(&dep_a_jar, b"").expect("write dep-a jar placeholder");

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

    // `lib` depends on dep-a (which depends on dep-b); `app` depends only on `lib`, but excludes
    // dep-b at the dependency edge.
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
      <groupId>com.dep</groupId>
      <artifactId>dep-a</artifactId>
      <version>1.0.0</version>
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
      <exclusions>
        <exclusion>
          <groupId>com.dep</groupId>
          <artifactId>dep-b</artifactId>
        </exclusion>
      </exclusions>
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

    let jar_entries: Vec<String> = app_module
        .module_path
        .iter()
        .chain(app_module.classpath.iter())
        .filter(|e| e.kind == ClasspathEntryKind::Jar)
        .map(|e| e.path.to_string_lossy().replace('\\', "/"))
        .collect();

    assert!(
        jar_entries
            .iter()
            .any(|p| p.contains("com/dep/dep-a/1.0.0/dep-a-1.0.0.jar")),
        "expected dep-a jar to be present on app classpath/module-path; got: {jar_entries:?}"
    );
    assert!(
        !jar_entries
            .iter()
            .any(|p| p.contains("com/dep/dep-b/2.0.0/dep-b-2.0.0.jar")),
        "expected dep-b jar to be excluded from app classpath/module-path; got: {jar_entries:?}"
    );

    // Determinism.
    let model2 = load_workspace_model_with_options(root, &options).expect("reload workspace model");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}

#[test]
fn maven_workspace_model_does_not_over_exclude_workspace_module_deps_across_multiple_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let maven_repo = root.join("m2");
    fs::create_dir_all(&maven_repo).expect("mkdir m2");

    // External dep graph:
    // dep-a:1.0.0 -> dep-b:2.0.0
    let dep_b_pom = repo_pom_path(&maven_repo, "com.dep", "dep-b", "2.0.0");
    write_file(
        &dep_b_pom,
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-b</artifactId>
  <version>2.0.0</version>
</project>
"#,
    );
    let dep_b_jar = repo_jar_path(&maven_repo, "com.dep", "dep-b", "2.0.0");
    fs::create_dir_all(dep_b_jar.parent().expect("dep-b jar parent")).expect("mkdir dep-b parent");
    fs::write(&dep_b_jar, b"").expect("write dep-b jar placeholder");

    let dep_a_pom = repo_pom_path(&maven_repo, "com.dep", "dep-a", "1.0.0");
    write_file(
        &dep_a_pom,
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-a</artifactId>
  <version>1.0.0</version>

  <dependencies>
    <dependency>
      <groupId>com.dep</groupId>
      <artifactId>dep-b</artifactId>
      <version>2.0.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );
    let dep_a_jar = repo_jar_path(&maven_repo, "com.dep", "dep-a", "1.0.0");
    fs::create_dir_all(dep_a_jar.parent().expect("dep-a jar parent")).expect("mkdir dep-a parent");
    fs::write(&dep_a_jar, b"").expect("write dep-a jar placeholder");

    // Root aggregator with four workspace modules.
    write_file(
        &root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>root</artifactId>
  <version>1.0.0</version>
  <packaging>pom</packaging>

  <modules>
    <module>util</module>
    <module>lib1</module>
    <module>lib2</module>
    <module>app</module>
  </modules>
</project>
"#,
    );

    // `util` depends on dep-a (which depends on dep-b).
    write_file(
        &root.join("util/pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent>
    <groupId>com.example</groupId>
    <artifactId>root</artifactId>
    <version>1.0.0</version>
  </parent>
  <artifactId>util</artifactId>

  <dependencies>
    <dependency>
      <groupId>com.dep</groupId>
      <artifactId>dep-a</artifactId>
      <version>1.0.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );

    // Both libs depend on the workspace `util` module.
    for lib in ["lib1", "lib2"] {
        write_file(
            &root.join(lib).join("pom.xml"),
            &format!(
                r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent>
    <groupId>com.example</groupId>
    <artifactId>root</artifactId>
    <version>1.0.0</version>
  </parent>
  <artifactId>{lib}</artifactId>

  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>util</artifactId>
      <version>${{project.version}}</version>
    </dependency>
  </dependencies>
</project>
"#
            ),
        );
    }

    // `app` depends on both libs, but excludes dep-b only via the `lib1` edge.
    // dep-b should still be present because it is reachable through `lib2` without exclusion.
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
      <artifactId>lib1</artifactId>
      <version>${project.version}</version>
      <exclusions>
        <exclusion>
          <groupId>com.dep</groupId>
          <artifactId>dep-b</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>lib2</artifactId>
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

    let jar_entries: Vec<String> = app_module
        .module_path
        .iter()
        .chain(app_module.classpath.iter())
        .filter(|e| e.kind == ClasspathEntryKind::Jar)
        .map(|e| e.path.to_string_lossy().replace('\\', "/"))
        .collect();

    assert!(
        jar_entries
            .iter()
            .any(|p| p.contains("com/dep/dep-a/1.0.0/dep-a-1.0.0.jar")),
        "expected dep-a jar to be present on app classpath/module-path; got: {jar_entries:?}"
    );
    assert!(
        jar_entries
            .iter()
            .any(|p| p.contains("com/dep/dep-b/2.0.0/dep-b-2.0.0.jar")),
        "expected dep-b jar to be present on app classpath/module-path (it is not excluded on all paths); got: {jar_entries:?}"
    );

    // Determinism.
    let model2 = load_workspace_model_with_options(root, &options).expect("reload workspace model");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}
