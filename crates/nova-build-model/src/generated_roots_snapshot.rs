use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::SourceRootKind;

pub const GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GeneratedRootsSnapshotSourceRootKind {
    Main,
    Test,
}

impl From<SourceRootKind> for GeneratedRootsSnapshotSourceRootKind {
    fn from(value: SourceRootKind) -> Self {
        match value {
            SourceRootKind::Main => Self::Main,
            SourceRootKind::Test => Self::Test,
        }
    }
}

impl From<GeneratedRootsSnapshotSourceRootKind> for SourceRootKind {
    fn from(value: GeneratedRootsSnapshotSourceRootKind) -> Self {
        match value {
            GeneratedRootsSnapshotSourceRootKind::Main => Self::Main,
            GeneratedRootsSnapshotSourceRootKind::Test => Self::Test,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedRootsSnapshotRoot {
    pub kind: GeneratedRootsSnapshotSourceRootKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedRootsSnapshotModule {
    pub module_root: PathBuf,
    pub roots: Vec<GeneratedRootsSnapshotRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedRootsSnapshotFile {
    pub schema_version: u32,
    pub modules: Vec<GeneratedRootsSnapshotModule>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
}
