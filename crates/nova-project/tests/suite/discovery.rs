use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use nova_project::{
    load_project, load_project_with_options, load_workspace_model_with_options, BuildSystem,
    ClasspathEntryKind, JavaVersion, LanguageLevelProvenance, LoadOptions, OutputDirKind,
    SourceRootKind, SourceRootOrigin,
};
use tempfile::tempdir;

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

#[test]
fn loads_maven_multi_module_workspace() {
    let root = testdata_path("maven-multi");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");

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

    // Multi-module workspaces can have very large dependency closures. When the local Maven repo is
    // empty, avoid synthesizing missing dependency jars to keep the classpath lean.
    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        jar_entries.is_empty(),
        "expected no jar entries with empty repo, found: {jar_entries:?}"
    );

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
    let config2 = load_project_with_options(&root, &options).expect("load maven project again");
    assert_eq!(config, config2);
}

#[test]
fn loads_maven_nested_multi_module_workspace() {
    let root = testdata_path("maven-nested");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");

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

    // Ensure config is deterministic.
    let config2 = load_project_with_options(&root, &options).expect("load maven project again");
    assert_eq!(config, config2);
}

#[test]
fn resolves_maven_nested_properties() {
    let root = testdata_path("maven-nested-properties");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");

    let dep = config
        .dependencies
        .iter()
        .find(|d| d.group_id == "com.example" && d.artifact_id == "managed-dep")
        .expect("expected managed dependency to be discovered");
    assert_eq!(dep.version, Some("1.2.3".to_string()));

    // This test uses an empty repo; version resolution is validated via the dependency list.
}

#[test]
fn resolves_inherited_maven_managed_versions_with_child_property_overrides() {
    let root = testdata_path("maven-nested-properties-override");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");

    let dep = config
        .dependencies
        .iter()
        .find(|d| d.group_id == "com.example" && d.artifact_id == "managed-dep")
        .expect("expected managed dependency to be discovered");
    assert_eq!(dep.version, Some("2.0.0".to_string()));

    // This test uses an empty repo; version resolution is validated via the dependency list.
}

#[test]
fn resolves_maven_java_version_placeholders() {
    let root = testdata_path("maven-java-placeholder");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");

    assert_eq!(config.build_system, BuildSystem::Maven);
    assert_eq!(config.java.source, JavaVersion(11));
    assert_eq!(config.java.target, JavaVersion(11));
}

#[test]
fn loads_maven_java_property_placeholders() {
    let root = testdata_path("maven-java-property-placeholders");
    let config = load_project(&root).expect("load maven project");

    assert_eq!(config.build_system, BuildSystem::Maven);
    assert_eq!(config.java.source, JavaVersion(21));
    assert_eq!(config.java.target, JavaVersion(21));

    // Ensure config is deterministic.
    let config2 = load_project(&root).expect("load maven project again");
    assert_eq!(config, config2);
}

#[test]
fn loads_maven_profile_modules_active_by_default() {
    let root = testdata_path("maven-profile-modules");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");

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
    assert!(module_roots.contains(&PathBuf::from("child")));

    let source_roots: BTreeSet<_> = config
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
    assert!(source_roots.contains(&(SourceRootKind::Main, PathBuf::from("child/src/main/java"))));
}

#[test]
fn resolves_maven_managed_dependency_coordinates_placeholders() {
    let root = testdata_path("maven-managed-coordinates-placeholder");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");

    let dep = config
        .dependencies
        .iter()
        .find(|d| d.group_id == "com.example" && d.artifact_id == "managed-dep")
        .expect("expected managed dependency to be discovered");
    assert_eq!(dep.version, Some("1.2.3".to_string()));

    let jar_path = repo_dir
        .path()
        .join("com/example/managed-dep/1.2.3/managed-dep-1.2.3.jar");
    assert!(
        !jar_path.is_file(),
        "jar should not exist yet for this test: {jar_path:?}"
    );
    let jar_entries: Vec<String> = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.to_string_lossy().replace('\\', "/"))
        .collect();
    let jar_path_str = jar_path.to_string_lossy().replace('\\', "/");
    assert!(
        !jar_entries.iter().any(|p| p == &jar_path_str),
        "expected managed-dep jar to be omitted when it is missing on disk, got: {jar_entries:?}"
    );
    assert!(jar_path_str.contains("/1.2.3/"), "jar path: {jar_path_str}");
    assert!(!jar_path_str.contains("${"), "jar path: {jar_path_str}");

    // Creating the jar should make the expected path appear on the classpath.
    std::fs::create_dir_all(jar_path.parent().expect("jar parent")).expect("mkdir jar parent");
    std::fs::write(&jar_path, b"").expect("write jar placeholder");

    let config2 = load_project_with_options(&root, &options).expect("reload maven project");
    let jar_entries2: Vec<String> = config2
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.to_string_lossy().replace('\\', "/"))
        .collect();
    assert!(
        jar_entries2.iter().any(|p| p == &jar_path_str),
        "expected managed-dep jar to be present after creation, got: {jar_entries2:?}"
    );
}

#[test]
fn loads_gradle_multi_module_workspace() {
    let root = testdata_path("gradle-multi");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

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

    let config2 = load_project_with_options(&root, &options).expect("load gradle project again");
    assert_eq!(config, config2);
}

