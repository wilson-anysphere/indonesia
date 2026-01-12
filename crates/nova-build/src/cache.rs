use crate::{BuildSystemKind, JavaCompileConfig, Result};
pub use nova_build_model::BuildFileFingerprint;
use nova_build_model::AnnotationProcessing;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use thiserror::Error;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CachedModuleData {
    pub classpath: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub java_compile_config: Option<JavaCompileConfig>,
    pub diagnostics: Option<Vec<CachedDiagnostic>>,
    pub annotation_processing: Option<AnnotationProcessing>,
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
    pub projects: Option<Vec<CachedProjectInfo>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedProjectInfo {
    pub path: String,
    pub dir: PathBuf,
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
        let parent = path.parent().ok_or_else(|| CacheError::Write {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::Other, "path has no parent"),
        })?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        fs::create_dir_all(parent)?;

        let bytes = serde_json::to_vec_pretty(data).map_err(|e| CacheError::Json {
            path: path.clone(),
            source: e,
        })?;
        let (tmp_path, mut file) =
            open_unique_tmp_file(&path, parent).map_err(|source| CacheError::Write {
                path: path.clone(),
                source,
            })?;

        if let Err(source) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&tmp_path);
            return Err(CacheError::Write {
                path: tmp_path.clone(),
                source,
            }
            .into());
        }
        drop(file);

        const MAX_RENAME_ATTEMPTS: usize = 1024;
        let rename_result = (|| -> io::Result<()> {
            let mut attempts = 0usize;
            loop {
                match fs::rename(&tmp_path, &path) {
                    Ok(()) => return Ok(()),
                    Err(err)
                        if cfg!(windows)
                            && (err.kind() == io::ErrorKind::AlreadyExists || path.exists()) =>
                    {
                        // On Windows, `rename` doesn't overwrite. Under concurrent writers,
                        // multiple `remove + rename` sequences can race; retry until we win.
                        match fs::remove_file(&path) {
                            Ok(()) => {}
                            Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                            Err(remove_err) => return Err(remove_err),
                        }

                        attempts += 1;
                        if attempts >= MAX_RENAME_ATTEMPTS {
                            return Err(err);
                        }

                        continue;
                    }
                    Err(err) => return Err(err),
                }
            }
        })();

        if let Err(source) = rename_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(CacheError::Write {
                path: path.clone(),
                source,
            }
            .into());
        }

        #[cfg(unix)]
        {
            let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
        }

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

    pub fn get_module_best_effort(
        &self,
        project_root: &Path,
        kind: BuildSystemKind,
        module_key: &str,
    ) -> Result<Option<CachedModuleData>> {
        let dir = {
            let kind_dir = match kind {
                BuildSystemKind::Maven => "maven",
                BuildSystemKind::Gradle => "gradle",
            };
            self.project_dir(project_root).join(kind_dir)
        };

        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };

        let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((modified, path));
        }

        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, path) in candidates {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let data: CachedBuildData = match serde_json::from_slice(&bytes) {
                Ok(data) => data,
                Err(_) => continue,
            };
            if let Some(module) = data.modules.get(module_key) {
                return Ok(Some(module.clone()));
            }
        }

        Ok(None)
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

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "destination path has no file name"))?;
    let pid = std::process::id();

    loop {
        let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp.{pid}.{counter}"));
        let tmp_path = parent.join(tmp_name);

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}
