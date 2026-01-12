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
