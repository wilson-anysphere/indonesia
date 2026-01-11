use std::collections::BTreeSet;
use std::path::PathBuf;

use nova_project::{load_project, BuildSystem, ClasspathEntryKind, JavaVersion, SourceRootKind};

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

#[test]
fn loads_maven_multi_module_workspace() {
    let root = testdata_path("maven-multi");
    let config = load_project(&root).expect("load maven project");

    assert_eq!(config.build_system, BuildSystem::Maven);
    assert_eq!(config.java.source, JavaVersion(17));
    assert_eq!(config.java.target, JavaVersion(17));

    // Both module source roots should be discovered.
    let roots: BTreeSet<_> = config
        .source_roots
        .iter()
        .map(|sr| {
            (
                sr.kind,
                sr.path
                    .strip_prefix(&config.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();

    assert!(roots.contains(&(SourceRootKind::Main, PathBuf::from("lib/src/main/java"))));
    assert!(roots.contains(&(SourceRootKind::Main, PathBuf::from("app/src/main/java"))));

    // Classpath should include dependency jar placeholders.
    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(jar_entries.iter().any(|p| p
        .to_string_lossy()
        .contains("com/google/guava/guava/33.0.0-jre")));

    // Dependencies should be stable and contain expected coordinates.
    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
        .collect();
    assert!(deps.contains(&(
        "com.google.guava".to_string(),
        "guava".to_string(),
        Some("33.0.0-jre".to_string())
    )));
    assert!(deps.contains(&(
        "org.junit.jupiter".to_string(),
        "junit-jupiter-api".to_string(),
        Some("5.10.0".to_string())
    )));

    // Ensure config is deterministic.
    let config2 = load_project(&root).expect("load maven project again");
    assert_eq!(config, config2);
}

#[test]
fn loads_maven_nested_multi_module_workspace() {
    let root = testdata_path("maven-nested");
    let config = load_project(&root).expect("load maven project");

    let module_roots: BTreeSet<_> = config
        .modules
        .iter()
        .map(|m| {
            m.root
                .strip_prefix(&config.workspace_root)
                .unwrap()
                .to_path_buf()
        })
        .collect();
    assert!(module_roots.contains(&PathBuf::from("parent-a")));
    assert!(module_roots.contains(&PathBuf::from("parent-a/child-a1")));

    let roots: BTreeSet<_> = config
        .source_roots
        .iter()
        .map(|sr| {
            (
                sr.kind,
                sr.path
                    .strip_prefix(&config.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();
    assert!(roots.contains(&(
        SourceRootKind::Main,
        PathBuf::from("parent-a/child-a1/src/main/java")
    )));
}

#[test]
fn loads_gradle_multi_module_workspace() {
    let root = testdata_path("gradle-multi");
    let config = load_project(&root).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.java.source, JavaVersion(17));

    let roots: BTreeSet<_> = config
        .source_roots
        .iter()
        .map(|sr| {
            (
                sr.kind,
                sr.path
                    .strip_prefix(&config.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();
    assert!(roots.contains(&(SourceRootKind::Main, PathBuf::from("lib/src/main/java"))));
    assert!(roots.contains(&(SourceRootKind::Main, PathBuf::from("app/src/main/java"))));

    let deps: BTreeSet<_> = config
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
        .collect();
    assert!(deps.contains(&(
        "com.google.guava".to_string(),
        "guava".to_string(),
        Some("33.0.0-jre".to_string())
    )));

    let config2 = load_project(&root).expect("load gradle project again");
    assert_eq!(config, config2);
}
