use crate::error::CacheError;
use crate::fingerprint::Fingerprint;
use crate::path::normalize_rel_path;
use crate::util::{
    atomic_write, bincode_deserialize, bincode_serialize, now_millis, read_file_limited,
    remove_file_best_effort,
};
use crate::CacheLock;
use nova_hir::token_item_tree::{TokenItemTree, TokenSymbolSummary};
use nova_syntax::ParseResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Version of the on-disk AST artifact cache format.
///
/// This gates the *wrapper* structs in this module (metadata and per-file
/// artifact headers). The serialized payload also depends on:
/// - `nova_syntax::SYNTAX_SCHEMA_VERSION` (syntax artifacts)
/// - `nova_hir::HIR_SCHEMA_VERSION` (HIR summaries)
///
/// See `docs/18-cache-schema-versioning.md` for the intended workflow.
pub const AST_ARTIFACT_SCHEMA_VERSION: u32 = 4;

/// Persisted, per-file syntax + HIR artifacts used to enable near-instant warm
/// starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAstArtifacts {
    /// Lossless syntax tree + parse errors.
    pub parse: ParseResult,
    /// Cheap structural summary for name resolution / indexing.
    pub item_tree: TokenItemTree,
    /// Optional per-file symbol summary.
    pub symbol_summary: Option<TokenSymbolSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AstCacheMetadata {
    schema_version: u32,
    syntax_schema_version: u32,
    hir_schema_version: u32,
    nova_version: String,
    files: BTreeMap<String, AstCacheFileEntry>,
}

impl AstCacheMetadata {
    fn empty() -> Self {
        Self {
            schema_version: AST_ARTIFACT_SCHEMA_VERSION,
            syntax_schema_version: nova_syntax::SYNTAX_SCHEMA_VERSION,
            hir_schema_version: nova_hir::HIR_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            files: BTreeMap::new(),
        }
    }

    fn is_compatible(&self) -> bool {
        self.schema_version == AST_ARTIFACT_SCHEMA_VERSION
            && self.syntax_schema_version == nova_syntax::SYNTAX_SCHEMA_VERSION
            && self.hir_schema_version == nova_hir::HIR_SCHEMA_VERSION
            && self.nova_version == nova_core::NOVA_VERSION
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
    syntax_schema_version: u32,
    hir_schema_version: u32,
    nova_version: String,
    saved_at_millis: u64,
    file_path: String,
    file_fingerprint: Fingerprint,
    artifacts: &'a FileAstArtifacts,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedAstArtifactsOwned {
    schema_version: u32,
    syntax_schema_version: u32,
    hir_schema_version: u32,
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
        Self {
            root,
            metadata_path,
        }
    }

