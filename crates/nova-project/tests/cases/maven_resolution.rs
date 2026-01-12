use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use nova_project::{load_project_with_options, ClasspathEntryKind, LoadOptions};

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, contents).expect("write file");
}

fn repo_pom_path(repo: &Path, group_id: &str, artifact_id: &str, version: &str) -> PathBuf {
    let group_path = group_id.replace('.', "/");
    repo.join(group_path)
        .join(artifact_id)
        .join(version)
        .join(format!("{artifact_id}-{version}.pom"))
}

fn repo_jar_path(repo: &Path, group_id: &str, artifact_id: &str, version: &str) -> PathBuf {
    let group_path = group_id.replace('.', "/");
    repo.join(group_path)
        .join(artifact_id)
        .join(version)
        .join(format!("{artifact_id}-{version}.jar"))
}

#[test]
fn resolves_parent_bom_profiles_and_transitive_deps_offline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace_root = temp.path().join("workspace");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("create workspace dir");
    fs::create_dir_all(&repo).expect("create repo dir");

    // Workspace POM depends on:
    // - parent POM (in repo): provides managed versions via properties + dependencyManagement
    // - BOM import (in repo): overrides parent-managed dep versions
    // - an activeByDefault profile: contributes additional dependencies
    // - dep-a -> dep-b: transitive dependency resolution from repo
    write_file(
        &workspace_root.join("pom.xml"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>

  <parent>
    <groupId>com.test</groupId>
    <artifactId>parent</artifactId>
    <version>1.0.0</version>
  </parent>

  <artifactId>app</artifactId>

  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>com.test</groupId>
        <artifactId>bom</artifactId>
        <version>2.0.0</version>
        <type>pom</type>
        <scope>import</scope>
      </dependency>
    </dependencies>
  </dependencyManagement>

  <dependencies>
    <dependency>
      <groupId>com.dep</groupId>
      <artifactId>dep-parent</artifactId>
    </dependency>
    <dependency>
      <groupId>com.dep</groupId>
      <artifactId>dep-a</artifactId>
    </dependency>
  </dependencies>

  <profiles>
    <profile>
      <id>default</id>
      <activation>
        <activeByDefault>true</activeByDefault>
      </activation>
      <dependencies>
        <dependency>
          <groupId>com.dep</groupId>
          <artifactId>dep-profile</artifactId>
          <version>3.0.0</version>
        </dependency>
      </dependencies>
    </profile>
  </profiles>
</project>
"#,
    );

    // Parent POM in repo provides properties + dependencyManagement.
    write_file(
        &repo_pom_path(&repo, "com.test", "parent", "1.0.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.test</groupId>
  <artifactId>parent</artifactId>
  <version>1.0.0</version>
  <packaging>pom</packaging>

  <properties>
    <dep.parent.version>9.9.9</dep.parent.version>
  </properties>

  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>com.dep</groupId>
        <artifactId>dep-parent</artifactId>
        <version>${dep.parent.version}</version>
      </dependency>
      <dependency>
        <groupId>com.dep</groupId>
        <artifactId>dep-a</artifactId>
        <version>0.9.0</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
</project>
"#,
    );

    // BOM in repo overrides parent-managed dep-a version.
    write_file(
        &repo_pom_path(&repo, "com.test", "bom", "2.0.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.test</groupId>
  <artifactId>bom</artifactId>
  <version>2.0.0</version>
  <packaging>pom</packaging>

  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>com.dep</groupId>
        <artifactId>dep-a</artifactId>
        <version>1.0.0</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
</project>
"#,
    );

    // dep-a has a transitive dependency on dep-b.
    write_file(
        &repo_pom_path(&repo, "com.dep", "dep-a", "1.0.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
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

    write_file(
        &repo_pom_path(&repo, "com.dep", "dep-b", "2.0.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-b</artifactId>
  <version>2.0.0</version>
</project>
"#,
    );

    // Create placeholder jars so Maven dependency jar discovery can find them.
    for (group_id, artifact_id, version) in [
        ("com.dep", "dep-a", "1.0.0"),
        ("com.dep", "dep-b", "2.0.0"),
        ("com.dep", "dep-parent", "9.9.9"),
        ("com.dep", "dep-profile", "3.0.0"),
    ] {
        write_file(&repo_jar_path(&repo, group_id, artifact_id, version), "");
    }
    // Leaf dependencies.
    write_file(
        &repo_pom_path(&repo, "com.dep", "dep-parent", "9.9.9"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-parent</artifactId>
  <version>9.9.9</version>
</project>
"#,
    );
    write_file(&repo_jar_path(&repo, "com.dep", "dep-parent", "9.9.9"), "");
    write_file(
        &repo_pom_path(&repo, "com.dep", "dep-profile", "3.0.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.dep</groupId>
  <artifactId>dep-profile</artifactId>
  <version>3.0.0</version>
</project>
"#,
    );
    write_file(&repo_jar_path(&repo, "com.dep", "dep-profile", "3.0.0"), "");

    // Fake jars for resolved dependencies so classpath resolution includes them.
    for (group, artifact, version) in [
        ("com.dep", "dep-a", "1.0.0"),
        ("com.dep", "dep-b", "2.0.0"),
    ] {
        let jar_path = repo_jar_path(&repo, group, artifact, version);
        std::fs::create_dir_all(jar_path.parent().expect("jar parent"))
            .expect("create jar parent");
        std::fs::write(&jar_path, b"not really a jar").expect("write jar placeholder");
    }

    let options = LoadOptions {
        maven_repo: Some(repo.clone()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&workspace_root, &options).expect("load project");

    // Parent dependencyManagement + properties should provide managed version.
    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
        .collect();
    assert!(deps.contains(&(
        "com.dep".to_string(),
        "dep-parent".to_string(),
        Some("9.9.9".to_string())
    )));

    // BOM import should override parent-managed version.
    assert!(deps.contains(&(
        "com.dep".to_string(),
        "dep-a".to_string(),
        Some("1.0.0".to_string())
    )));

    // Active-by-default profile deps should be included.
    assert!(deps.contains(&(
        "com.dep".to_string(),
        "dep-profile".to_string(),
        Some("3.0.0".to_string())
    )));

    // Transitive dependency resolution from repo.
    assert!(deps.contains(&(
        "com.dep".to_string(),
        "dep-b".to_string(),
        Some("2.0.0".to_string())
    )));

    // Classpath should include jar entries for transitive deps when present on disk.
    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    assert!(jar_entries
        .iter()
        .any(|p| p.contains("com/dep/dep-a/1.0.0/dep-a-1.0.0.jar")));
    assert!(jar_entries
        .iter()
        .any(|p| p.contains("com/dep/dep-b/2.0.0/dep-b-2.0.0.jar")));

    // Ensure deterministic output (no dependence on host ~/.m2).
    let config2 = load_project_with_options(&workspace_root, &options).expect("reload project");
    assert_eq!(config, config2);
}
#[test]
fn resolves_optional_dependencies_as_non_transitive() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace_root = temp.path().join("workspace");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("create workspace dir");
    fs::create_dir_all(&repo).expect("create repo dir");

    // dep-a -> dep-b (optional); workspace depends on dep-a.
    write_file(
        &workspace_root.join("pom.xml"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>workspace</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-a</artifactId>
      <version>1.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );

    write_file(
        &repo_pom_path(&repo, "com.example", "dep-a", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-a</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-b</artifactId>
      <version>1.0</version>
      <optional>true</optional>
    </dependency>
  </dependencies>
</project>
"#,
    );
    write_file(
        &repo_pom_path(&repo, "com.example", "dep-b", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-b</artifactId>
  <version>1.0</version>
</project>
"#,
    );

    let options = LoadOptions {
        maven_repo: Some(repo.clone()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&workspace_root, &options).expect("load project");

    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone()))
        .collect();
    assert!(deps.contains(&("com.example".to_string(), "dep-a".to_string())));
    assert!(!deps.contains(&("com.example".to_string(), "dep-b".to_string())));
}

#[test]
fn resolves_dependency_exclusions_transitively() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace_root = temp.path().join("workspace");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("create workspace dir");
    fs::create_dir_all(&repo).expect("create repo dir");

    // dep-a -> dep-b (non-optional); workspace depends on dep-a but excludes dep-b.
    write_file(
        &workspace_root.join("pom.xml"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>workspace</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-a</artifactId>
      <version>1.0</version>
      <exclusions>
        <exclusion>
          <groupId>com.example</groupId>
          <artifactId>dep-b</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>
"#,
    );

    write_file(
        &repo_pom_path(&repo, "com.example", "dep-a", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-a</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-b</artifactId>
      <version>1.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );
    write_file(
        &repo_pom_path(&repo, "com.example", "dep-b", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-b</artifactId>
  <version>1.0</version>
</project>
"#,
    );

    let options = LoadOptions {
        maven_repo: Some(repo.clone()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&workspace_root, &options).expect("load project");

    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone()))
        .collect();
    assert!(deps.contains(&("com.example".to_string(), "dep-a".to_string())));
    assert!(!deps.contains(&("com.example".to_string(), "dep-b".to_string())));
}

#[test]
fn resolves_dependency_management_exclusions() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace_root = temp.path().join("workspace");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("create workspace dir");
    fs::create_dir_all(&repo).expect("create repo dir");

    // dependencyManagement provides exclusions for dep-a; workspace depends on dep-a without
    // specifying exclusions inline.
    write_file(
        &workspace_root.join("pom.xml"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>workspace</artifactId>
  <version>1.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>com.example</groupId>
        <artifactId>dep-a</artifactId>
        <version>1.0</version>
        <exclusions>
          <exclusion>
            <groupId>com.example</groupId>
            <artifactId>dep-b</artifactId>
          </exclusion>
        </exclusions>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-a</artifactId>
      <version>1.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );

    write_file(
        &repo_pom_path(&repo, "com.example", "dep-a", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-a</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-b</artifactId>
      <version>1.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );
    write_file(
        &repo_pom_path(&repo, "com.example", "dep-b", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-b</artifactId>
  <version>1.0</version>
</project>
"#,
    );

    let options = LoadOptions {
        maven_repo: Some(repo.clone()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&workspace_root, &options).expect("load project");

    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone()))
        .collect();
    assert!(deps.contains(&("com.example".to_string(), "dep-a".to_string())));
    assert!(!deps.contains(&("com.example".to_string(), "dep-b".to_string())));
}

#[test]
fn resolves_dependency_management_optional_override() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace_root = temp.path().join("workspace");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("create workspace dir");
    fs::create_dir_all(&repo).expect("create repo dir");

    // dep-a manages dep-b as optional, but explicitly declares dep-b as non-optional.
    write_file(
        &workspace_root.join("pom.xml"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>workspace</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-a</artifactId>
      <version>1.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );

    write_file(
        &repo_pom_path(&repo, "com.example", "dep-a", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-a</artifactId>
  <version>1.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>com.example</groupId>
        <artifactId>dep-b</artifactId>
        <version>1.0</version>
        <optional>true</optional>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-b</artifactId>
      <version>1.0</version>
      <optional>false</optional>
    </dependency>
  </dependencies>
</project>
"#,
    );

    write_file(
        &repo_pom_path(&repo, "com.example", "dep-b", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-b</artifactId>
  <version>1.0</version>
</project>
"#,
    );

    let options = LoadOptions {
        maven_repo: Some(repo.clone()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&workspace_root, &options).expect("load project");

    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone()))
        .collect();
    assert!(deps.contains(&("com.example".to_string(), "dep-a".to_string())));
    assert!(deps.contains(&("com.example".to_string(), "dep-b".to_string())));
}

#[test]
fn resolves_wildcard_exclusions_across_paths() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace_root = temp.path().join("workspace");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&workspace_root).expect("create workspace dir");
    fs::create_dir_all(&repo).expect("create repo dir");

    // Two different paths to dep-b exclude dep-d, but using different exclusion patterns.
    write_file(
        &workspace_root.join("pom.xml"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>workspace</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-a</artifactId>
      <version>1.0</version>
    </dependency>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-c</artifactId>
      <version>1.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );

    // dep-a brings dep-b but excludes all of dep-b's transitive deps.
    write_file(
        &repo_pom_path(&repo, "com.example", "dep-a", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-a</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-b</artifactId>
      <version>1.0</version>
      <exclusions>
        <exclusion>
          <groupId>*</groupId>
          <artifactId>*</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>
"#,
    );

    // dep-c brings dep-b but explicitly excludes dep-d.
    write_file(
        &repo_pom_path(&repo, "com.example", "dep-c", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-c</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-b</artifactId>
      <version>1.0</version>
      <exclusions>
        <exclusion>
          <groupId>com.example</groupId>
          <artifactId>dep-d</artifactId>
        </exclusion>
      </exclusions>
    </dependency>
  </dependencies>
</project>
"#,
    );

    write_file(
        &repo_pom_path(&repo, "com.example", "dep-b", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-b</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>dep-d</artifactId>
      <version>1.0</version>
    </dependency>
  </dependencies>
</project>
"#,
    );

    write_file(
        &repo_pom_path(&repo, "com.example", "dep-d", "1.0"),
        r#"
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>dep-d</artifactId>
  <version>1.0</version>
</project>
"#,
    );

    let options = LoadOptions {
        maven_repo: Some(repo.clone()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&workspace_root, &options).expect("load project");

    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone()))
        .collect();
    assert!(deps.contains(&("com.example".to_string(), "dep-a".to_string())));
    assert!(deps.contains(&("com.example".to_string(), "dep-c".to_string())));
    assert!(deps.contains(&("com.example".to_string(), "dep-b".to_string())));
    assert!(!deps.contains(&("com.example".to_string(), "dep-d".to_string())));
}
