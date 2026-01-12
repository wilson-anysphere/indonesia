use anyhow::Context;

use nova_project::{load_project_with_options, BuildSystem, Dependency, LoadOptions};

#[test]
fn gradle_extracts_versionless_dependencies() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().context("tempdir")?;

    // Intentionally mix Kotlin/Groovy call styles and both string + map notation.
    std::fs::write(
        dir.path().join("build.gradle"),
        r#"
plugins {
  id 'java'
}

dependencies {
  // Versionless string notation (common with BOMs, e.g. Spring Boot).
  implementation("org.springframework.boot:spring-boot-starter-web")

  // Versioned string notation.
  testImplementation "org.junit.jupiter:junit-jupiter:5.10.0"

  // Versionless map notation.
  implementation group: 'com.example', name: 'my-lib'

  // Duplicates (should be deduped deterministically).
  implementation "org.springframework.boot:spring-boot-starter-web"
  implementation group: 'com.example', name: 'my-lib'
}
"#,
    )
    .context("write build.gradle")?;

    let gradle_home = tempfile::tempdir().context("tempdir (gradle home)")?;
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(dir.path(), &options).context("load_project")?;
    assert_eq!(config.build_system, BuildSystem::Gradle);

    assert_eq!(
        config.dependencies,
        vec![
            Dependency {
                group_id: "com.example".to_string(),
                artifact_id: "my-lib".to_string(),
                version: None,
                scope: Some("compile".to_string()),
                classifier: None,
                type_: None,
            },
            Dependency {
                group_id: "org.junit.jupiter".to_string(),
                artifact_id: "junit-jupiter".to_string(),
                version: Some("5.10.0".to_string()),
                scope: Some("test".to_string()),
                classifier: None,
                type_: None,
            },
            Dependency {
                group_id: "org.springframework.boot".to_string(),
                artifact_id: "spring-boot-starter-web".to_string(),
                version: None,
                scope: Some("compile".to_string()),
                classifier: None,
                type_: None,
            },
        ]
    );

    Ok(())
}

#[test]
fn gradle_interpolates_versions_from_gradle_properties() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().context("tempdir")?;

    std::fs::write(
        dir.path().join("gradle.properties"),
        "guavaVersion=33.0.0-jre\n",
    )
    .context("write gradle.properties")?;

    std::fs::write(
        dir.path().join("build.gradle"),
        r#"
plugins {
  id 'java'
}

dependencies {
  implementation("com.google.guava:guava:$guavaVersion")
}
"#,
    )
    .context("write build.gradle")?;

    let gradle_home = tempfile::tempdir().context("tempdir (gradle home)")?;
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(dir.path(), &options).context("load_project")?;
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(
        config.dependencies,
        vec![Dependency {
            group_id: "com.google.guava".to_string(),
            artifact_id: "guava".to_string(),
            version: Some("33.0.0-jre".to_string()),
            scope: Some("compile".to_string()),
            classifier: None,
            type_: None,
        }]
    );

    Ok(())
}

#[test]
fn gradle_interpolates_version_catalog_versions_from_gradle_properties() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().context("tempdir")?;

    std::fs::write(
        dir.path().join("gradle.properties"),
        "guavaVersion=33.0.0-jre\n",
    )
    .context("write gradle.properties")?;

    std::fs::create_dir_all(dir.path().join("gradle")).context("mkdir gradle/")?;
    std::fs::write(
        dir.path().join("gradle/libs.versions.toml"),
        r#"
[versions]
guava = "$guavaVersion"

[libraries]
guava = { module = "com.google.guava:guava", version = { ref = "guava" } }
"#,
    )
    .context("write libs.versions.toml")?;

    std::fs::write(
        dir.path().join("build.gradle"),
        r#"
plugins {
  id 'java'
}

dependencies {
  implementation(libs.guava)
}
"#,
    )
    .context("write build.gradle")?;

    let gradle_home = tempfile::tempdir().context("tempdir (gradle home)")?;
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(dir.path(), &options).context("load_project")?;
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(
        config.dependencies,
        vec![Dependency {
            group_id: "com.google.guava".to_string(),
            artifact_id: "guava".to_string(),
            version: Some("33.0.0-jre".to_string()),
            scope: Some("compile".to_string()),
            classifier: None,
            type_: None,
        }]
    );

    Ok(())
}

#[test]
fn gradle_unresolved_property_interpolation_drops_version() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().context("tempdir")?;

    std::fs::write(
        dir.path().join("build.gradle"),
        r#"
plugins {
  id 'java'
}

dependencies {
  implementation("com.google.guava:guava:$guavaVersion")
}
"#,
    )
    .context("write build.gradle")?;

    let gradle_home = tempfile::tempdir().context("tempdir (gradle home)")?;
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(dir.path(), &options).context("load_project")?;
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(
        config.dependencies,
        vec![Dependency {
            group_id: "com.google.guava".to_string(),
            artifact_id: "guava".to_string(),
            version: None,
            scope: Some("compile".to_string()),
            classifier: None,
            type_: None,
        }]
    );

    Ok(())
}

#[test]
fn gradle_interpolates_versions_from_module_gradle_properties() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().context("tempdir")?;

    std::fs::write(dir.path().join("settings.gradle"), "include ':app'\n")
        .context("write settings.gradle")?;

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

dependencies {
  implementation("com.google.guava:guava:$guavaVersion")
}
"#,
    )
    .context("write app/build.gradle")?;

    let gradle_home = tempfile::tempdir().context("tempdir (gradle home)")?;
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let config = load_project_with_options(dir.path(), &options).context("load_project")?;
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(
        config.dependencies,
        vec![Dependency {
            group_id: "com.google.guava".to_string(),
            artifact_id: "guava".to_string(),
            version: Some("33.0.0-jre".to_string()),
            scope: Some("compile".to_string()),
            classifier: None,
            type_: None,
        }]
    );

    Ok(())
}
