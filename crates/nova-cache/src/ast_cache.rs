use crate::error::CacheError;
use crate::fingerprint::Fingerprint;
use crate::util::{atomic_write, now_millis};
use bincode::Options;
use nova_hir::{ItemTree, SymbolSummary};
use nova_syntax::ParseResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const AST_ARTIFACT_SCHEMA_VERSION: u32 = 2;

/// Persisted, per-file syntax + HIR artifacts used to enable near-instant warm
/// starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAstArtifacts {
    /// Lossless syntax tree + parse errors.
    pub parse: ParseResult,
    /// Cheap structural summary for name resolution / indexing.
    pub item_tree: ItemTree,
    /// Optional per-file symbol summary.
    pub symbol_summary: Option<SymbolSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AstCacheMetadata {
    schema_version: u32,
    nova_version: String,
    files: BTreeMap<String, AstCacheFileEntry>,
}

impl AstCacheMetadata {
    fn empty() -> Self {
        Self {
            schema_version: AST_ARTIFACT_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            files: BTreeMap::new(),
        }
    }

    fn is_compatible(&self) -> bool {
        self.schema_version == AST_ARTIFACT_SCHEMA_VERSION && self.nova_version == nova_core::NOVA_VERSION
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AstCacheFileEntry {
    fingerprint: Fingerprint,
    artifact_file: String,
    saved_at_millis: u64,
}

#[derive(Debug, Serialize)]
struct PersistedAstArtifacts<'a> {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    file_path: String,
    file_fingerprint: Fingerprint,
    artifacts: &'a FileAstArtifacts,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedAstArtifactsOwned {
    schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    file_path: String,
    file_fingerprint: Fingerprint,
    artifacts: FileAstArtifacts,
}

/// A disk-backed cache for AST/HIR artifacts stored under a project's `ast/`
/// directory.
///
/// Layout:
/// - `ast/metadata.bin` (bincode, versioned)
/// - `ast/<file-key>.ast` (bincode, versioned)
#[derive(Debug, Clone)]
pub struct AstArtifactCache {
    root: PathBuf,
    metadata_path: PathBuf,
}

impl AstArtifactCache {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let metadata_path = root.join("metadata.bin");
        Self { root, metadata_path }
    }

    pub fn load(
        &self,
        file_path: &str,
        fingerprint: &Fingerprint,
    ) -> Result<Option<FileAstArtifacts>, CacheError> {
        let metadata = self.read_metadata()?;
        if !metadata.is_compatible() {
            return Ok(None);
        }

        let entry = match metadata.files.get(file_path) {
            Some(entry) => entry,
            None => return Ok(None),
        };

        if &entry.fingerprint != fingerprint {
            return Ok(None);
        }

        let artifact_path = self.root.join(&entry.artifact_file);
        if !artifact_path.exists() {
            return Ok(None);
        }

        let bytes = std::fs::read(artifact_path)?;
        let persisted: PersistedAstArtifactsOwned = decode(&bytes)?;

        if persisted.schema_version != AST_ARTIFACT_SCHEMA_VERSION {
            return Ok(None);
        }
        if persisted.nova_version != nova_core::NOVA_VERSION {
            return Ok(None);
        }
        if persisted.file_path != file_path {
            return Ok(None);
        }
        if persisted.file_fingerprint != *fingerprint {
            return Ok(None);
        }

        Ok(Some(persisted.artifacts))
    }

    pub fn store(
        &self,
        file_path: &str,
        fingerprint: &Fingerprint,
        artifacts: &FileAstArtifacts,
    ) -> Result<(), CacheError> {
        std::fs::create_dir_all(&self.root)?;

        let artifact_file = artifact_file_name(file_path);
        let artifact_path = self.root.join(&artifact_file);

        let persisted = PersistedAstArtifacts {
            schema_version: AST_ARTIFACT_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis: now_millis(),
            file_path: file_path.to_string(),
            file_fingerprint: fingerprint.clone(),
            artifacts,
        };

        let bytes = encode(&persisted)?;
        atomic_write(&artifact_path, &bytes)?;

        let mut metadata = self.read_metadata()?;
        if !metadata.is_compatible() {
            metadata = AstCacheMetadata::empty();
        }

        metadata.files.insert(
            file_path.to_string(),
            AstCacheFileEntry {
                fingerprint: fingerprint.clone(),
                artifact_file,
                saved_at_millis: now_millis(),
            },
        );

        let meta_bytes = encode(&metadata)?;
        atomic_write(&self.metadata_path, &meta_bytes)?;
        Ok(())
    }

    fn read_metadata(&self) -> Result<AstCacheMetadata, CacheError> {
        if !self.metadata_path.exists() {
            return Ok(AstCacheMetadata::empty());
        }
        let bytes = std::fs::read(&self.metadata_path)?;
        Ok(decode(&bytes)?)
    }
}

fn artifact_file_name(file_path: &str) -> String {
    let fingerprint = Fingerprint::from_bytes(file_path.as_bytes());
    format!("{}.ast", fingerprint.as_str())
}

fn bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_no_limit()
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CacheError> {
    Ok(bincode_options().serialize(value)?)
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, CacheError> {
    Ok(bincode_options().deserialize(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_hir::item_tree;
    use nova_syntax::parse;
    use tempfile::TempDir;

    #[test]
    fn roundtrip_is_deterministic_and_respects_fingerprints() {
        let tmp = TempDir::new().unwrap();
        let cache = AstArtifactCache::new(tmp.path());

        let text = "class Foo { /* comment */ }";
        let parsed = parse(text);
        let it = item_tree(&parsed, text);
        let artifacts = FileAstArtifacts {
            parse: parsed,
            item_tree: it,
            symbol_summary: None,
        };

        let fp = Fingerprint::from_bytes(text.as_bytes());

        // Deterministic serialization.
        let persisted = PersistedAstArtifacts {
            schema_version: AST_ARTIFACT_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis: 0,
            file_path: "src/Foo.java".to_string(),
            file_fingerprint: fp.clone(),
            artifacts: &artifacts,
        };
        let bytes1 = encode(&persisted).unwrap();
        let bytes2 = encode(&persisted).unwrap();
        assert_eq!(bytes1, bytes2);

        cache.store("src/Foo.java", &fp, &artifacts).unwrap();
        let loaded = cache.load("src/Foo.java", &fp).unwrap().unwrap();
        assert_eq!(loaded, artifacts);

        let fp2 = Fingerprint::from_bytes(b"class Foo { }");
        assert!(cache.load("src/Foo.java", &fp2).unwrap().is_none());
    }
}
