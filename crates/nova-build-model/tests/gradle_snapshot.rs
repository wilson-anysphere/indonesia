use std::collections::BTreeMap;
use std::path::PathBuf;

use nova_build_model::{
    GradleSnapshotFile, GradleSnapshotJavaCompileConfig, GradleSnapshotProject,
    GRADLE_SNAPSHOT_SCHEMA_VERSION,
};

#[test]
fn gradle_snapshot_roundtrip_json() {
    let snapshot = GradleSnapshotFile {
        schema_version: GRADLE_SNAPSHOT_SCHEMA_VERSION,
        build_fingerprint: "fingerprint".to_string(),
        projects: vec![GradleSnapshotProject {
            path: ":app".to_string(),
            project_dir: PathBuf::from("app"),
        }],
        java_compile_configs: BTreeMap::from([(
            ":app".to_string(),
            GradleSnapshotJavaCompileConfig {
                project_dir: PathBuf::from("app"),
                compile_classpath: vec![PathBuf::from("out").join("main")],
                test_classpath: vec![PathBuf::from("out").join("test")],
                module_path: vec![PathBuf::from("mods")],
                main_source_roots: vec![PathBuf::from("src").join("main").join("java")],
                test_source_roots: vec![PathBuf::from("src").join("test").join("java")],
                main_output_dir: Some(PathBuf::from("out").join("main")),
                test_output_dir: Some(PathBuf::from("out").join("test")),
                source: Some("17".to_string()),
                target: Some("17".to_string()),
                release: Some("21".to_string()),
                enable_preview: true,
            },
        )]),
    };

    let json = serde_json::to_string_pretty(&snapshot).expect("serialize snapshot");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json is valid");

    let obj = value.as_object().expect("top-level json object");
    assert_eq!(
        obj.get("schemaVersion"),
        Some(&serde_json::Value::from(GRADLE_SNAPSHOT_SCHEMA_VERSION))
    );
    assert!(obj.contains_key("buildFingerprint"));
    assert!(obj.contains_key("projects"));
    assert!(obj.contains_key("javaCompileConfigs"));
    assert!(!obj.contains_key("schema_version"));

    assert_eq!(value["projects"][0]["projectDir"], "app");
    assert_eq!(
        value["javaCompileConfigs"][":app"]["enablePreview"],
        serde_json::Value::from(true)
    );
    let expected_path = serde_json::to_value(PathBuf::from("out").join("main"))
        .expect("serialize path");
    assert_eq!(
        value["javaCompileConfigs"][":app"]["compileClasspath"][0],
        expected_path
    );

    let decoded: GradleSnapshotFile = serde_json::from_str(&json).expect("deserialize snapshot");
    assert_eq!(decoded, snapshot);
}

#[test]
fn gradle_snapshot_deserialize_missing_fields_defaults() {
    // Older / partial snapshots may be missing optional fields. Ensure we deserialize them using
    // sensible defaults to stay forward-compatible.
    let json = r#"
{
  "schemaVersion": 1,
  "buildFingerprint": "fingerprint",
  "projects": [
    { "path": ":app", "projectDir": "app" }
  ],
  "javaCompileConfigs": {
    ":app": {
      "projectDir": "app",
      "compileClasspath": ["out/main"],
      "testClasspath": ["out/test"],
      "mainSourceRoots": ["src/main/java"],
      "mainOutputDir": "out/main",
      "testOutputDir": "out/test",
      "source": "17",
      "target": "17",
      "release": "21"
    }
  }
}
"#;

    let decoded: GradleSnapshotFile =
        serde_json::from_str(json).expect("deserialize snapshot with missing fields");
    assert_eq!(decoded.schema_version, GRADLE_SNAPSHOT_SCHEMA_VERSION);
    let cfg = decoded
        .java_compile_configs
        .get(":app")
        .expect("config for :app");

    assert!(cfg.module_path.is_empty());
    assert!(cfg.test_source_roots.is_empty());
    assert!(!cfg.enable_preview);
}