    pub fn load(
        &self,
        file_path: &str,
        fingerprint: &Fingerprint,
    ) -> Result<Option<FileAstArtifacts>, CacheError> {
        let file_path = normalize_rel_path(file_path);
        let metadata = self.read_metadata_for_load();
        if !metadata.is_compatible() {
            return Ok(None);
        }

        let entry = match metadata.files.get(&file_path) {
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

        let bytes = match read_file_limited(&artifact_path) {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        let persisted: PersistedAstArtifactsOwned = match bincode_deserialize(&bytes) {
            Ok(persisted) => persisted,
            Err(_) => {
                remove_file_best_effort(&artifact_path, "ast_cache.decode");
                return Ok(None);
            }
        };

        if persisted.schema_version != AST_ARTIFACT_SCHEMA_VERSION {
            remove_file_best_effort(&artifact_path, "ast_cache.schema_version");
            return Ok(None);
        }
        if persisted.syntax_schema_version != nova_syntax::SYNTAX_SCHEMA_VERSION {
            remove_file_best_effort(&artifact_path, "ast_cache.syntax_schema_version");
            return Ok(None);
        }
        if persisted.hir_schema_version != nova_hir::HIR_SCHEMA_VERSION {
            remove_file_best_effort(&artifact_path, "ast_cache.hir_schema_version");
            return Ok(None);
        }
        if persisted.nova_version != nova_core::NOVA_VERSION {
            remove_file_best_effort(&artifact_path, "ast_cache.nova_version");
            return Ok(None);
        }
        if persisted.file_path != file_path {
            remove_file_best_effort(&artifact_path, "ast_cache.file_path");
            return Ok(None);
        }
        if persisted.file_fingerprint != *fingerprint {
            remove_file_best_effort(&artifact_path, "ast_cache.file_fingerprint");
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
        let file_path = normalize_rel_path(file_path);
        std::fs::create_dir_all(&self.root)?;
        let _lock = CacheLock::lock_exclusive(&self.root.join(".lock"))?;

        let artifact_file = artifact_file_name(&file_path);
        let artifact_path = self.root.join(&artifact_file);

        let persisted = PersistedAstArtifacts {
            schema_version: AST_ARTIFACT_SCHEMA_VERSION,
            syntax_schema_version: nova_syntax::SYNTAX_SCHEMA_VERSION,
            hir_schema_version: nova_hir::HIR_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            saved_at_millis: now_millis(),
            file_path: file_path.clone(),
            file_fingerprint: fingerprint.clone(),
            artifacts,
        };

        let bytes = encode(&persisted)?;
        atomic_write(&artifact_path, &bytes)?;

        let mut metadata = self.read_metadata_unlocked();
        if !metadata.is_compatible() {
            metadata = AstCacheMetadata::empty();
        }

        metadata.files.insert(
            file_path.clone(),
            AstCacheFileEntry {
                fingerprint: fingerprint.clone(),
                artifact_file,
                saved_at_millis: now_millis(),
            },
        );

        let meta_bytes = bincode_serialize(&metadata)?;
        atomic_write(&self.metadata_path, &meta_bytes)?;
        Ok(())
    }

    fn read_metadata_for_load(&self) -> AstCacheMetadata {
        if !self.metadata_path.exists() {
            return AstCacheMetadata::empty();
        }

        self.read_metadata_unlocked()
    }

    fn read_metadata_unlocked(&self) -> AstCacheMetadata {
        if !self.metadata_path.exists() {
            return AstCacheMetadata::empty();
        }

        let bytes = match read_file_limited(&self.metadata_path) {
            Some(bytes) => bytes,
            None => return AstCacheMetadata::empty(),
        };

        match bincode_deserialize::<AstCacheMetadata>(&bytes) {
            Ok(metadata) => metadata,
            Err(_) => AstCacheMetadata::empty(),
        }
    }
}

fn artifact_file_name(file_path: &str) -> String {
    let fingerprint = Fingerprint::from_bytes(file_path.as_bytes());
    format!("{}.ast", fingerprint.as_str())
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CacheError> {
    bincode_serialize(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_hir::token_item_tree::token_item_tree;
    use nova_syntax::parse;
    use tempfile::TempDir;

    #[test]
    fn roundtrip_is_deterministic_and_respects_fingerprints() {
        let tmp = TempDir::new().unwrap();
        let cache = AstArtifactCache::new(tmp.path());

        let text = "class Foo { /* comment */ }";
        let parsed = parse(text);
        let it = token_item_tree(&parsed, text);
        let artifacts = FileAstArtifacts {
            parse: parsed,
            item_tree: it,
            symbol_summary: None,
        };

        let fp = Fingerprint::from_bytes(text.as_bytes());

        // Deterministic serialization.
        let persisted = PersistedAstArtifacts {
            schema_version: AST_ARTIFACT_SCHEMA_VERSION,
            syntax_schema_version: nova_syntax::SYNTAX_SCHEMA_VERSION,
            hir_schema_version: nova_hir::HIR_SCHEMA_VERSION,
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

    #[test]
    fn syntax_schema_mismatch_is_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = AstArtifactCache::new(tmp.path());

        let text = "class Foo {}";
        let parsed = parse(text);
        let it = token_item_tree(&parsed, text);
        let artifacts = FileAstArtifacts {
            parse: parsed,
            item_tree: it,
            symbol_summary: None,
        };
        let fp = Fingerprint::from_bytes(text.as_bytes());

        cache.store("src/Foo.java", &fp, &artifacts).unwrap();

        let artifact_name = artifact_file_name("src/Foo.java");
        let artifact_path = tmp.path().join(&artifact_name);
        let bytes = std::fs::read(&artifact_path).unwrap();
        let mut persisted: PersistedAstArtifactsOwned = bincode_deserialize(&bytes).unwrap();
        persisted.syntax_schema_version = nova_syntax::SYNTAX_SCHEMA_VERSION + 1;
        let bytes = encode(&persisted).unwrap();
        std::fs::write(artifact_path, bytes).unwrap();

        assert!(cache.load("src/Foo.java", &fp).unwrap().is_none());
    }

    #[test]
    fn corrupt_metadata_is_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = AstArtifactCache::new(tmp.path());

        let text = "class Foo {}";
        let parsed = parse(text);
        let it = token_item_tree(&parsed, text);
        let artifacts = FileAstArtifacts {
            parse: parsed,
            item_tree: it,
            symbol_summary: None,
        };
        let fp = Fingerprint::from_bytes(text.as_bytes());

        cache.store("src/Foo.java", &fp, &artifacts).unwrap();

        std::fs::write(tmp.path().join("metadata.bin"), b"not bincode").unwrap();

        assert!(cache.load("src/Foo.java", &fp).unwrap().is_none());
    }

    #[test]
    fn corrupt_artifact_is_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = AstArtifactCache::new(tmp.path());

        let text = "class Foo {}";
        let parsed = parse(text);
        let it = token_item_tree(&parsed, text);
        let artifacts = FileAstArtifacts {
            parse: parsed,
            item_tree: it,
            symbol_summary: None,
        };
        let fp = Fingerprint::from_bytes(text.as_bytes());

        cache.store("src/Foo.java", &fp, &artifacts).unwrap();

        let artifact_name = artifact_file_name("src/Foo.java");
        std::fs::write(tmp.path().join(artifact_name), b"broken").unwrap();

        assert!(cache.load("src/Foo.java", &fp).unwrap().is_none());
    }

    #[test]
    fn cache_keys_normalize_path_separators() {
        let tmp = TempDir::new().unwrap();
        let cache = AstArtifactCache::new(tmp.path());

        let text = "class A {}";
        let parsed = parse(text);
        let it = token_item_tree(&parsed, text);
        let artifacts = FileAstArtifacts {
            parse: parsed,
            item_tree: it,
            symbol_summary: None,
        };
        let fp = Fingerprint::from_bytes(text.as_bytes());

        cache.store("src\\A.java", &fp, &artifacts).unwrap();
        let loaded = cache.load("src/A.java", &fp).unwrap().unwrap();
        assert_eq!(loaded, artifacts);
    }
}
