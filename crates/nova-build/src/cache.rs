use crate::{BuildSystemKind, JavaCompileConfig, Result};
use nova_build_model::AnnotationProcessing;
pub use nova_build_model::BuildFileFingerprint;
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
    #[error("failed to parse cache file {path}: {message}")]
    Json {
        path: PathBuf,
        message: String,
    },
}

fn sanitize_json_error_message(message: &str) -> String {
    // `serde_json::Error` display strings can include user-provided scalar values (for example:
    // `invalid type: string "..."` or `unknown field `...``). Cache files can contain build output
    // and other potentially-sensitive values; avoid echoing those values in error messages.
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let mut end = None;
        let bytes = rest.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if b != b'"' {
                continue;
            }

            // Treat quotes preceded by an odd number of backslashes as escaped.
            let mut backslashes = 0usize;
            let mut k = idx;
            while k > 0 && bytes[k - 1] == b'\\' {
                backslashes += 1;
                k -= 1;
            }
            if backslashes % 2 == 0 {
                end = Some(idx);
                break;
            }
        }

        let Some(end) = end else {
            // Unterminated quote: redact the remainder and stop.
            out.push_str("<redacted>");
            rest = "";
            break;
        };
        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Redact only the first backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        if let Some(end_rel) = out[start.saturating_add(1)..].find('`') {
            let end = start.saturating_add(1).saturating_add(end_rel);
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CachedModuleData {
    #[serde(default)]
    pub project_dir: Option<PathBuf>,
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

impl From<&nova_core::BuildDiagnostic> for CachedDiagnostic {
    fn from(value: &nova_core::BuildDiagnostic) -> Self {
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
                nova_core::BuildDiagnosticSeverity::Error => CachedDiagnosticSeverity::Error,
                nova_core::BuildDiagnosticSeverity::Warning => CachedDiagnosticSeverity::Warning,
                nova_core::BuildDiagnosticSeverity::Information => CachedDiagnosticSeverity::Information,
                nova_core::BuildDiagnosticSeverity::Hint => CachedDiagnosticSeverity::Hint,
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
            message: sanitize_json_error_message(&source.to_string()),
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
            source: io::Error::other("path has no parent"),
        })?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        fs::create_dir_all(parent)?;

        let bytes = serde_json::to_vec_pretty(data).map_err(|e| CacheError::Json {
            path: path.clone(),
            message: sanitize_json_error_message(&e.to_string()),
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
        // Best-effort canonicalization to keep cache keys stable when callers mix canonical and
        // non-canonical workspace roots (e.g. macOS `/var` vs `/private/var`, or symlinked roots).
        //
        // This mirrors the canonicalization approach used by `nova-build-model::BuildFileFingerprint`.
        let project_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
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
        .ok_or_else(|| io::Error::other("destination path has no file name"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cache_load_json_errors_do_not_echo_string_values() {
        let dir = tempfile::tempdir().expect("tempdir should succeed");
        let project_root = tempfile::tempdir().expect("tempdir should succeed");
        let cache = BuildCache::new(dir.path());
        let fingerprint = BuildFileFingerprint {
            digest: "deadbeef".to_string(),
        };
        let cache_file = cache.cache_file(project_root.path(), BuildSystemKind::Maven, &fingerprint);
        std::fs::create_dir_all(
            cache_file
                .parent()
                .expect("cache file should have a parent directory"),
        )
        .expect("mkdirs should succeed");

        let secret_suffix = "nova-build-cache-super-secret-token";
        let secret = format!("prefix\"{secret_suffix}");
        // `modules` expects a map. Using a string triggers `invalid type: string "..."`.
        let payload = serde_json::json!({ "modules": secret }).to_string();
        std::fs::write(&cache_file, payload).expect("write cache file should succeed");

        let err = cache
            .load(project_root.path(), BuildSystemKind::Maven, &fingerprint)
            .expect_err("expected load to fail");
        let message = err.to_string();
        assert!(
            !message.contains(secret_suffix),
            "expected build cache JSON error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected build cache JSON error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn build_cache_sanitize_json_error_message_redacts_backticked_values() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            foo: u32,
        }

        let secret = "nova-build-cache-backticked-secret";
        let json = format!(r#"{{"{secret}": 1}}"#);
        let err =
            serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");

        let sanitized = sanitize_json_error_message(&err.to_string());
        assert!(
            !sanitized.contains(secret),
            "expected sanitized serde_json error message to omit backticked values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected sanitized serde_json error message to include redaction marker: {sanitized}"
        );
    }
}
