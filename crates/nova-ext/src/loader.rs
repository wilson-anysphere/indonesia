use crate::manifest::{ExtensionCapability, ExtensionManifest, MANIFEST_FILE_NAME, SUPPORTED_ABI_VERSION};
use crate::{ExtensionRegistry, RegisterError};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

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
    #[error("failed to parse extension manifest {manifest_path:?}: {source}")]
    ManifestParse {
        manifest_path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("extension {id:?} at {dir:?}: unsupported abi_version {abi_version} (supported: {supported})")]
    UnsupportedAbiVersion {
        dir: PathBuf,
        id: String,
        abi_version: u32,
        supported: u32,
    },
    #[error("extension {id:?} at {dir:?}: entry must be a relative path, got {entry:?}")]
    EntryNotRelative { dir: PathBuf, id: String, entry: PathBuf },
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
            return Err(LoadError::MissingDirectory { dir: dir.to_path_buf() });
        }
        if !dir.is_dir() {
            return Err(LoadError::NotADirectory { dir: dir.to_path_buf() });
        }

        let manifest_path = dir.join(MANIFEST_FILE_NAME);
        let manifest_text =
            fs::read_to_string(&manifest_path).map_err(|source| LoadError::ManifestRead {
                manifest_path: manifest_path.clone(),
                source,
            })?;

        let manifest = toml::from_str::<ExtensionManifest>(&manifest_text).map_err(|source| {
            LoadError::ManifestParse {
                manifest_path: manifest_path.clone(),
                source,
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

        let entry_path = dir.join(&manifest.entry);
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
            errors.extend(duplicates.into_iter().map(|(id, dirs)| LoadError::DuplicateId { id, dirs }));
            loaded.retain(|ext| !dup_ids.contains(ext.id()));
        }

        loaded.sort_by(|a, b| a.id().cmp(b.id()));

        (loaded, errors)
    }

    pub fn list(loaded: &[LoadedExtension]) -> Vec<ExtensionMetadata> {
        let mut out: Vec<_> = loaded.iter().map(LoadedExtension::metadata).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn register_all<DB>(
        registry: &mut ExtensionRegistry<DB>,
        loaded: &[LoadedExtension],
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

        let mut ordered: Vec<_> = loaded.iter().collect();
        ordered.sort_by(|a, b| a.id().cmp(b.id()));

        for ext in ordered {
            let plugin = WasmPlugin::from_wasm_bytes(ext.id(), ext.entry_bytes(), WasmPluginConfig::default())
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
                            Arc::clone(&plugin) as Arc<dyn DiagnosticProvider<DB>>,
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
                            Arc::clone(&plugin) as Arc<dyn CompletionProvider<DB>>,
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
                            Arc::clone(&plugin) as Arc<dyn CodeActionProvider<DB>>,
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
                            Arc::clone(&plugin) as Arc<dyn NavigationProvider<DB>>,
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
                            Arc::clone(&plugin) as Arc<dyn InlayHintProvider<DB>>,
                        )?;
                    }
                }
            }
        }

        Ok(())
    }
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
        by_id.entry(ext.id().to_string()).or_default().push(ext.dir.clone());
    }

    by_id.retain(|_, dirs| dirs.len() > 1);
    by_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, manifest: &str) {
        fs::write(dir.join(MANIFEST_FILE_NAME), manifest).unwrap();
    }

    fn write_dummy_wasm(dir: &Path, file: &str) {
        fs::write(dir.join(file), [0u8; 1]).unwrap();
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
        assert_eq!(listed.into_iter().map(|m| m.id).collect::<Vec<_>>(), vec!["a", "b"]);
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
        assert!(matches!(errors[0], LoadError::DuplicateId { .. }), "{errors:?}");
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
    fn reports_manifest_parse_errors() {
        let temp = TempDir::new().unwrap();
        let ext_dir = temp.path().join("ext");
        fs::create_dir_all(&ext_dir).unwrap();

        write_manifest(&ext_dir, "not = [valid");

        let err = ExtensionManager::load_from_dir(&ext_dir).unwrap_err();
        assert!(matches!(err, LoadError::ManifestParse { .. }), "{err:?}");
    }

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

        fn write_wat_wasm(dir: &Path, file: &str, wat: &str) {
            let bytes = wat::parse_str(wat).unwrap();
            fs::write(dir.join(file), bytes).unwrap();
        }

        fn escape_wat_string(value: &str) -> String {
            value.replace('\\', "\\\\").replace('\"', "\\\"")
        }

        fn simple_wat(diag_message: &str, completion_label: Option<&str>) -> String {
            let diag_json = format!("[{{\"message\":\"{}\"}}]", diag_message);
            let diag_len = diag_json.as_bytes().len();
            let diag_wat = escape_wat_string(&diag_json);

            let completion_offset = 256;
            let completion = completion_label.map(|label| {
                let json = format!("[{{\"label\":\"{}\"}}]", label);
                let len = json.as_bytes().len();
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

            out.push_str("  (func (export \"nova_ext_diagnostics\") (param i32 i32) (result i64)\n");
            out.push_str("    (local $out_ptr i32)\n");
            out.push_str("    (local $out_len i32)\n");
            out.push_str(&format!("    (local.set $out_len (i32.const {diag_len}))\n"));
            out.push_str("    (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))\n");
            out.push_str("    (memory.copy (local.get $out_ptr) (i32.const 0) (local.get $out_len))\n");
            out.push_str("    (i64.or\n");
            out.push_str("      (i64.shl (i64.extend_i32_u (local.get $out_len)) (i64.const 32))\n");
            out.push_str("      (i64.extend_i32_u (local.get $out_ptr))\n");
            out.push_str("    )\n");
            out.push_str("  )\n\n");

            if let Some((_completion_wat, completion_len)) = completion {
                out.push_str("  (func (export \"nova_ext_completions\") (param i32 i32) (result i64)\n");
                out.push_str("    (local $out_ptr i32)\n");
                out.push_str("    (local $out_len i32)\n");
                out.push_str(&format!("    (local.set $out_len (i32.const {completion_len}))\n"));
                out.push_str("    (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))\n");
                out.push_str(&format!(
                    "    (memory.copy (local.get $out_ptr) (i32.const {completion_offset}) (local.get $out_len))\n"
                ));
                out.push_str("    (i64.or\n");
                out.push_str("      (i64.shl (i64.extend_i32_u (local.get $out_len)) (i64.const 32))\n");
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
            write_wat_wasm(
                &ext_b,
                "plugin.wasm",
                &simple_wat("from-b", None),
            );

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
            write_wat_wasm(
                &ext_a,
                "plugin.wasm",
                &simple_wat("from-a", None),
            );

            let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
            assert!(errors.is_empty(), "{errors:?}");

            let mut registry = ExtensionRegistry::<TestDb>::default();
            ExtensionManager::register_all(&mut registry, &loaded).unwrap();

            let db = Arc::new(TestDb { text: String::new() });
            let out = registry.diagnostics(ctx(db), DiagnosticParams { file: FileId::from_raw(1) });
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

            write_wat_wasm(&ext, "plugin.wasm", &simple_wat("from-multi", Some("completion")));

            let (loaded, errors) = ExtensionManager::load_all(&[root.to_path_buf()]);
            assert!(errors.is_empty(), "{errors:?}");

            let mut registry = ExtensionRegistry::<TestDb>::default();
            ExtensionManager::register_all(&mut registry, &loaded).unwrap();

            let db = Arc::new(TestDb { text: String::new() });
            let diagnostics = registry.diagnostics(
                ctx(Arc::clone(&db)),
                DiagnosticParams { file: FileId::from_raw(1) },
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
    }
}
