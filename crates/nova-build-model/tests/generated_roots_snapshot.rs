use std::path::PathBuf;

use nova_build_model::{
    GeneratedRootsSnapshotFile, GeneratedRootsSnapshotModule, GeneratedRootsSnapshotRoot,
    SourceRootKind, GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
};

#[test]
fn generated_roots_snapshot_roundtrip_json() {
    let snapshot = GeneratedRootsSnapshotFile {
        schema_version: GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
        modules: vec![GeneratedRootsSnapshotModule {
            module_root: PathBuf::from("/workspace/module"),
            roots: vec![
                GeneratedRootsSnapshotRoot {
                    kind: SourceRootKind::Main.into(),
                    path: PathBuf::from("target/generated-sources/annotations"),
                },
                GeneratedRootsSnapshotRoot {
                    kind: SourceRootKind::Test.into(),
                    path: PathBuf::from("target/generated-test-sources/test-annotations"),
                },
            ],
        }],
    };

    let json = serde_json::to_string_pretty(&snapshot).expect("serialize snapshot");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json is valid");

    assert_eq!(
        value["schema_version"],
        serde_json::Value::from(GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION)
    );
    assert_eq!(value["modules"][0]["roots"][0]["kind"], "main");
    assert_eq!(value["modules"][0]["roots"][1]["kind"], "test");

    let decoded: GeneratedRootsSnapshotFile =
        serde_json::from_str(&json).expect("deserialize snapshot");
    assert_eq!(decoded, snapshot);
}