#[test]
fn loads_gradle_multi_module_workspace_includes_root_project_when_it_has_sources() {
    let root = testdata_path("gradle-multi-root-sources");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

    // Root sources should be indexed, even when `settings.gradle` includes subprojects.
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
    assert!(roots.contains(&(SourceRootKind::Main, PathBuf::from("src/main/java"))));

    // For determinism, the root module is always first.
    assert_eq!(config.modules[0].root, config.workspace_root);

    // Ensure config is deterministic.
    let config2 = load_project_with_options(&root, &options).expect("load gradle project again");
    assert_eq!(config, config2);
}

#[test]
fn loads_gradle_includeflat_workspace() {
    let root = testdata_path("gradle-includeflat/root");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

    let expected_app_root =
        fs::canonicalize(config.workspace_root.join("../app")).expect("canonicalize ../app");
    let expected_lib_root =
        fs::canonicalize(config.workspace_root.join("../lib")).expect("canonicalize ../lib");
    let module_roots: Vec<_> = config.modules.iter().map(|m| m.root.clone()).collect();
    assert!(
        config.modules.iter().any(|m| m.root == expected_app_root),
        "expected includeFlat module root to canonicalize to {expected_app_root:?}; got: {module_roots:?}",
    );
    assert!(
        config.modules.iter().any(|m| m.root == expected_lib_root),
        "expected includeFlat module root to canonicalize to {expected_lib_root:?}; got: {module_roots:?}",
    );

    let expected_app_source_root = expected_app_root.join("src/main/java");
    let expected_lib_source_root = expected_lib_root.join("src/main/java");
    assert!(
        config
            .source_roots
            .iter()
            .any(|sr| sr.kind == SourceRootKind::Main && sr.path == expected_app_source_root),
        "expected includeFlat source root to resolve to {expected_app_source_root:?}. got: {:?}",
        config.source_roots
    );
    assert!(
        config
            .source_roots
            .iter()
            .any(|sr| sr.kind == SourceRootKind::Main && sr.path == expected_lib_source_root),
        "expected includeFlat source root to resolve to {expected_lib_source_root:?}. got: {:?}",
        config.source_roots
    );
}

#[test]
fn loads_gradle_includeflat_kts_workspace() {
    let root = testdata_path("gradle-includeflat/root-kts");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

    let expected_app_root =
        fs::canonicalize(config.workspace_root.join("../app")).expect("canonicalize ../app");
    let expected_lib_root =
        fs::canonicalize(config.workspace_root.join("../lib")).expect("canonicalize ../lib");
    let module_roots: Vec<_> = config.modules.iter().map(|m| m.root.clone()).collect();
    assert!(
        config.modules.iter().any(|m| m.root == expected_app_root),
        "expected includeFlat module root to canonicalize to {expected_app_root:?}; got: {module_roots:?}",
    );
    assert!(
        config.modules.iter().any(|m| m.root == expected_lib_root),
        "expected includeFlat module root to canonicalize to {expected_lib_root:?}; got: {module_roots:?}",
    );

    let expected_app_source_root = expected_app_root.join("src/main/java");
    let expected_lib_source_root = expected_lib_root.join("src/main/java");
    assert!(
        config
            .source_roots
            .iter()
            .any(|sr| sr.kind == SourceRootKind::Main && sr.path == expected_app_source_root),
        "expected includeFlat source root to resolve to {expected_app_source_root:?}. got: {:?}",
        config.source_roots
    );
    assert!(
        config
            .source_roots
            .iter()
            .any(|sr| sr.kind == SourceRootKind::Main && sr.path == expected_lib_source_root),
        "expected includeFlat source root to resolve to {expected_lib_source_root:?}. got: {:?}",
        config.source_roots
    );
}

#[test]
fn loads_gradle_root_buildscript_dependencies() {
    let root = testdata_path("gradle-multi-root-deps");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");
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
}

#[test]
fn loads_gradle_root_buildscript_dependencies_kotlin_dsl() {
    let root = testdata_path("gradle-multi-root-deps-kts");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

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
}

#[test]
fn loads_gradle_root_buildscript_dependencies_with_version_catalog() {
    let root = testdata_path("gradle-multi-root-deps-version-catalog");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

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
}

#[test]
fn loads_gradle_root_buildscript_dependencies_with_root_version_catalog() {
    let root = testdata_path("gradle-multi-root-deps-version-catalog-root");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

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
}

#[test]
fn loads_gradle_composite_workspace() {
    let root = testdata_path("gradle-composite/root");
    let workspace_root = std::fs::canonicalize(&root).expect("canonicalize workspace root");
    let included_root = std::fs::canonicalize(workspace_root.join("../included"))
        .expect("canonicalize included build root");
    let included_src = included_root.join("src/main/java");

    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert!(
        config.modules.iter().any(|m| m.root == included_root),
        "expected included build root module to be discovered; got: {:?}",
        config.modules
    );
    assert!(
        config
            .source_roots
            .iter()
            .any(|sr| sr.kind == SourceRootKind::Main && sr.path == included_src),
        "expected included build source root to be discovered; got: {:?}",
        config.source_roots
    );

    // Ensure config is deterministic.
    let config2 = load_project_with_options(&root, &options).expect("load gradle project again");
    assert_eq!(config, config2);
}

