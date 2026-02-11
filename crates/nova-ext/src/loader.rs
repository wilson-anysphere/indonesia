use crate::manifest::{
    ExtensionCapability, ExtensionManifest, MANIFEST_FILE_NAME, SUPPORTED_ABI_VERSION,
};
#[cfg(feature = "wasm-extensions")]
use crate::{ExtensionRegistry, RegisterError};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct LoadedExtension {
    dir: PathBuf,
    manifest: ExtensionManifest,
    entry_path: PathBuf,
    entry_bytes: Vec<u8>,
}

impl LoadedExtension {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    pub fn entry_path(&self) -> &Path {
        &self.entry_path
    }

    pub fn metadata(&self) -> ExtensionMetadata {
        ExtensionMetadata {
            dir: self.dir.clone(),
            id: self.manifest.id.clone(),
            version: self.manifest.version.clone(),
            name: self.manifest.name.clone(),
            description: self.manifest.description.clone(),
            authors: self.manifest.authors.clone(),
            homepage: self.manifest.homepage.clone(),
            license: self.manifest.license.clone(),
            abi_version: self.manifest.abi_version,
            capabilities: self.manifest.capabilities.clone(),
        }
    }

    pub fn entry_bytes(&self) -> &[u8] {
        &self.entry_bytes
    }
}

#[derive(Debug, Clone)]
pub struct ExtensionMetadata {
    pub dir: PathBuf,
    pub id: String,
    pub version: semver::Version,
    pub name: Option<String>,
    pub description: Option<String>,
    pub authors: Vec<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub abi_version: u32,
    pub capabilities: Vec<ExtensionCapability>,
}

#[cfg(feature = "wasm-extensions")]
#[derive(Debug, Clone, Default)]
pub struct RegisterReport {
    pub registered: Vec<ExtensionMetadata>,
    pub errors: Vec<RegisterFailure>,
}

