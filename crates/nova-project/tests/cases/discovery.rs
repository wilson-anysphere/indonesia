use std::collections::BTreeSet;
use std::path::PathBuf;

use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    JavaVersion, LanguageLevelProvenance, LoadOptions, OutputDirKind, SourceRootKind,
    SourceRootOrigin,
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

    // Classpath should include dependency jar placeholders.
    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.clone())
        .collect::<Vec<_>>();
    assert!(jar_entries.iter().any(|p| {
        p.to_string_lossy()
            .replace('\\', "/")
            .contains("com/google/guava/guava/33.0.0-jre")
    }));

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
    let config2 =
        load_project_with_options(&root, &options).expect("load maven project again");
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
    let config2 =
        load_project_with_options(&root, &options).expect("load maven project again");
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

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();

    let jar_path = jar_entries
        .iter()
        .find(|p| p.contains("com/example/managed-dep"))
        .expect("expected managed-dep to have a synthesized jar path");
    assert!(jar_path.contains("/1.2.3/"), "jar path: {jar_path}");
    assert!(!jar_path.contains("${"), "jar path: {jar_path}");
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

    let jar_entries = config
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Jar)
        .map(|cp| cp.path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();

    let jar_path = jar_entries
        .iter()
        .find(|p| p.contains("com/example/managed-dep"))
        .expect("expected managed-dep to have a synthesized jar path");
    assert!(jar_path.contains("/2.0.0/"), "jar path: {jar_path}");
    assert!(!jar_path.contains("${"), "jar path: {jar_path}");
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

    let config2 =
        load_project_with_options(&root, &options).expect("load gradle project again");
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

    let config2 =
        load_project_with_options(&root, &options).expect("load gradle project again");
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
    let config = load_project(&root).expect("load gradle project");

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

    // Non-JPMS Maven workspace model: dependency jars should remain on the classpath and
    // `module_path` should stay empty.
    assert!(
        app.module_path.is_empty(),
        "expected module_path to remain empty for non-JPMS workspaces"
    );
    assert!(app.classpath.iter().any(|cp| {
        cp.kind == ClasspathEntryKind::Jar
            && cp
                .path
                .to_string_lossy()
                .replace('\\', "/")
                .contains("com/google/guava/guava/33.0.0-jre")
    }));

    // Ensure model is deterministic.
    let model2 =
        load_workspace_model_with_options(&root, &options).expect("load maven workspace model again");
    assert_eq!(model.modules, model2.modules);
    assert_eq!(model.jpms_modules, model2.jpms_modules);
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
fn loads_gradle_projectdir_mapping_workspace_model() {
    let root = testdata_path("gradle-projectdir-mapping");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model = load_workspace_model_with_options(&root, &options)
        .expect("load gradle workspace model");

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
    let model = load_workspace_model_with_options(&root, &options)
        .expect("load gradle workspace model");

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
    let model = load_workspace_model(&root).expect("load gradle workspace model");

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