#[test]
fn loads_gradle_projectdir_mapping_workspace() {
    let root = testdata_path("gradle-projectdir-mapping");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

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
        PathBuf::from("modules/app/src/main/java")
    )));
    assert!(roots.contains(&(
        SourceRootKind::Main,
        PathBuf::from("modules/lib/src/main/java")
    )));
}

#[test]
fn loads_gradle_projectdir_mapping_kts_workspace() {
    let root = testdata_path("gradle-projectdir-mapping-kts");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

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
        PathBuf::from("modules/app/src/main/java")
    )));
    assert!(roots.contains(&(
        SourceRootKind::Main,
        PathBuf::from("modules/lib/src/main/java")
    )));
}

#[test]
fn loads_gradle_custom_source_sets_workspace() {
    let root = testdata_path("gradle-custom-sourcesets");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

    let roots: BTreeSet<_> = config
        .source_roots
        .iter()
        .map(|sr| {
            (
                sr.kind,
                sr.origin,
                sr.path
                    .strip_prefix(&config.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();

    assert!(roots.contains(&(
        SourceRootKind::Test,
        SourceRootOrigin::Source,
        PathBuf::from("app/src/integrationTest/java")
    )));

    let config2 = load_project_with_options(&root, &options).expect("load gradle project again");
    assert_eq!(config, config2);
}

#[test]
fn loads_gradle_workspace_java_config_max_across_modules() {
    let root = testdata_path("gradle-multi-java-config");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.java.source, JavaVersion(17));
    assert_eq!(config.java.target, JavaVersion(17));

    let config2 = load_project_with_options(&root, &options).expect("load gradle project again");
    assert_eq!(config, config2);
}

#[test]
fn loads_gradle_toolchain_language_version() {
    let root = testdata_path("gradle-toolchain-only");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.java.source, JavaVersion(21));
    assert_eq!(config.java.target, JavaVersion(21));
}

#[test]
fn gradle_java_version_is_max_across_modules_without_snapshot() {
    let root = testdata_path("gradle-mixed-java-levels");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    // Root build script targets Java 11; `:app` targets Java 17. Without a Gradle snapshot, Nova
    // should best-effort aggregate to the maximum across modules.
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.java.source, JavaVersion(17));
    assert_eq!(config.java.target, JavaVersion(17));
}

#[test]
fn gradle_enable_preview_is_or_across_modules_without_snapshot() {
    let root = testdata_path("gradle-preview-multi");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);

    // Root build script targets Java 11; `:app` targets Java 17 and enables preview features.
    // Without a Gradle snapshot, Nova should best-effort aggregate Java config across modules by:
    // - taking the max source/target version
    // - OR-ing enable_preview
    assert_eq!(config.java.source, JavaVersion(17));
    assert_eq!(config.java.target, JavaVersion(17));
    assert!(config.java.enable_preview);
}

#[test]
fn gradle_workspace_model_java_version_is_max_across_modules_without_snapshot() {
    let root = testdata_path("gradle-preview-multi");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    // Root build script targets Java 11; `:app` targets Java 17 and enables preview features.
    // Without a Gradle snapshot, Nova should best-effort aggregate Java config across modules by:
    // - taking the max source/target version
    // - OR-ing enable_preview
    assert_eq!(model.build_system, BuildSystem::Gradle);
    assert_eq!(model.java.source, JavaVersion(17));
    assert_eq!(model.java.target, JavaVersion(17));
    assert!(model.java.enable_preview);
}

#[test]
fn gradle_source_compatibility_overrides_toolchain_language_version() {
    let root = testdata_path("gradle-toolchain-with-compat");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(config.java.source, JavaVersion(17));
    assert_eq!(config.java.target, JavaVersion(17));
}

#[test]
fn loads_gradle_local_jar_dependencies() {
    let root = testdata_path("gradle-local-jars");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

    assert!(
        config.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&config.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("libs/local.jar"))
        }),
        "expected libs/local.jar to be present on the resolved classpath"
    );

    assert!(
        config.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&config.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("libs/tree-only.jar"))
        }),
        "expected fileTree(dir: \"libs\") to contribute jars on the resolved classpath"
    );
}

#[test]
fn loads_gradle_local_jar_dependencies_kotlin_dsl_filetree() {
    let root = testdata_path("gradle-local-jars-kotlin");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load gradle project");

    assert_eq!(config.build_system, BuildSystem::Gradle);

    assert!(
        config.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&config.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("libs/tree-only.jar"))
        }),
        "expected Kotlin DSL fileTree(\"libs\") to contribute jars on the resolved classpath"
    );

    assert!(
        config.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&config.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("other-libs/map-only.jar"))
        }),
        "expected Kotlin DSL fileTree(mapOf(\"dir\" to ...)) to contribute jars on the resolved classpath"
    );

    assert!(
        config.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&config.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("file-libs/file-only.jar"))
        }),
        "expected Kotlin DSL fileTree(dir = file(\"...\")) to contribute jars on the resolved classpath"
    );
}