#[cfg(feature = "wasm-extensions")]
#[derive(Debug, Clone)]
pub struct RegisterFailure {
    pub extension: ExtensionMetadata,
    pub error: RegisterError,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("extension directory does not exist: {dir:?}")]
    MissingDirectory { dir: PathBuf },
    #[error("extension path is not a directory: {dir:?}")]
    NotADirectory { dir: PathBuf },
    #[error("failed to read extension manifest {manifest_path:?}: {source}")]
    ManifestRead {
        manifest_path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse extension manifest {manifest_path:?}: {message}")]
    ManifestParse {
        manifest_path: PathBuf,
        /// Snippet-free, sanitized error message produced by `toml::de::Error::message()`.
        ///
        /// `toml::de::Error`'s `Display` output can include the offending source snippet, which may
        /// contain secrets (for example, credentials embedded in URLs). Keep only the message so
        /// extension load failures are safe to log and include in bug report bundles.
        message: String,
    },
    #[error("extension {id:?} at {dir:?}: unsupported abi_version {abi_version} (supported: {supported})")]
    UnsupportedAbiVersion {
        dir: PathBuf,
        id: String,
        abi_version: u32,
        supported: u32,
    },
    #[error("extension {id:?} at {dir:?}: entry must be a relative path, got {entry:?}")]
    EntryNotRelative {
        dir: PathBuf,
        id: String,
        entry: PathBuf,
    },
    #[error("extension {id:?} at {dir:?}: entry path escapes extension directory: {entry:?}")]
    EntryEscapesDirectory {
        dir: PathBuf,
        id: String,
        entry: PathBuf,
    },
    #[error("extension {id:?} at {dir:?}: missing entry file {entry_path:?}")]
    MissingEntry {
        dir: PathBuf,
        id: String,
        entry_path: PathBuf,
    },
    #[error("extension {id:?} at {dir:?}: failed to read entry file {entry_path:?}: {source}")]
    EntryRead {
        dir: PathBuf,
        id: String,
        entry_path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("duplicate extension id {id:?} found in {dirs:?}")]
    DuplicateId { id: String, dirs: Vec<PathBuf> },
    #[error("extension {id:?} at {dir:?}: denied by configuration")]
    DeniedByConfig { dir: PathBuf, id: String },
    #[error("extension {id:?} at {dir:?}: not allowed by configuration")]
    NotAllowedByConfig { dir: PathBuf, id: String },
    #[error("search path does not exist: {path:?}")]
    SearchPathMissing { path: PathBuf },
    #[error("search path is not a directory: {path:?}")]
    SearchPathNotDirectory { path: PathBuf },
    #[error("failed to read search path directory {path:?}: {source}")]
    SearchPathReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to enumerate search path directory {path:?}: {source}")]
    SearchPathEnumerate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub struct ExtensionManager;

impl ExtensionManager {
    pub fn load_from_dir(dir: &Path) -> Result<LoadedExtension, LoadError> {
        if !dir.exists() {
            return Err(LoadError::MissingDirectory {
                dir: dir.to_path_buf(),
            });
        }
        if !dir.is_dir() {
            return Err(LoadError::NotADirectory {
                dir: dir.to_path_buf(),
            });
        }

        let manifest_path = dir.join(MANIFEST_FILE_NAME);
        let manifest_text =
            fs::read_to_string(&manifest_path).map_err(|source| LoadError::ManifestRead {
                manifest_path: manifest_path.clone(),
                source,
            })?;

        let manifest = toml::from_str::<ExtensionManifest>(&manifest_text).map_err(|err| {
            LoadError::ManifestParse {
                manifest_path: manifest_path.clone(),
                message: sanitize_toml_error_message(err.message()),
            }
        })?;

        if manifest.abi_version != SUPPORTED_ABI_VERSION {
            return Err(LoadError::UnsupportedAbiVersion {
                dir: dir.to_path_buf(),
                id: manifest.id.clone(),
                abi_version: manifest.abi_version,
                supported: SUPPORTED_ABI_VERSION,
            });
        }

        if !manifest.entry.is_relative() {
            return Err(LoadError::EntryNotRelative {
                dir: dir.to_path_buf(),
                id: manifest.id.clone(),
                entry: manifest.entry.clone(),
            });
        }

        let entry_path = resolve_entry_path(dir, &manifest)?;
        if !entry_path.is_file() {
            return Err(LoadError::MissingEntry {
                dir: dir.to_path_buf(),
                id: manifest.id.clone(),
                entry_path,
            });
        }

        let entry_bytes = fs::read(&entry_path).map_err(|source| LoadError::EntryRead {
            dir: dir.to_path_buf(),
            id: manifest.id.clone(),
            entry_path: entry_path.clone(),
            source,
        })?;

        tracing::debug!(
            extension_id = %manifest.id,
            extension_dir = ?dir,
            "loaded extension manifest"
        );

        Ok(LoadedExtension {
            dir: dir.to_path_buf(),
            manifest,
            entry_path,
            entry_bytes,
        })
    }

    pub fn load_all(search_paths: &[PathBuf]) -> (Vec<LoadedExtension>, Vec<LoadError>) {
        let mut errors = Vec::new();
        let mut extension_dirs = BTreeSet::<PathBuf>::new();

        for search_path in search_paths {
            match discover_extension_dirs(search_path) {
                Ok(dirs) => extension_dirs.extend(dirs),
                Err(err) => errors.push(err),
            }
        }

        let mut loaded = Vec::new();
        for dir in extension_dirs {
            match Self::load_from_dir(&dir) {
                Ok(ext) => loaded.push(ext),
                Err(err) => errors.push(err),
            }
        }

        let duplicates = find_duplicate_extension_ids(&loaded);
        if !duplicates.is_empty() {
            let dup_ids: BTreeSet<String> = duplicates.keys().cloned().collect();
            errors.extend(
                duplicates
                    .into_iter()
                    .map(|(id, dirs)| LoadError::DuplicateId { id, dirs }),
            );
            loaded.retain(|ext| !dup_ids.contains(ext.id()));
        }

        loaded.sort_by(|a, b| a.id().cmp(b.id()));

        (loaded, errors)
    }

    /// Load extensions from the given search paths and apply allow/deny filters.
    ///
    /// This is a convenience wrapper around [`Self::load_all`] and [`Self::filter_by_id`].
    pub fn load_all_filtered(
        search_paths: &[PathBuf],
        allow: Option<&[String]>,
        deny: &[String],
    ) -> (Vec<LoadedExtension>, Vec<LoadError>) {
        let (loaded, mut errors) = Self::load_all(search_paths);
        let (loaded, filter_errors) = Self::filter_by_id(loaded, allow, deny);
        errors.extend(filter_errors);
        (loaded, errors)
    }

    pub fn filter_by_id(
        loaded: Vec<LoadedExtension>,
        allow: Option<&[String]>,
        deny: &[String],
    ) -> (Vec<LoadedExtension>, Vec<LoadError>) {
        let mut kept = Vec::new();
        let mut errors = Vec::new();

        for ext in loaded {
            let id = ext.id().trim();

            if let Some(_pattern) = deny.iter().find(|pattern| id_matches_pattern(id, pattern)) {
                errors.push(LoadError::DeniedByConfig {
                    dir: ext.dir.clone(),
                    id: id.to_string(),
                });
                continue;
            }

            if let Some(allow) = allow {
                if !allow.iter().any(|pattern| id_matches_pattern(id, pattern)) {
                    errors.push(LoadError::NotAllowedByConfig {
                        dir: ext.dir.clone(),
                        id: id.to_string(),
                    });
                    continue;
                }
            }

            kept.push(ext);
        }

        (kept, errors)
    }

    pub fn list(loaded: &[LoadedExtension]) -> Vec<ExtensionMetadata> {
        let mut out: Vec<_> = loaded.iter().map(LoadedExtension::metadata).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    #[cfg(feature = "wasm-extensions")]
    pub fn register_all<DB>(
        registry: &mut ExtensionRegistry<DB>,
        loaded: &[LoadedExtension],
    ) -> Result<(), RegisterError>
    where
        DB: ?Sized + Send + Sync + crate::wasm::WasmHostDb + 'static,
    {
        let mut ordered: Vec<_> = loaded.iter().collect();
        ordered.sort_by(|a, b| a.id().cmp(b.id()));

        for ext in ordered {
            let mut scratch = registry.clone();
            Self::register_one(&mut scratch, ext)?;
            *registry = scratch;
        }

        Ok(())
    }

    #[cfg(feature = "wasm-extensions")]
    pub fn register_all_best_effort<DB>(
        registry: &mut ExtensionRegistry<DB>,
        loaded: &[LoadedExtension],
    ) -> RegisterReport
    where
        DB: ?Sized + Send + Sync + crate::wasm::WasmHostDb + 'static,
    {
        let mut ordered: Vec<_> = loaded.iter().collect();
        ordered.sort_by(|a, b| a.id().cmp(b.id()));

        let mut report = RegisterReport::default();

        for ext in ordered {
            let metadata = ext.metadata();
            let mut scratch = registry.clone();
            match Self::register_one(&mut scratch, ext) {
                Ok(()) => {
                    *registry = scratch;
                    report.registered.push(metadata);
                }
                Err(error) => report.errors.push(RegisterFailure {
                    extension: metadata,
                    error,
                }),
            }
        }

        report
    }

    #[cfg(feature = "wasm-extensions")]
    fn register_one<DB>(
        registry: &mut ExtensionRegistry<DB>,
        ext: &LoadedExtension,
    ) -> Result<(), RegisterError>
    where
        DB: ?Sized + Send + Sync + crate::wasm::WasmHostDb + 'static,
    {
        use crate::traits::{
            CodeActionProvider, CompletionProvider, DiagnosticProvider, InlayHintProvider,
            NavigationProvider,
        };
        use crate::wasm::{WasmCapabilities, WasmPlugin, WasmPluginConfig};
        use std::sync::Arc;

        let plugin =
            WasmPlugin::from_wasm_bytes(ext.id(), ext.entry_bytes(), WasmPluginConfig::default())
                .map_err(|err| RegisterError::WasmCompile {
                id: ext.id().to_string(),
                dir: ext.dir().to_path_buf(),
                entry_path: ext.entry_path().to_path_buf(),
                message: err.to_string(),
            })?;
        let plugin = Arc::new(plugin);
        let caps = plugin.capabilities();

        for cap in &ext.manifest().capabilities {
            match cap {
                ExtensionCapability::Diagnostics => {
                    if !caps.contains(WasmCapabilities::DIAGNOSTICS) {
                        return Err(RegisterError::WasmCapabilityNotSupported {
                            id: ext.id().to_string(),
                            dir: ext.dir().to_path_buf(),
                            capability: cap.as_str().to_string(),
                        });
                    }
                    registry.register_diagnostic_provider(
                        Arc::clone(&plugin) as Arc<dyn DiagnosticProvider<DB>>
                    )?;
                }
                ExtensionCapability::Completion => {
                    if !caps.contains(WasmCapabilities::COMPLETIONS) {
                        return Err(RegisterError::WasmCapabilityNotSupported {
                            id: ext.id().to_string(),
                            dir: ext.dir().to_path_buf(),
                            capability: cap.as_str().to_string(),
                        });
                    }
                    registry.register_completion_provider(
                        Arc::clone(&plugin) as Arc<dyn CompletionProvider<DB>>
                    )?;
                }
                ExtensionCapability::CodeAction => {
                    if !caps.contains(WasmCapabilities::CODE_ACTIONS) {
                        return Err(RegisterError::WasmCapabilityNotSupported {
                            id: ext.id().to_string(),
                            dir: ext.dir().to_path_buf(),
                            capability: cap.as_str().to_string(),
                        });
                    }
                    registry.register_code_action_provider(
                        Arc::clone(&plugin) as Arc<dyn CodeActionProvider<DB>>
                    )?;
                }
                ExtensionCapability::Navigation => {
                    if !caps.contains(WasmCapabilities::NAVIGATION) {
                        return Err(RegisterError::WasmCapabilityNotSupported {
                            id: ext.id().to_string(),
                            dir: ext.dir().to_path_buf(),
                            capability: cap.as_str().to_string(),
                        });
                    }
                    registry.register_navigation_provider(
                        Arc::clone(&plugin) as Arc<dyn NavigationProvider<DB>>
                    )?;
                }
                ExtensionCapability::InlayHint => {
                    if !caps.contains(WasmCapabilities::INLAY_HINTS) {
                        return Err(RegisterError::WasmCapabilityNotSupported {
                            id: ext.id().to_string(),
                            dir: ext.dir().to_path_buf(),
                            capability: cap.as_str().to_string(),
                        });
                    }
                    registry.register_inlay_hint_provider(
                        Arc::clone(&plugin) as Arc<dyn InlayHintProvider<DB>>
                    )?;
                }
            }
        }

        Ok(())
    }
}

fn sanitize_toml_error_message(message: &str) -> String {
    fn redact_quoted(message: &str, quote: char) -> String {
        const REDACTED: &str = "<redacted>";
        let mut out = String::with_capacity(message.len());
        let mut rest = message;
        while let Some(start) = rest.find(quote) {
            out.push_str(&rest[..start]);
            let quote_len = quote.len_utf8();
            out.push(quote);
            let Some(after_open) = rest.get(start + quote_len..) else {
                // Unterminated quote: redact the remainder and stop.
                out.push_str(REDACTED);
                return out;
            };

            let quote_byte = quote as u8;
            let bytes = after_open.as_bytes();
            let mut end = None;
            for (idx, &b) in bytes.iter().enumerate() {
                if b != quote_byte {
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
                out.push_str(REDACTED);
                return out;
            };

            out.push_str(REDACTED);
            out.push(quote);
            let Some(after_close) = after_open.get(end + quote_len..) else {
                return out;
            };
            rest = after_close;
        }
        out.push_str(rest);
        out
    }

    // `toml::de::Error::message()` can still include user-provided scalar values in quotes, for
    // example:
    // `invalid semver version 'secret': ...` or `invalid type: string "secret", expected u32`.
    //
    // Extension load errors are commonly surfaced through CLI/LSP diagnostics and logs; redact
    // quoted substrings to avoid leaking arbitrary manifest contents.
    let mut out = redact_quoted(message, '"');
    out = redact_quoted(&out, '\'');

    // `serde` / `toml` wrap offending enum variants (and unknown fields) in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Only redact the *first* backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        let after_start = &out[start.saturating_add(1)..];
        let end = if let Some(end_rel) = after_start.rfind("`, expected") {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else if let Some(end_rel) = after_start.rfind('`') {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else {
            None
        };
        if let Some(end) = end {
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

fn resolve_entry_path(dir: &Path, manifest: &ExtensionManifest) -> Result<PathBuf, LoadError> {
    if manifest
        .entry
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(LoadError::EntryEscapesDirectory {
            dir: dir.to_path_buf(),
            id: manifest.id.clone(),
            entry: manifest.entry.clone(),
        });
    }

    let mut entry_path = dir.to_path_buf();
    for component in manifest.entry.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => entry_path.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(LoadError::EntryEscapesDirectory {
                    dir: dir.to_path_buf(),
                    id: manifest.id.clone(),
                    entry: manifest.entry.clone(),
                });
            }
        }
    }

    Ok(entry_path)
}

fn id_matches_pattern(id: &str, pattern: &str) -> bool {
    let id = id.trim();
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }

    if !pattern.contains('*') {
        return id == pattern;
    }

    let starts_with_star = pattern.starts_with('*');
    let ends_with_star = pattern.ends_with('*');

    let parts: Vec<&str> = pattern.split('*').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return true;
    }

    let mut pos = 0usize;
    for (idx, part) in parts.iter().enumerate() {
        let Some(found) = id[pos..].find(part) else {
            return false;
        };
        let found_pos = pos + found;
        if idx == 0 && !starts_with_star && found_pos != 0 {
            return false;
        }
        pos = found_pos + part.len();
    }

    if !ends_with_star {
        return pos == id.len();
    }

    true
}

fn discover_extension_dirs(search_path: &Path) -> Result<Vec<PathBuf>, LoadError> {
    if !search_path.exists() {
        return Err(LoadError::SearchPathMissing {
            path: search_path.to_path_buf(),
        });
    }
    if !search_path.is_dir() {
        return Err(LoadError::SearchPathNotDirectory {
            path: search_path.to_path_buf(),
        });
    }

    let manifest_path = search_path.join(MANIFEST_FILE_NAME);
    if manifest_path.is_file() {
        return Ok(vec![search_path.to_path_buf()]);
    }

    let read_dir = fs::read_dir(search_path).map_err(|source| LoadError::SearchPathReadDir {
        path: search_path.to_path_buf(),
        source,
    })?;

    let mut out = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|source| LoadError::SearchPathEnumerate {
            path: search_path.to_path_buf(),
            source,
        })?;
        let child_path = entry.path();
        if !child_path.is_dir() {
            continue;
        }
        if child_path.join(MANIFEST_FILE_NAME).is_file() {
            out.push(child_path);
        }
    }

    Ok(out)
}

