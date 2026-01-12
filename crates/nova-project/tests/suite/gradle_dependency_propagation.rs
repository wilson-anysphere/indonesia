use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Context;
use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    LoadOptions,
};
use tempfile::tempdir;

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

#[test]
fn gradle_workspace_model_propagates_project_dependencies_transitively_into_classpath() {
    let root = testdata_path("gradle-project-deps-transitive");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    let app = model.module_by_id("gradle::app").expect("app module");
    let dirs: BTreeSet<_> = app
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
        dirs.contains(&PathBuf::from("lib/build/classes/java/main")),
        "expected app classpath to contain lib/build/classes/java/main"
    );
    assert!(
        dirs.contains(&PathBuf::from("core/build/classes/java/main")),
        "expected app classpath to contain core/build/classes/java/main"
    );
}

#[test]
fn gradle_root_subprojects_dependencies_propagate_into_modules() {
    let root = testdata_path("gradle-root-subprojects-deps");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert!(
        config.dependencies.iter().any(|d| {
            d.group_id == "com.google.guava"
                && d.artifact_id == "guava"
                && d.version.as_deref() == Some("33.0.0-jre")
        }),
        "expected project dependency list to include guava from root subprojects block"
    );

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    for id in ["gradle::app", "gradle::lib"] {
        let module = model.module_by_id(id).expect("module");
        assert!(
            module.dependencies.iter().any(|d| {
                d.group_id == "com.google.guava"
                    && d.artifact_id == "guava"
                    && d.version.as_deref() == Some("33.0.0-jre")
            }),
            "expected {id} to include guava from root subprojects block"
        );
    }
}

#[test]
fn gradle_subprojects_dependencies_do_not_apply_to_root_module() {
    let root = testdata_path("gradle-root-subprojects-deps-with-root-module");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    let root_module = model.module_by_id("gradle::").expect("root module");
    assert!(
        !root_module.dependencies.iter().any(|d| {
            d.group_id == "com.google.guava"
                && d.artifact_id == "guava"
                && d.version.as_deref() == Some("33.0.0-jre")
        }),
        "expected root module dependencies to omit guava from root subprojects block"
    );

    for id in ["gradle::app", "gradle::lib"] {
        let module = model.module_by_id(id).expect("module");
        assert!(
            module.dependencies.iter().any(|d| {
                d.group_id == "com.google.guava"
                    && d.artifact_id == "guava"
                    && d.version.as_deref() == Some("33.0.0-jre")
            }),
            "expected {id} to include guava from root subprojects block"
        );
    }
}

#[test]
fn gradle_allprojects_dependencies_apply_to_root_module() {
    let root = testdata_path("gradle-root-allprojects-deps-with-root-module");
    let gradle_home = tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model =
        load_workspace_model_with_options(&root, &options).expect("load gradle workspace model");

    for id in ["gradle::", "gradle::app", "gradle::lib"] {
        let module = model.module_by_id(id).expect("module");
        assert!(
            module.dependencies.iter().any(|d| {
                d.group_id == "com.google.guava"
                    && d.artifact_id == "guava"
                    && d.version.as_deref() == Some("33.0.0-jre")
            }),
            "expected {id} to include guava from root allprojects block"
        );
    }
}

#[test]
fn gradle_root_subprojects_dependencies_interpolate_from_module_gradle_properties(
) -> anyhow::Result<()> {
    let dir = tempdir().context("tempdir")?;

    std::fs::write(dir.path().join("settings.gradle"), "include ':app'\n")
        .context("write settings.gradle")?;
    std::fs::write(
        dir.path().join("build.gradle"),
        r#"
subprojects {
  dependencies {
    implementation "com.google.guava:guava:$guavaVersion"
  }
}
"#,
    )
    .context("write build.gradle")?;

    std::fs::create_dir_all(dir.path().join("app")).context("mkdir app/")?;
    std::fs::write(
        dir.path().join("app/gradle.properties"),
        "guavaVersion=33.0.0-jre\n",
    )
    .context("write app/gradle.properties")?;
    std::fs::write(
        dir.path().join("app/build.gradle"),
        r#"
plugins {
  id 'java'
}
"#,
    )
    .context("write app/build.gradle")?;

    let gradle_home = tempdir().context("tempdir (gradle home)")?;
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let model = load_workspace_model_with_options(dir.path(), &options)
        .context("load gradle workspace model")?;
    assert_eq!(model.build_system, BuildSystem::Gradle);

    let app = model.module_by_id("gradle::app").context("app module")?;
    assert!(
        app.dependencies.iter().any(|d| {
            d.group_id == "com.google.guava"
                && d.artifact_id == "guava"
                && d.version.as_deref() == Some("33.0.0-jre")
        }),
        "expected gradle::app to include guava with interpolated version from app/gradle.properties"
    );

    Ok(())
}

#[test]
fn gradle_project_dependencies_interpolate_from_module_gradle_properties_in_root_subprojects_block(
) -> anyhow::Result<()> {
    let dir = tempdir().context("tempdir")?;

    std::fs::write(dir.path().join("settings.gradle"), "include ':app'\n")
        .context("write settings.gradle")?;
    std::fs::write(
        dir.path().join("build.gradle"),
        r#"
subprojects {
  dependencies {
    implementation "com.google.guava:guava:$guavaVersion"
  }
}
"#,
    )
    .context("write build.gradle")?;

    std::fs::create_dir_all(dir.path().join("app")).context("mkdir app/")?;
    std::fs::write(
        dir.path().join("app/gradle.properties"),
        "guavaVersion=33.0.0-jre\n",
    )
    .context("write app/gradle.properties")?;
    std::fs::write(
        dir.path().join("app/build.gradle"),
        r#"
plugins {
  id 'java'
}
"#,
    )
    .context("write app/build.gradle")?;

    let gradle_home = tempdir().context("tempdir (gradle home)")?;
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(dir.path(), &options).context("load gradle project")?;
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert!(
        config.dependencies.iter().any(|d| {
            d.group_id == "com.google.guava"
                && d.artifact_id == "guava"
                && d.version.as_deref() == Some("33.0.0-jre")
        }),
        "expected project dependency list to include guava with interpolated version from app/gradle.properties"
    );

    Ok(())
}