#[test]
fn loads_maven_multi_module_workspace_model() {
    let root = testdata_path("maven-multi");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load maven workspace model");

    assert_eq!(model.build_system, BuildSystem::Maven);

    let app = model
        .module_by_id("maven:com.example:app")
        .expect("app module");
    let lib = model
        .module_by_id("maven:com.example:lib")
        .expect("lib module");

    // App depends on lib; ensure the lib output directory is on app's classpath.
    let app_classpath_dirs: BTreeSet<_> = app
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Directory)
        .map(|cp| {
            cp.path
                .strip_prefix(&model.workspace_root)
                .unwrap()
                .to_path_buf()
        })
        .collect();
    assert!(
        app_classpath_dirs.contains(&PathBuf::from("lib/target/classes")),
        "expected app classpath to contain lib/target/classes"
    );

    let app_source_roots: BTreeSet<_> = app
        .source_roots
        .iter()
        .map(|sr| {
            (
                sr.kind,
                sr.origin,
                sr.path
                    .strip_prefix(&model.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();
    assert!(app_source_roots.contains(&(
        SourceRootKind::Main,
        SourceRootOrigin::Source,
        PathBuf::from("app/src/main/java")
    )));

    let app_outputs: BTreeSet<_> = app
        .output_dirs
        .iter()
        .map(|out| {
            (
                out.kind,
                out.path
                    .strip_prefix(&model.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();
    assert!(app_outputs.contains(&(OutputDirKind::Main, PathBuf::from("app/target/classes"))));
    assert!(app_outputs.contains(&(
        OutputDirKind::Test,
        PathBuf::from("app/target/test-classes")
    )));

    let lib_source_roots: BTreeSet<_> = lib
        .source_roots
        .iter()
        .map(|sr| {
            (
                sr.kind,
                sr.origin,
                sr.path
                    .strip_prefix(&model.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();
    assert!(lib_source_roots.contains(&(
        SourceRootKind::Main,
        SourceRootOrigin::Source,
        PathBuf::from("lib/src/main/java")
    )));

    let app_file = model
        .workspace_root
        .join("app/src/main/java/com/example/app/App.java");
    let match_app = model
        .module_for_path(&app_file)
        .expect("module for App.java");
    assert_eq!(match_app.module.id, "maven:com.example:app");
    assert_eq!(match_app.source_root.kind, SourceRootKind::Main);

    let lib_file = model
        .workspace_root
        .join("lib/src/main/java/com/example/lib/Lib.java");
    let match_lib = model
        .module_for_path(&lib_file)
        .expect("module for Lib.java");
    assert_eq!(match_lib.module.id, "maven:com.example:lib");
    assert_eq!(match_lib.source_root.kind, SourceRootKind::Main);

    // Non-JPMS Maven workspace model: `module_path` should stay empty. For multi-module workspaces
    // we avoid synthesizing missing dependency jars to keep classpaths lean; this test uses an
    // empty Maven repo.
    assert!(
        app.module_path.is_empty(),
        "expected module_path to remain empty for non-JPMS workspaces"
    );
    let jar_entries = app
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(
        jar_entries.is_empty(),
        "expected no jar entries with empty repo, found: {jar_entries:?}"
    );

    // Ensure model is deterministic.
    let model2 = load_workspace_model_with_options(&root, &options)
        .expect("load maven workspace model again");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
}

#[test]
fn maven_workspace_model_java_version_is_max_across_modules() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("workspace");
    fs::create_dir_all(&root).expect("mkdir workspace");

    // Root declares Java 11; `app` overrides to Java 21; `lib` overrides to Java 8.
    fs::write(
        root.join("pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>root</artifactId>
              <version>1.0.0</version>
              <packaging>pom</packaging>
              <properties>
                <maven.compiler.source>11</maven.compiler.source>
                <maven.compiler.target>11</maven.compiler.target>
              </properties>
              <modules>
                <module>app</module>
                <module>lib</module>
              </modules>
            </project>
        "#,
    )
    .expect("write root pom.xml");

    fs::create_dir_all(root.join("app")).expect("mkdir app");
    fs::write(
        root.join("app/pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <parent>
                <groupId>com.example</groupId>
                <artifactId>root</artifactId>
                <version>1.0.0</version>
              </parent>
              <artifactId>app</artifactId>
              <properties>
                <maven.compiler.release>21</maven.compiler.release>
              </properties>
            </project>
        "#,
    )
    .expect("write app pom.xml");

    fs::create_dir_all(root.join("lib")).expect("mkdir lib");
    fs::write(
        root.join("lib/pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <parent>
                <groupId>com.example</groupId>
                <artifactId>root</artifactId>
                <version>1.0.0</version>
              </parent>
              <artifactId>lib</artifactId>
              <properties>
                <maven.compiler.release>8</maven.compiler.release>
              </properties>
            </project>
        "#,
    )
    .expect("write lib pom.xml");

    let maven_repo = temp.path().join("m2");
    fs::create_dir_all(&maven_repo).expect("mkdir maven repo");
    let options = LoadOptions {
        maven_repo: Some(maven_repo),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(&root, &options).expect("load maven workspace model");
    assert_eq!(model.build_system, BuildSystem::Maven);
    assert_eq!(model.java.source, JavaVersion(21));
    assert_eq!(model.java.target, JavaVersion(21));

    // `ProjectConfig` should match the workspace model's aggregated Java config.
    let config = load_project_with_options(&root, &options).expect("load maven project");
    assert_eq!(config.java.source, JavaVersion(21));
    assert_eq!(config.java.target, JavaVersion(21));
}

#[test]
fn loads_gradle_multi_module_workspace_model() {
    let root = testdata_path("gradle-multi");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    // The `gradle-multi` fixture is an aggregator-only root project (no `src/main/java`), so we
    // should not synthesize a root module.
    assert!(model.module_by_id("gradle::").is_none());

    let _app = model.module_by_id("gradle::app").expect("app module");
    let lib = model.module_by_id("gradle::lib").expect("lib module");

    let lib_outputs: BTreeSet<_> = lib
        .output_dirs
        .iter()
        .map(|out| {
            (
                out.kind,
                out.path
                    .strip_prefix(&model.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();
    assert!(lib_outputs.contains(&(
        OutputDirKind::Main,
        PathBuf::from("lib/build/classes/java/main")
    )));
    assert!(lib_outputs.contains(&(
        OutputDirKind::Test,
        PathBuf::from("lib/build/classes/java/test")
    )));

    let app_file = model
        .workspace_root
        .join("app/src/main/java/com/example/app/App.java");
    let match_app = model
        .module_for_path(&app_file)
        .expect("module for App.java");
    assert_eq!(match_app.module.id, "gradle::app");
    assert_eq!(match_app.source_root.kind, SourceRootKind::Main);

    let lib_file = model
        .workspace_root
        .join("lib/src/main/java/com/example/lib/Lib.java");
    let match_lib = model
        .module_for_path(&lib_file)
        .expect("module for Lib.java");
    assert_eq!(match_lib.module.id, "gradle::lib");
    assert_eq!(match_lib.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_project_dependencies_into_module_classpath() {
    let root = testdata_path("gradle-project-deps");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let app = model.module_by_id("gradle::app").expect("app module");
    let app_classpath_dirs: BTreeSet<_> = app
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Directory)
        .map(|cp| {
            cp.path
                .strip_prefix(&model.workspace_root)
                .unwrap()
                .to_path_buf()
        })
        .collect();
    assert!(
        app_classpath_dirs.contains(&PathBuf::from("lib/build/classes/java/main")),
        "expected app classpath to contain lib/build/classes/java/main"
    );

    let app_file = model
        .workspace_root
        .join("app/src/main/java/com/example/app/App.java");
    let match_app = model
        .module_for_path(&app_file)
        .expect("module for App.java");
    assert_eq!(match_app.module.id, "gradle::app");
    assert_eq!(match_app.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_multi_module_workspace_model_includes_root_project_when_it_has_sources() {
    let root = testdata_path("gradle-multi-root-sources");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    // For determinism, the root module is always first.
    assert_eq!(model.modules[0].id, "gradle::");

    let root_module = model.module_by_id("gradle::").expect("root gradle module");
    let root_source_roots: BTreeSet<_> = root_module
        .source_roots
        .iter()
        .map(|sr| {
            (
                sr.kind,
                sr.origin,
                sr.path
                    .strip_prefix(&model.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();
    assert!(root_source_roots.contains(&(
        SourceRootKind::Main,
        SourceRootOrigin::Source,
        PathBuf::from("src/main/java")
    )));

    let root_file = model
        .workspace_root
        .join("src/main/java/com/example/root/Root.java");
    let match_root = model
        .module_for_path(&root_file)
        .expect("module for Root.java");
    assert_eq!(match_root.module.id, "gradle::");
    assert_eq!(match_root.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_includeflat_workspace_model() {
    let root = testdata_path("gradle-includeflat/root");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let app = model.module_by_id("gradle::app").expect("app module");
    let expected_app_root =
        std::fs::canonicalize(model.workspace_root.join("../app")).expect("canonicalize app root");
    assert_eq!(app.root, expected_app_root);
    let lib = model.module_by_id("gradle::lib").expect("lib module");
    let expected_lib_root =
        std::fs::canonicalize(model.workspace_root.join("../lib")).expect("canonicalize lib root");
    assert_eq!(lib.root, expected_lib_root);

    let app_file = std::fs::canonicalize(
        model
            .workspace_root
            .join("../app/src/main/java/com/example/app/App.java"),
    )
    .expect("canonicalize app file");
    let match_app = model
        .module_for_path(&app_file)
        .expect("module for App.java");
    assert_eq!(match_app.module.id, "gradle::app");
    assert_eq!(match_app.source_root.kind, SourceRootKind::Main);

    let lib_file = std::fs::canonicalize(
        model
            .workspace_root
            .join("../lib/src/main/java/com/example/lib/Lib.java"),
    )
    .expect("canonicalize lib file");
    let match_lib = model
        .module_for_path(&lib_file)
        .expect("module for Lib.java");
    assert_eq!(match_lib.module.id, "gradle::lib");
    assert_eq!(match_lib.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_includeflat_kts_workspace_model() {
    let root = testdata_path("gradle-includeflat/root-kts");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let app = model.module_by_id("gradle::app").expect("app module");
    let lib = model.module_by_id("gradle::lib").expect("lib module");
    let expected_app_root =
        std::fs::canonicalize(model.workspace_root.join("../app")).expect("canonicalize app root");
    assert_eq!(app.root, expected_app_root);
    let expected_lib_root =
        std::fs::canonicalize(model.workspace_root.join("../lib")).expect("canonicalize lib root");
    assert_eq!(lib.root, expected_lib_root);

    let app_file = std::fs::canonicalize(
        model
            .workspace_root
            .join("../app/src/main/java/com/example/app/App.java"),
    )
    .expect("canonicalize app file");
    let match_app = model
        .module_for_path(&app_file)
        .expect("module for App.java");
    assert_eq!(match_app.module.id, "gradle::app");
    assert_eq!(match_app.source_root.kind, SourceRootKind::Main);

    let lib_file = std::fs::canonicalize(
        model
            .workspace_root
            .join("../lib/src/main/java/com/example/lib/Lib.java"),
    )
    .expect("canonicalize lib file");
    let match_lib = model
        .module_for_path(&lib_file)
        .expect("module for Lib.java");
    assert_eq!(match_lib.module.id, "gradle::lib");
    assert_eq!(match_lib.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_root_buildscript_dependencies_workspace_model() {
    let root = testdata_path("gradle-multi-root-deps");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    let app = model.module_by_id("gradle::app").expect("app module");
    let deps: BTreeSet<_> = app
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
        .collect();
    assert!(deps.contains(&(
        "com.google.guava".to_string(),
        "guava".to_string(),
        Some("33.0.0-jre".to_string())
    )));
}

#[test]
fn loads_gradle_root_buildscript_dependencies_kotlin_dsl_workspace_model() {
    let root = testdata_path("gradle-multi-root-deps-kts");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    let app = model.module_by_id("gradle::app").expect("app module");
    let deps: BTreeSet<_> = app
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
        .collect();
    assert!(deps.contains(&(
        "com.google.guava".to_string(),
        "guava".to_string(),
        Some("33.0.0-jre".to_string())
    )));
}

#[test]
fn loads_gradle_root_buildscript_dependencies_with_version_catalog_workspace_model() {
    let root = testdata_path("gradle-multi-root-deps-version-catalog");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    let app = model.module_by_id("gradle::app").expect("app module");
    let deps: BTreeSet<_> = app
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
        .collect();
    assert!(deps.contains(&(
        "com.google.guava".to_string(),
        "guava".to_string(),
        Some("33.0.0-jre".to_string())
    )));
}

#[test]
fn loads_gradle_root_buildscript_dependencies_with_root_version_catalog_workspace_model() {
    let root = testdata_path("gradle-multi-root-deps-version-catalog-root");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    let app = model.module_by_id("gradle::app").expect("app module");
    let deps: BTreeSet<_> = app
        .dependencies
        .iter()
        .map(|d| (d.group_id.clone(), d.artifact_id.clone(), d.version.clone()))
        .collect();
    assert!(deps.contains(&(
        "com.google.guava".to_string(),
        "guava".to_string(),
        Some("33.0.0-jre".to_string())
    )));
}

#[test]
fn loads_gradle_includebuild_workspace_model() {
    let root = testdata_path("gradle-includebuild");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");
    assert_eq!(model.build_system, BuildSystem::Gradle);

    let build_logic_root = model.workspace_root.join("build-logic");
    let build_logic = model
        .modules
        .iter()
        .find(|m| m.root == build_logic_root)
        .expect("expected build-logic module to be discovered via includeBuild");
    assert_eq!(build_logic.id, "gradle::__includedBuild_build-logic");

    let java_file = model
        .workspace_root
        .join("build-logic/src/main/java/com/example/buildlogic/BuildLogic.java");
    let match_java = model
        .module_for_path(&java_file)
        .expect("module for build-logic java file");
    assert_eq!(match_java.module.id, build_logic.id);
    assert_eq!(match_java.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_includebuild_kts_workspace_model() {
    let root = testdata_path("gradle-includebuild-kts");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");
    assert_eq!(model.build_system, BuildSystem::Gradle);

    let build_logic_root = model.workspace_root.join("build-logic");
    let build_logic = model
        .modules
        .iter()
        .find(|m| m.root == build_logic_root)
        .expect("expected build-logic module to be discovered via includeBuild");
    assert_eq!(build_logic.id, "gradle::__includedBuild_build-logic");

    let java_file = model
        .workspace_root
        .join("build-logic/src/main/java/com/example/buildlogic/BuildLogic.java");
    let match_java = model
        .module_for_path(&java_file)
        .expect("module for build-logic java file");
    assert_eq!(match_java.module.id, build_logic.id);
    assert_eq!(match_java.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_includebuild_dependencies_from_included_build_version_catalog() {
    let root = testdata_path("gradle-includebuild-version-catalog");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert!(
        config.dependencies.iter().any(|d| {
            d.group_id == "com.google.guava"
                && d.artifact_id == "guava"
                && d.version.as_deref() == Some("33.0.0-jre")
        }),
        "expected workspace dependency list to include guava from included build version catalog"
    );

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");
    let included = model
        .module_by_id("gradle::__includedBuild_build-logic")
        .expect("included build module");
    assert!(
        included.dependencies.iter().any(|d| {
            d.group_id == "com.google.guava"
                && d.artifact_id == "guava"
                && d.version.as_deref() == Some("33.0.0-jre")
        }),
        "expected included build module to include guava from its own version catalog"
    );
}

#[test]
fn loads_gradle_composite_workspace_model() {
    let root = testdata_path("gradle-composite/root");
    let included_root =
        std::fs::canonicalize(root.join("../included")).expect("canonicalize included build root");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let included = model
        .modules
        .iter()
        .find(|m| m.root == included_root)
        .expect("expected included build module to be discovered via includeBuild");
    assert_eq!(included.id, "gradle::__includedBuild_included");
}

#[test]
fn loads_gradle_includebuild_subproject_workspace_model() {
    let root = testdata_path("gradle-includebuild");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");
    assert_eq!(model.build_system, BuildSystem::Gradle);

    let plugins_root = model.workspace_root.join("build-logic/plugins");
    let plugins = model
        .modules
        .iter()
        .find(|m| m.root == plugins_root)
        .expect("expected build-logic/plugins module to be discovered via includeBuild settings");
    assert_eq!(plugins.id, "gradle::__includedBuild_build-logic:plugins");

    let java_file = model
        .workspace_root
        .join("build-logic/plugins/src/main/java/com/example/buildlogic/plugins/Plugin.java");
    let match_java = model
        .module_for_path(&java_file)
        .expect("module for build-logic/plugins java file");
    assert_eq!(match_java.module.id, plugins.id);
    assert_eq!(match_java.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_includebuild_kts_subproject_workspace_model() {
    let root = testdata_path("gradle-includebuild-kts");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");
    assert_eq!(model.build_system, BuildSystem::Gradle);

    let plugins_root = model.workspace_root.join("build-logic/plugins");
    let plugins = model
        .modules
        .iter()
        .find(|m| m.root == plugins_root)
        .expect("expected build-logic/plugins module to be discovered via includeBuild settings");
    assert_eq!(plugins.id, "gradle::__includedBuild_build-logic:plugins");

    let java_file = model
        .workspace_root
        .join("build-logic/plugins/src/main/java/com/example/buildlogic/plugins/Plugin.java");
    let match_java = model
        .module_for_path(&java_file)
        .expect("module for build-logic/plugins java file");
    assert_eq!(match_java.module.id, plugins.id);
    assert_eq!(match_java.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_includebuild_subproject_project_dependencies_are_scoped_to_included_build() {
    let root = testdata_path("gradle-includebuild");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");
    assert_eq!(model.build_system, BuildSystem::Gradle);

    let plugins = model
        .module_by_id("gradle::__includedBuild_build-logic:plugins")
        .expect("plugins module");

    let classpath_dirs: BTreeSet<_> = plugins
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Directory)
        .map(|cp| {
            cp.path
                .strip_prefix(&model.workspace_root)
                .unwrap()
                .to_path_buf()
        })
        .collect();

    assert!(
        classpath_dirs.contains(&PathBuf::from("build-logic/build/classes/java/main")),
        "expected plugins classpath to include included build root output dir"
    );
    assert!(
        !classpath_dirs.contains(&PathBuf::from("build/classes/java/main")),
        "did not expect plugins classpath to include outer build root output dir"
    );
}

#[test]
fn loads_gradle_projectdir_mapping_workspace_model() {
    let root = testdata_path("gradle-projectdir-mapping");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let app = model.module_by_id("gradle::app").expect("app module");
    let lib = model.module_by_id("gradle::lib").expect("lib module");

    assert_eq!(
        app.root
            .strip_prefix(&model.workspace_root)
            .unwrap()
            .to_path_buf(),
        PathBuf::from("modules/app")
    );
    assert_eq!(
        lib.root
            .strip_prefix(&model.workspace_root)
            .unwrap()
            .to_path_buf(),
        PathBuf::from("modules/lib")
    );

    let app_file = model
        .workspace_root
        .join("modules/app/src/main/java/com/example/app/App.java");
    let match_app = model
        .module_for_path(&app_file)
        .expect("module for App.java");
    assert_eq!(match_app.module.id, "gradle::app");
    assert_eq!(match_app.source_root.kind, SourceRootKind::Main);

    let lib_file = model
        .workspace_root
        .join("modules/lib/src/main/java/com/example/lib/Lib.java");
    let match_lib = model
        .module_for_path(&lib_file)
        .expect("module for Lib.java");
    assert_eq!(match_lib.module.id, "gradle::lib");
    assert_eq!(match_lib.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_projectdir_mapping_kts_workspace_model() {
    let root = testdata_path("gradle-projectdir-mapping-kts");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let app = model.module_by_id("gradle::app").expect("app module");
    let lib = model.module_by_id("gradle::lib").expect("lib module");

    assert_eq!(
        app.root
            .strip_prefix(&model.workspace_root)
            .unwrap()
            .to_path_buf(),
        PathBuf::from("modules/app")
    );
    assert_eq!(
        lib.root
            .strip_prefix(&model.workspace_root)
            .unwrap()
            .to_path_buf(),
        PathBuf::from("modules/lib")
    );

    let app_file = model
        .workspace_root
        .join("modules/app/src/main/java/com/example/app/App.java");
    let match_app = model
        .module_for_path(&app_file)
        .expect("module for App.java");
    assert_eq!(match_app.module.id, "gradle::app");
    assert_eq!(match_app.source_root.kind, SourceRootKind::Main);

    let lib_file = model
        .workspace_root
        .join("modules/lib/src/main/java/com/example/lib/Lib.java");
    let match_lib = model
        .module_for_path(&lib_file)
        .expect("module for Lib.java");
    assert_eq!(match_lib.module.id, "gradle::lib");
    assert_eq!(match_lib.source_root.kind, SourceRootKind::Main);
}

#[test]
fn loads_gradle_custom_source_sets_workspace_model() {
    let root = testdata_path("gradle-custom-sourcesets");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let app = model.module_by_id("gradle::app").expect("app module");
    let app_source_roots: BTreeSet<_> = app
        .source_roots
        .iter()
        .map(|sr| {
            (
                sr.kind,
                sr.origin,
                sr.path
                    .strip_prefix(&model.workspace_root)
                    .unwrap()
                    .to_path_buf(),
            )
        })
        .collect();
    assert!(app_source_roots.contains(&(
        SourceRootKind::Test,
        SourceRootOrigin::Source,
        PathBuf::from("app/src/integrationTest/java")
    )));

    let it_file = model
        .workspace_root
        .join("app/src/integrationTest/java/com/example/app/AppIT.java");
    let match_it = model
        .module_for_path(&it_file)
        .expect("module for AppIT.java");
    assert_eq!(match_it.module.id, "gradle::app");
    assert_eq!(match_it.source_root.kind, SourceRootKind::Test);
    assert_eq!(match_it.source_root.origin, SourceRootOrigin::Source);
}

#[test]
fn loads_gradle_local_jar_dependencies_into_workspace_model() {
    let root = testdata_path("gradle-local-jars");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let root_module = model.module_by_id("gradle::").expect("root module");

    assert!(
        root_module.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&model.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("libs/local.jar"))
        }),
        "expected libs/local.jar to be present on the module classpath"
    );
}

#[test]
fn loads_gradle_local_jar_dependencies_kotlin_dsl_filetree_into_workspace_model() {
    let root = testdata_path("gradle-local-jars-kotlin");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);

    let root_module = model.module_by_id("gradle::").expect("root module");

    assert!(
        root_module.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&model.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("libs/tree-only.jar"))
        }),
        "expected Kotlin DSL fileTree(\"libs\") to contribute jars on the module classpath"
    );

    assert!(
        root_module.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&model.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("other-libs/map-only.jar"))
        }),
        "expected Kotlin DSL fileTree(mapOf(\"dir\" to ...)) to contribute jars on the module classpath"
    );

    assert!(
        root_module.classpath.iter().any(|cp| {
            cp.kind == ClasspathEntryKind::Jar
                && cp
                    .path
                    .strip_prefix(&model.workspace_root)
                    .ok()
                    .is_some_and(|p| p == std::path::Path::new("file-libs/file-only.jar"))
        }),
        "expected Kotlin DSL fileTree(dir = file(\"...\")) to contribute jars on the module classpath"
    );
}

#[test]
fn loads_maven_compiler_plugin_language_level() {
    let root = testdata_path("maven-compiler-plugin-java");
    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(&root, &options).expect("load maven project");

    assert_eq!(config.build_system, BuildSystem::Maven);
    assert_eq!(config.java.source, JavaVersion(21));
    assert_eq!(config.java.target, JavaVersion(21));
    assert!(config.java.enable_preview);

    let model =
        load_workspace_model_with_options(&root, &options).expect("load maven workspace model");
    let module = model
        .module_by_id("maven:com.example:maven-compiler-plugin-java")
        .expect("root module");

    assert!(module.language_level.level.preview);
    assert_eq!(
        module.language_level.provenance,
        LanguageLevelProvenance::BuildFile(model.workspace_root.join("pom.xml"))
    );
}

#[test]
fn loads_maven_preview_flag_from_properties() {
    let root = testdata_path("maven-java-preview-property");
    let config = load_project(&root).expect("load maven project");

    assert_eq!(config.build_system, BuildSystem::Maven);
    assert_eq!(config.java.source, JavaVersion(21));
    assert_eq!(config.java.target, JavaVersion(21));
    assert!(config.java.enable_preview);

    let repo_dir = tempdir().expect("tempdir");
    let options = LoadOptions {
        maven_repo: Some(repo_dir.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model =
        load_workspace_model_with_options(&root, &options).expect("load maven workspace model");
    let module = model
        .module_by_id("maven:com.example:maven-java-preview-property")
        .expect("root module");

    assert!(module.language_level.level.preview);
    assert_eq!(
        module.language_level.provenance,
        LanguageLevelProvenance::BuildFile(model.workspace_root.join("pom.xml"))
    );
}