fn find_duplicate_extension_ids(loaded: &[LoadedExtension]) -> BTreeMap<String, Vec<PathBuf>> {
    let mut by_id: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for ext in loaded {
        by_id
            .entry(ext.id().to_string())
            .or_default()
            .push(ext.dir.clone());
    }

    by_id.retain(|_, dirs| dirs.len() > 1);
    by_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, manifest: &str) {
        fs::write(dir.join(MANIFEST_FILE_NAME), manifest).unwrap();
    }

    fn write_dummy_wasm(dir: &Path, file: &str) {
        let path = dir.join(file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, [0u8; 1]).unwrap();
    }

    #[test]
    fn toml_error_sanitization_does_not_echo_string_values_with_escaped_quotes() {
        #[derive(serde::Deserialize)]
        struct Dummy {
            #[allow(dead_code)]
            enabled: bool,
        }

        let secret_suffix = "nova-ext-toml-secret-token";
        let text = format!(r#"enabled = "prefix\"{secret_suffix}""#);

        let raw_err = match toml::from_str::<Dummy>(&text) {
            Ok(_) => panic!("expected type mismatch"),
            Err(err) => err,
        };
        let raw_message = raw_err.message();
        assert!(
            raw_message.contains(secret_suffix),
            "expected raw toml error message to include the string value so this test would catch leaks: {raw_message}"
        );

        let message = sanitize_toml_error_message(raw_message);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized toml error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized toml error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn toml_error_sanitization_does_not_echo_backticked_values_with_embedded_expected_delimiter() {
        #[derive(serde::Deserialize)]
        struct Dummy {
            #[allow(dead_code)]
            kind: Kind,
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Kind {
            Foo,
        }

        let secret_suffix = "nova-ext-toml-backticked-secret-token";
        let secret = format!("prefix`, expected {secret_suffix}");
        let text = format!(r#"kind = "{secret}""#);

        let raw_err = match toml::from_str::<Dummy>(&text) {
            Ok(_) => panic!("expected unknown variant"),
            Err(err) => err,
        };
        let raw_message = raw_err.message();
        assert!(
            raw_message.contains(secret_suffix),
            "expected raw toml error message to include the string value so this test would catch leaks: {raw_message}"
        );

        let message = sanitize_toml_error_message(raw_message);
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized toml error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized toml error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn happy_path_loads_two_extensions_and_sorts_by_id() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let ext_b = root.join("ext-b");
        fs::create_dir_all(&ext_b).unwrap();
        write_manifest(
            &ext_b,
            r#"
id = "b"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
        );
        write_dummy_wasm(&ext_b, "plugin.wasm");

        let ext_a = root.join("ext-a");
        fs::create_dir_all(&ext_a).unwrap();
        write_manifest(
            &ext_a,
            r#"
id = "a"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
        );
        write_dummy_wasm(&ext_a, "plugin.wasm");

        let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id(), "a");
        assert_eq!(loaded[1].id(), "b");

        let listed = ExtensionManager::list(&[loaded[1].clone(), loaded[0].clone()]);
        assert_eq!(
            listed.into_iter().map(|m| m.id).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn rejects_duplicate_extension_ids() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let ext_1 = root.join("ext-1");
        fs::create_dir_all(&ext_1).unwrap();
        write_manifest(
            &ext_1,
            r#"
id = "dup"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
        );
        write_dummy_wasm(&ext_1, "plugin.wasm");

        let ext_2 = root.join("ext-2");
        fs::create_dir_all(&ext_2).unwrap();
        write_manifest(
            &ext_2,
            r#"
id = "dup"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
        );
        write_dummy_wasm(&ext_2, "plugin.wasm");

        let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
        assert!(loaded.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(errors[0], LoadError::DuplicateId { .. }),
            "{errors:?}"
        );
    }

    #[test]
    fn reports_missing_entry_file() {
        let temp = TempDir::new().unwrap();
        let ext_dir = temp.path().join("ext");
        fs::create_dir_all(&ext_dir).unwrap();
        write_manifest(
            &ext_dir,
            r#"
id = "missing-entry"
version = "0.1.0"
entry = "missing.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
        );

        let err = ExtensionManager::load_from_dir(&ext_dir).unwrap_err();
        assert!(matches!(err, LoadError::MissingEntry { .. }), "{err:?}");
    }

    #[test]
    fn rejects_entry_paths_with_parent_dir() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        fs::write(root.join("outside.wasm"), [0u8; 1]).unwrap();

        let ext_dir = root.join("ext");
        fs::create_dir_all(&ext_dir).unwrap();
        write_manifest(
            &ext_dir,
            r#"
id = "traversal"
version = "0.1.0"
entry = "../outside.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
        );

        let err = ExtensionManager::load_from_dir(&ext_dir).unwrap_err();
        assert!(
            matches!(err, LoadError::EntryEscapesDirectory { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn supports_nested_entry_paths() {
        let temp = TempDir::new().unwrap();
        let ext_dir = temp.path().join("ext");
        fs::create_dir_all(&ext_dir).unwrap();
        write_manifest(
            &ext_dir,
            r#"
id = "nested"
version = "0.1.0"
entry = "bin/plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
        );
        write_dummy_wasm(&ext_dir, "bin/plugin.wasm");

        let ext = ExtensionManager::load_from_dir(&ext_dir).unwrap();
        assert_eq!(
            ext.entry_path().strip_prefix(&ext_dir).unwrap(),
            Path::new("bin/plugin.wasm")
        );
    }

    #[test]
    fn reports_manifest_parse_errors() {
        let temp = TempDir::new().unwrap();
        let ext_dir = temp.path().join("ext");
        fs::create_dir_all(&ext_dir).unwrap();

        write_manifest(&ext_dir, "not = [valid");

        let err = ExtensionManager::load_from_dir(&ext_dir).unwrap_err();
        assert!(matches!(err, LoadError::ManifestParse { .. }), "{err:?}");
    }

    #[test]
    fn manifest_parse_errors_do_not_leak_string_values_or_source_snippets() {
        let secret = "super-secret-api-key";
        let temp = TempDir::new().unwrap();
        let ext_dir = temp.path().join("ext");
        fs::create_dir_all(&ext_dir).unwrap();

        // Invalid semver versions include the raw string in a custom error message wrapped in
        // single quotes; ensure we don't leak arbitrary string values in logs/diagnostics.
        write_manifest(
            &ext_dir,
            &format!(
                r#"
id = "test"
version = "{secret}"
entry = "plugin.wasm"
abi_version = {SUPPORTED_ABI_VERSION}
capabilities = ["diagnostics"]
"#
            ),
        );
        write_dummy_wasm(&ext_dir, "plugin.wasm");

        let err = ExtensionManager::load_from_dir(&ext_dir).unwrap_err();
        let message = err.to_string();

        assert!(
            !message.contains(secret),
            "LoadError leaked string value from manifest: {message}"
        );
        assert!(
            !message.contains("version ="),
            "LoadError display should not include source snippets: {message}"
        );
        assert!(
            err.source().is_none(),
            "LoadError should not expose toml::de::Error as source (may leak manifest snippets)"
        );

        let debug = format!("{err:?}");
        assert!(
            !debug.contains(secret),
            "LoadError debug leaked string value from manifest: {debug}"
        );
    }

    #[test]
    fn id_glob_matching_supports_simple_wildcards() {
        assert!(id_matches_pattern("com.mycorp.rules", "com.mycorp.rules"));
        assert!(id_matches_pattern("com.mycorp.rules", "com.mycorp.*"));
        assert!(id_matches_pattern("com.mycorp.rules", "*rules*"));
        assert!(id_matches_pattern("com.mycorp.rules", "com.*.rules*"));
        assert!(!id_matches_pattern("com.mycorp.other", "com.*.rules*"));
    }

    #[test]
    fn filter_by_id_applies_allow_and_deny_lists() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let loaded = vec![
            LoadedExtension {
                dir: root.join("a"),
                manifest: ExtensionManifest {
                    id: "com.mycorp.rules".to_string(),
                    version: semver::Version::parse("0.1.0").unwrap(),
                    entry: PathBuf::from("plugin.wasm"),
                    abi_version: SUPPORTED_ABI_VERSION,
                    capabilities: vec![ExtensionCapability::Diagnostics],
                    name: None,
                    description: None,
                    authors: Vec::new(),
                    homepage: None,
                    license: None,
                    config_schema: None,
                },
                entry_path: root.join("a/plugin.wasm"),
                entry_bytes: Vec::new(),
            },
            LoadedExtension {
                dir: root.join("b"),
                manifest: ExtensionManifest {
                    id: "com.evil.rules".to_string(),
                    version: semver::Version::parse("0.1.0").unwrap(),
                    entry: PathBuf::from("plugin.wasm"),
                    abi_version: SUPPORTED_ABI_VERSION,
                    capabilities: vec![ExtensionCapability::Diagnostics],
                    name: None,
                    description: None,
                    authors: Vec::new(),
                    homepage: None,
                    license: None,
                    config_schema: None,
                },
                entry_path: root.join("b/plugin.wasm"),
                entry_bytes: Vec::new(),
            },
            LoadedExtension {
                dir: root.join("c"),
                manifest: ExtensionManifest {
                    id: "com.mycorp.other".to_string(),
                    version: semver::Version::parse("0.1.0").unwrap(),
                    entry: PathBuf::from("plugin.wasm"),
                    abi_version: SUPPORTED_ABI_VERSION,
                    capabilities: vec![ExtensionCapability::Diagnostics],
                    name: None,
                    description: None,
                    authors: Vec::new(),
                    homepage: None,
                    license: None,
                    config_schema: None,
                },
                entry_path: root.join("c/plugin.wasm"),
                entry_bytes: Vec::new(),
            },
        ];

        let allow = vec!["com.*.rules*".to_string()];
        let deny = vec!["com.evil.*".to_string()];

        let (kept, errors) = ExtensionManager::filter_by_id(loaded, Some(&allow), &deny);
        assert_eq!(
            kept.iter().map(|ext| ext.id()).collect::<Vec<_>>(),
            vec!["com.mycorp.rules"]
        );
        assert!(
            errors.iter().any(
                |err| matches!(err, LoadError::DeniedByConfig { id, .. } if id == "com.evil.rules")
            ),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|err| matches!(err, LoadError::NotAllowedByConfig { id, .. } if id == "com.mycorp.other")),
            "{errors:?}"
        );
    }

    #[cfg(feature = "wasm-extensions")]
    mod wasm_tests {
        use super::*;
        use crate::traits::{CompletionParams, DiagnosticParams};
        use crate::wasm::WasmHostDb;
        use crate::ExtensionContext;
        use nova_config::NovaConfig;
        use nova_core::{FileId, ProjectId};
        use nova_scheduler::CancellationToken;
        use std::sync::Arc;

        struct TestDb {
            text: String,
        }

        impl WasmHostDb for TestDb {
            fn file_text(&self, _file: FileId) -> &str {
                &self.text
            }
        }

        fn ctx(db: Arc<TestDb>) -> ExtensionContext<TestDb> {
            ExtensionContext::new(
                db,
                Arc::new(NovaConfig::default()),
                ProjectId::new(0),
                CancellationToken::new(),
            )
        }

        fn registry_without_metrics() -> ExtensionRegistry<TestDb> {
            let mut options = crate::ExtensionRegistryOptions::default();
            options.metrics = None;
            ExtensionRegistry::new(options)
        }

        fn write_wat_wasm(dir: &Path, file: &str, wat: &str) {
            let bytes = wat::parse_str(wat).unwrap();
            fs::write(dir.join(file), bytes).unwrap();
        }

        fn escape_wat_string(value: &str) -> String {
            value.replace('\\', "\\\\").replace('\"', "\\\"")
        }

        fn simple_wat(diag_message: &str, completion_label: Option<&str>) -> String {
            let diag_json = format!("[{{\"message\":\"{}\"}}]", diag_message);
            let diag_len = diag_json.len();
            let diag_wat = escape_wat_string(&diag_json);

            let completion_offset = 256;
            let completion = completion_label.map(|label| {
                let json = format!("[{{\"label\":\"{}\"}}]", label);
                let len = json.len();
                let wat = escape_wat_string(&json);
                (wat, len)
            });

            let caps_bits = if completion.is_some() { 3 } else { 1 };

            let mut out = String::new();
            out.push_str("(module\n");
            out.push_str("  (memory (export \"memory\") 1)\n");
            out.push_str("  (global $heap (mut i32) (i32.const 1024))\n\n");
            out.push_str("  (func $nova_ext_alloc (export \"nova_ext_alloc\") (param $len i32) (result i32)\n");
            out.push_str("    (local $ptr i32)\n");
            out.push_str("    (local.set $ptr (global.get $heap))\n");
            out.push_str("    (global.set $heap (i32.add (global.get $heap) (local.get $len)))\n");
            out.push_str("    (local.get $ptr)\n");
            out.push_str("  )\n\n");
            out.push_str("  (func $nova_ext_free (export \"nova_ext_free\") (param i32 i32)\n");
            out.push_str("    nop\n");
            out.push_str("  )\n\n");
            out.push_str("  (func (export \"nova_ext_abi_version\") (result i32) (i32.const 1))\n");
            out.push_str(&format!(
                "  (func (export \"nova_ext_capabilities\") (result i32) (i32.const {caps_bits}))\n\n"
            ));
            out.push_str(&format!("  (data (i32.const 0) \"{diag_wat}\")\n"));

            if let Some((completion_wat, _)) = &completion {
                out.push_str(&format!(
                    "  (data (i32.const {completion_offset}) \"{completion_wat}\")\n"
                ));
            }
            out.push('\n');

            out.push_str(
                "  (func (export \"nova_ext_diagnostics\") (param i32 i32) (result i64)\n",
            );
            out.push_str("    (local $out_ptr i32)\n");
            out.push_str("    (local $out_len i32)\n");
            out.push_str(&format!(
                "    (local.set $out_len (i32.const {diag_len}))\n"
            ));
            out.push_str("    (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))\n");
            out.push_str(
                "    (memory.copy (local.get $out_ptr) (i32.const 0) (local.get $out_len))\n",
            );
            out.push_str("    (i64.or\n");
            out.push_str(
                "      (i64.shl (i64.extend_i32_u (local.get $out_len)) (i64.const 32))\n",
            );
            out.push_str("      (i64.extend_i32_u (local.get $out_ptr))\n");
            out.push_str("    )\n");
            out.push_str("  )\n\n");

            if let Some((_completion_wat, completion_len)) = completion {
                out.push_str(
                    "  (func (export \"nova_ext_completions\") (param i32 i32) (result i64)\n",
                );
                out.push_str("    (local $out_ptr i32)\n");
                out.push_str("    (local $out_len i32)\n");
                out.push_str(&format!(
                    "    (local.set $out_len (i32.const {completion_len}))\n"
                ));
                out.push_str(
                    "    (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))\n",
                );
                out.push_str(&format!(
                    "    (memory.copy (local.get $out_ptr) (i32.const {completion_offset}) (local.get $out_len))\n"
                ));
                out.push_str("    (i64.or\n");
                out.push_str(
                    "      (i64.shl (i64.extend_i32_u (local.get $out_len)) (i64.const 32))\n",
                );
                out.push_str("      (i64.extend_i32_u (local.get $out_ptr))\n");
                out.push_str("    )\n");
                out.push_str("  )\n");
            }

            out.push_str(")\n");
            out
        }

        #[test]
        fn registers_all_providers_deterministically() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();

            let ext_b = root.join("ext-b");
            fs::create_dir_all(&ext_b).unwrap();
            write_manifest(
                &ext_b,
                r#"
id = "b"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
            );
            write_wat_wasm(&ext_b, "plugin.wasm", &simple_wat("from-b", None));

            let ext_a = root.join("ext-a");
            fs::create_dir_all(&ext_a).unwrap();
            write_manifest(
                &ext_a,
                r#"
id = "a"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
            );
            write_wat_wasm(&ext_a, "plugin.wasm", &simple_wat("from-a", None));

            let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
            assert!(errors.is_empty(), "{errors:?}");

            let mut registry = registry_without_metrics();
            let report = ExtensionManager::register_all_best_effort(&mut registry, &loaded);
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(
                report
                    .registered
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["a", "b"]
            );

            let db = Arc::new(TestDb {
                text: String::new(),
            });
            let out = registry.diagnostics(
                ctx(db),
                DiagnosticParams {
                    file: FileId::from_raw(1),
                },
            );
            assert_eq!(
                out.into_iter().map(|d| d.message).collect::<Vec<_>>(),
                vec!["from-a".to_string(), "from-b".to_string()]
            );
        }

        #[test]
        fn supports_multiple_capabilities_from_one_extension() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();

            let ext = root.join("multi");
            fs::create_dir_all(&ext).unwrap();
            write_manifest(
                &ext,
                r#"
id = "multi"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics", "completions"]
"#,
            );

            write_wat_wasm(
                &ext,
                "plugin.wasm",
                &simple_wat("from-multi", Some("completion")),
            );

            let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
            assert!(errors.is_empty(), "{errors:?}");

            let mut registry = registry_without_metrics();
            let report = ExtensionManager::register_all_best_effort(&mut registry, &loaded);
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(report.registered.len(), 1);

            let db = Arc::new(TestDb {
                text: String::new(),
            });
            let diagnostics = registry.diagnostics(
                ctx(Arc::clone(&db)),
                DiagnosticParams {
                    file: FileId::from_raw(1),
                },
            );
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].message, "from-multi");

            let completions = registry.completions(
                ctx(db),
                CompletionParams {
                    file: FileId::from_raw(1),
                    offset: 0,
                },
            );
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].label, "completion");
        }

        #[test]
        fn reports_wasm_compile_error_but_registers_other_extensions() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();

            let bad_ext = root.join("ext-bad");
            fs::create_dir_all(&bad_ext).unwrap();
            write_manifest(
                &bad_ext,
                r#"
id = "bad"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
            );
            // Invalid WASM bytes (will fail Wasmtime compilation).
            fs::write(bad_ext.join("plugin.wasm"), [0u8; 1]).unwrap();

            let ok_ext = root.join("ext-ok");
            fs::create_dir_all(&ok_ext).unwrap();
            write_manifest(
                &ok_ext,
                r#"
id = "ok"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
            );
            write_wat_wasm(&ok_ext, "plugin.wasm", &simple_wat("from-ok", None));

            let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
            assert!(errors.is_empty(), "{errors:?}");

            let mut registry = ExtensionRegistry::<TestDb>::default();
            let report = ExtensionManager::register_all_best_effort(&mut registry, &loaded);
            assert_eq!(report.registered.len(), 1);
            assert_eq!(report.registered[0].id.as_str(), "ok");
            assert_eq!(report.errors.len(), 1);
            assert_eq!(report.errors[0].extension.id.as_str(), "bad");
            assert!(
                matches!(&report.errors[0].error, RegisterError::WasmCompile { id, .. } if id == "bad"),
                "{:?}",
                report.errors[0]
            );

            let db = Arc::new(TestDb {
                text: String::new(),
            });
            let out = registry.diagnostics(
                ctx(db),
                DiagnosticParams {
                    file: FileId::from_raw(1),
                },
            );
            assert_eq!(
                out.into_iter().map(|d| d.message).collect::<Vec<_>>(),
                vec!["from-ok".to_string()]
            );
        }

        #[test]
        fn reports_capability_mismatch_and_does_not_partially_register() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();

            let bad_ext = root.join("ext-bad");
            fs::create_dir_all(&bad_ext).unwrap();
            write_manifest(
                &bad_ext,
                r#"
id = "bad"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics", "completions"]
"#,
            );
            // Implements diagnostics, but not completions.
            write_wat_wasm(&bad_ext, "plugin.wasm", &simple_wat("from-bad", None));

            let ok_ext = root.join("ext-ok");
            fs::create_dir_all(&ok_ext).unwrap();
            write_manifest(
                &ok_ext,
                r#"
id = "ok"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
            );
            write_wat_wasm(&ok_ext, "plugin.wasm", &simple_wat("from-ok", None));

            let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
            assert!(errors.is_empty(), "{errors:?}");

            let mut registry = ExtensionRegistry::<TestDb>::default();
            let report = ExtensionManager::register_all_best_effort(&mut registry, &loaded);
            assert_eq!(report.registered.len(), 1);
            assert_eq!(report.registered[0].id.as_str(), "ok");
            assert_eq!(report.errors.len(), 1);
            assert_eq!(report.errors[0].extension.id.as_str(), "bad");
            assert!(
                matches!(&report.errors[0].error, RegisterError::WasmCapabilityNotSupported { id, capability, .. } if id == "bad" && capability == "completion"),
                "{:?}",
                report.errors[0]
            );

            // Ensure the failed extension's diagnostics provider was not partially registered.
            let db = Arc::new(TestDb {
                text: String::new(),
            });
            let out = registry.diagnostics(
                ctx(db),
                DiagnosticParams {
                    file: FileId::from_raw(1),
                },
            );
            assert_eq!(
                out.into_iter().map(|d| d.message).collect::<Vec<_>>(),
                vec!["from-ok".to_string()]
            );
        }

        #[test]
        fn does_not_partially_register_when_provider_registration_fails() {
            use crate::traits::{CompletionParams, CompletionProvider};
            use nova_types::CompletionItem;

            struct ExistingCompletionProvider;

            impl CompletionProvider<TestDb> for ExistingCompletionProvider {
                fn id(&self) -> &str {
                    "dup"
                }

                fn provide_completions(
                    &self,
                    _ctx: ExtensionContext<TestDb>,
                    _params: CompletionParams,
                ) -> Vec<CompletionItem> {
                    vec![CompletionItem::new("preexisting")]
                }
            }

            let temp = TempDir::new().unwrap();
            let root = temp.path();

            let dup_ext = root.join("ext-dup");
            fs::create_dir_all(&dup_ext).unwrap();
            write_manifest(
                &dup_ext,
                r#"
id = "dup"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics", "completions"]
"#,
            );
            write_wat_wasm(
                &dup_ext,
                "plugin.wasm",
                &simple_wat("from-dup", Some("dup")),
            );

            let ok_ext = root.join("ext-ok");
            fs::create_dir_all(&ok_ext).unwrap();
            write_manifest(
                &ok_ext,
                r#"
id = "ok"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
"#,
            );
            write_wat_wasm(&ok_ext, "plugin.wasm", &simple_wat("from-ok", None));

            let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
            assert!(errors.is_empty(), "{errors:?}");

            let mut registry = ExtensionRegistry::<TestDb>::default();
            registry
                .register_completion_provider(Arc::new(ExistingCompletionProvider))
                .unwrap();

            let report = ExtensionManager::register_all_best_effort(&mut registry, &loaded);
            assert_eq!(report.registered.len(), 1);
            assert_eq!(report.registered[0].id.as_str(), "ok");
            assert_eq!(report.errors.len(), 1);
            assert_eq!(report.errors[0].extension.id.as_str(), "dup");
            assert!(
                matches!(report.errors[0].error.clone(), RegisterError::DuplicateId { kind, id } if kind == "completion" && id == "dup"),
                "{:?}",
                report.errors[0]
            );

            // Ensure the failed extension didn't leave a diagnostic provider behind.
            let db = Arc::new(TestDb {
                text: String::new(),
            });
            let diags = registry.diagnostics(
                ctx(Arc::clone(&db)),
                DiagnosticParams {
                    file: FileId::from_raw(1),
                },
            );
            assert_eq!(
                diags.into_iter().map(|d| d.message).collect::<Vec<_>>(),
                vec!["from-ok".to_string()]
            );

            // The preexisting completions provider should still be present.
            let completions = registry.completions(
                ctx(db),
                CompletionParams {
                    file: FileId::from_raw(1),
                    offset: 0,
                },
            );
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].label, "preexisting");
        }
    }
}
