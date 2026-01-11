use crate::{BuildSystemKind, JavaCompileConfig, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("failed to read cache file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write cache file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse cache file {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildFileFingerprint {
    pub digest: String,
}

impl BuildFileFingerprint {
    pub fn from_files(project_root: &Path, mut files: Vec<PathBuf>) -> Result<Self> {
        files.sort();
        files.dedup();

        let mut hasher = Sha256::new();
        for path in files {
            let rel = path.strip_prefix(project_root).unwrap_or(&path);
            hasher.update(rel.to_string_lossy().as_bytes());
            hasher.update([0]);

            let bytes = fs::read(&path)?;
            hasher.update(&bytes);
            hasher.update([0]);
        }

        Ok(Self {
            digest: hex::encode(hasher.finalize()),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CachedModuleData {
    pub classpath: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub java_compile_config: Option<JavaCompileConfig>,
    pub diagnostics: Option<Vec<CachedDiagnostic>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedRange {
    pub start: CachedPosition,
    pub end: CachedPosition,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CachedDiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedDiagnostic {
    pub file: PathBuf,
    pub range: CachedRange,
    pub severity: CachedDiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
}

impl From<&nova_core::Diagnostic> for CachedDiagnostic {
    fn from(value: &nova_core::Diagnostic) -> Self {
        Self {
            file: value.file.clone(),
            range: CachedRange {
                start: CachedPosition {
                    line: value.range.start.line,
                    character: value.range.start.character,
                },
                end: CachedPosition {
                    line: value.range.end.line,
                    character: value.range.end.character,
                },
            },
            severity: match value.severity {
                nova_core::DiagnosticSeverity::Error => CachedDiagnosticSeverity::Error,
                nova_core::DiagnosticSeverity::Warning => CachedDiagnosticSeverity::Warning,
                nova_core::DiagnosticSeverity::Information => CachedDiagnosticSeverity::Information,
                nova_core::DiagnosticSeverity::Hint => CachedDiagnosticSeverity::Hint,
            },
            message: value.message.clone(),
            source: value.source.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CachedBuildData {
    pub modules: BTreeMap<String, CachedModuleData>,
}

#[derive(Debug, Clone)]
pub struct BuildCache {
    base_dir: PathBuf,
}

impl BuildCache {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    pub fn invalidate_project(&self, project_root: &Path) -> Result<()> {
        let dir = self.project_dir(project_root);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    pub fn load(
        &self,
        project_root: &Path,
        kind: BuildSystemKind,
        fingerprint: &BuildFileFingerprint,
    ) -> Result<Option<CachedBuildData>> {
        let path = self.cache_file(project_root, kind, fingerprint);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|source| CacheError::Read {
            path: path.clone(),
            source,
        })?;
        let data = serde_json::from_slice(&bytes).map_err(|source| CacheError::Json {
            path: path.clone(),
            source,
        })?;
        Ok(Some(data))
    }

    pub fn store(
        &self,
        project_root: &Path,
        kind: BuildSystemKind,
        fingerprint: &BuildFileFingerprint,
        data: &CachedBuildData,
    ) -> Result<()> {
        let path = self.cache_file(project_root, kind, fingerprint);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(data).map_err(|e| CacheError::Json {
            path: path.clone(),
            source: e,
        })?;
        fs::write(&tmp_path, bytes).map_err(|source| CacheError::Write {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &path).map_err(|source| CacheError::Write {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }

    pub fn get_module(
        &self,
        project_root: &Path,
        kind: BuildSystemKind,
        fingerprint: &BuildFileFingerprint,
        module_key: &str,
    ) -> Result<Option<CachedModuleData>> {
        let Some(data) = self.load(project_root, kind, fingerprint)? else {
            return Ok(None);
        };
        Ok(data.modules.get(module_key).cloned())
    }

    pub fn update_module(
        &self,
        project_root: &Path,
        kind: BuildSystemKind,
        fingerprint: &BuildFileFingerprint,
        module_key: &str,
        update: impl FnOnce(&mut CachedModuleData),
    ) -> Result<()> {
        let mut data = self
            .load(project_root, kind, fingerprint)?
            .unwrap_or_default();
        let entry = data.modules.entry(module_key.to_string()).or_default();
        update(entry);
        self.store(project_root, kind, fingerprint, &data)?;
        Ok(())
    }

    fn project_dir(&self, project_root: &Path) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(project_root.to_string_lossy().as_bytes());
        let digest = hex::encode(hasher.finalize());
        self.base_dir.join(digest)
    }

    fn cache_file(
        &self,
        project_root: &Path,
        kind: BuildSystemKind,
        fingerprint: &BuildFileFingerprint,
    ) -> PathBuf {
        let kind_dir = match kind {
            BuildSystemKind::Maven => "maven",
            BuildSystemKind::Gradle => "gradle",
        };
        self.project_dir(project_root)
            .join(kind_dir)
            .join(format!("{}.json", fingerprint.digest))
    }
}
