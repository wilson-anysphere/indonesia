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
    write_file(&repo_jar_path(&repo, "com.dep", "dep-a", "1.0.0"), "");

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
    write_file(&repo_jar_path(&repo, "com.dep", "dep-b", "2.0.0"), "");

    // Create placeholder jars so `nova-project` can include them on the classpath without
    // requiring network access or invoking Maven.
    for (group_id, artifact_id, version) in [
        ("com.dep", "dep-a", "1.0.0"),
        ("com.dep", "dep-b", "2.0.0"),
    ] {
        let jar = repo_jar_path(&repo, group_id, artifact_id, version);
        fs::create_dir_all(jar.parent().expect("jar parent")).expect("create jar parent");
        fs::write(&jar, b"not really a jar").expect("write fake jar");
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

    // Create placeholder jars in the local repo so `maven_dependency_jar_path` can find them.
    write_file(&repo_jar_path(&repo, "com.dep", "dep-a", "1.0.0"), "");
    write_file(&repo_jar_path(&repo, "com.dep", "dep-b", "2.0.0"), "");

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
