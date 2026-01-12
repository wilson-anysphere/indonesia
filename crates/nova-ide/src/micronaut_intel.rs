use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use nova_db::{Database, FileId};
use nova_framework_micronaut::{is_micronaut_applicable, is_micronaut_applicable_with_classpath};
use nova_framework_micronaut::{AnalysisResult, ConfigFile, JavaSource};
use nova_project::ProjectConfig;
use nova_scheduler::CancellationToken;

use crate::framework_cache;

pub(crate) fn may_have_micronaut_diagnostics(text: &str) -> bool {
    // Avoid computing the full workspace Micronaut analysis for Java files that cannot possibly
    // produce Micronaut diagnostics. Today the Micronaut analyzer only emits:
    // - DI diagnostics on `@Inject` injection points and `@Bean` factory method parameters.
    // - Validation diagnostics on common Bean Validation annotations (e.g. `@NotNull`).
    const NEEDLES: &[&str] = &[
        // Dependency injection.
        "Inject",
        "Bean",
        "Factory",
        // Bean Validation (see `nova-framework-micronaut/src/validation.rs`).
        "NotNull",
        "NotBlank",
        "Email",
        "Min",
        "Max",
        "Positive",
        "PositiveOrZero",
        "Negative",
        "NegativeOrZero",
        "DecimalMin",
        "DecimalMax",
    ];

    NEEDLES.iter().any(|needle| text.contains(needle))
}

#[derive(Clone)]
struct CachedAnalysis {
    signature: u64,
    analysis: Option<Arc<AnalysisResult>>,
}

static ANALYSIS_CACHE: Lazy<Mutex<HashMap<PathBuf, CachedAnalysis>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn analysis_for_file<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
) -> Option<Arc<AnalysisResult>> {
    let cancel = CancellationToken::new();
    analysis_for_file_with_cancel(db, file, &cancel)
}

pub(crate) fn analysis_for_file_with_cancel<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    cancel: &CancellationToken,
) -> Option<Arc<AnalysisResult>> {
    if cancel.is_cancelled() {
        return None;
    }
    let path = db.file_path(file)?;
    let root = project_root_for_path(path);

    let config = framework_cache::project_config(&root);
    let config_signature = config
        .as_deref()
        .map(project_config_signature)
        .unwrap_or_default();

    // Computing the full `JavaSource`/`ConfigFile` inputs requires cloning the
    // entire workspace's file text. Use a lightweight signature first so cache
    // hits avoid that work.
    let base_signature = match workspace_signature(db, &root, cancel) {
        Some(sig) => sig,
        None => {
            return ANALYSIS_CACHE
                .lock()
                .expect("micronaut analysis cache poisoned")
                .get(&root)
                .and_then(|entry| entry.analysis.clone());
        }
    };
    let signature = combined_signature(base_signature, config_signature);

    // Fast path: cache hit.
    {
        let cache = ANALYSIS_CACHE
            .lock()
            .expect("micronaut analysis cache poisoned");
        if let Some(entry) = cache.get(&root).filter(|e| e.signature == signature) {
            return entry.analysis.clone();
        }
    }

    if cancel.is_cancelled() {
        return ANALYSIS_CACHE
            .lock()
            .expect("micronaut analysis cache poisoned")
            .get(&root)
            .and_then(|entry| entry.analysis.clone());
    }

    let (sources, config_files) = match gather_workspace_inputs(db, &root, cancel) {
        Some(inputs) => inputs,
        None => {
            return ANALYSIS_CACHE
                .lock()
                .expect("micronaut analysis cache poisoned")
                .get(&root)
                .and_then(|entry| entry.analysis.clone());
        }
    };
    let applicable = is_applicable(&config, &sources);
    let analysis = if applicable {
        Some(Arc::new(
            nova_framework_micronaut::analyze_sources_with_config(&sources, &config_files),
        ))
    } else {
        None
    };

    if !cancel.is_cancelled() {
        let mut cache = ANALYSIS_CACHE
            .lock()
            .expect("micronaut analysis cache poisoned");
        cache.insert(
            root.clone(),
            CachedAnalysis {
                signature,
                analysis: analysis.clone(),
            },
        );
    }

    analysis
}

fn is_applicable(config: &Option<Arc<ProjectConfig>>, sources: &[JavaSource]) -> bool {
    let source_texts: Vec<&str> = sources.iter().map(|s| s.text.as_str()).collect();

    if let Some(cfg) = config.as_deref() {
        let dep_strings: Vec<String> = cfg
            .dependencies
            .iter()
            .map(|d| format!("{}:{}", d.group_id, d.artifact_id))
            .collect();
        let dep_refs: Vec<&str> = dep_strings.iter().map(|s| s.as_str()).collect();

        let classpath_entries: Vec<&Path> = cfg
            .classpath
            .iter()
            .map(|e| e.path.as_path())
            .chain(cfg.module_path.iter().map(|e| e.path.as_path()))
            .collect();

        if !classpath_entries.is_empty() {
            return is_micronaut_applicable_with_classpath(
                &dep_refs,
                classpath_entries.as_slice(),
                &source_texts,
            );
        }

        return is_micronaut_applicable(&dep_refs, &source_texts);
    }

    // Fallback heuristic: scan sources for Micronaut package prefixes.
    sources.iter().any(|src| src.text.contains("io.micronaut."))
}

fn project_root_for_path(path: &Path) -> PathBuf {
    if path.exists() {
        return framework_cache::project_root_for_path(path);
    }

    // Heuristic fallback (works for in-memory DB fixtures): `.../src/...` -> project is parent of `src`.
    if let Some(parent) = path.ancestors().find_map(|ancestor| {
        if ancestor
            .file_name()
            .is_some_and(|n| n == std::ffi::OsStr::new("src"))
        {
            ancestor.parent().map(Path::to_path_buf)
        } else {
            None
        }
    }) {
        return parent;
    }

    path.parent().unwrap_or(path).to_path_buf()
}

fn project_config_signature(cfg: &ProjectConfig) -> u64 {
    let mut hasher = DefaultHasher::new();
    for dep in &cfg.dependencies {
        dep.group_id.hash(&mut hasher);
        dep.artifact_id.hash(&mut hasher);
        dep.version.hash(&mut hasher);
        dep.scope.hash(&mut hasher);
        dep.classifier.hash(&mut hasher);
        dep.type_.hash(&mut hasher);
    }
    for entry in cfg
        .classpath
        .iter()
        .chain(cfg.module_path.iter())
        .map(|e| &e.path)
    {
        entry.hash(&mut hasher);
    }
    hasher.finish()
}

fn combined_signature(base: u64, config: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    base.hash(&mut hasher);
    config.hash(&mut hasher);
    hasher.finish()
}

fn workspace_signature<DB: ?Sized + Database>(
    db: &DB,
    root: &Path,
    cancel: &CancellationToken,
) -> Option<u64> {
    let mut paths: Vec<(PathBuf, FileId)> = db
        .all_file_ids()
        .into_iter()
        .filter_map(|id| {
            let path = db.file_path(id)?;
            if !path.starts_with(root) {
                return None;
            }
            Some((path.to_path_buf(), id))
        })
        .collect();
    paths.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = DefaultHasher::new();
    for (path, id) in paths {
        if cancel.is_cancelled() {
            return None;
        }
        let is_java = path.extension().and_then(|e| e.to_str()) == Some("java");
        let config_kind = if framework_cache::is_application_properties(&path) {
            Some("properties")
        } else if framework_cache::is_application_yaml(&path) {
            Some("yaml")
        } else {
            None
        };

        if !is_java && config_kind.is_none() {
            continue;
        }

        path.hash(&mut hasher);
        let text = db.file_content(id);
        text.len().hash(&mut hasher);
        text.as_ptr().hash(&mut hasher);
    }

    Some(hasher.finish())
}

fn gather_workspace_inputs<DB: ?Sized + Database>(
    db: &DB,
    root: &Path,
    cancel: &CancellationToken,
) -> Option<(Vec<JavaSource>, Vec<ConfigFile>)> {
    let mut paths: Vec<(PathBuf, FileId)> = db
        .all_file_ids()
        .into_iter()
        .filter_map(|id| {
            let path = db.file_path(id)?;
            if !path.starts_with(root) {
                return None;
            }
            Some((path.to_path_buf(), id))
        })
        .collect();
    paths.sort_by(|a, b| a.0.cmp(&b.0));

    let mut sources = Vec::new();
    let mut config_files = Vec::new();

    for (path, id) in paths {
        if cancel.is_cancelled() {
            return None;
        }
        let is_java = path.extension().and_then(|e| e.to_str()) == Some("java");
        let config_kind = if framework_cache::is_application_properties(&path) {
            Some("properties")
        } else if framework_cache::is_application_yaml(&path) {
            Some("yaml")
        } else {
            None
        };

        if !is_java && config_kind.is_none() {
            continue;
        }

        let path_string = path.to_string_lossy().to_string();
        let text = db.file_content(id).to_string();

        if is_java {
            sources.push(JavaSource::new(path_string, text));
            continue;
        }

        match config_kind {
            Some("properties") => config_files.push(ConfigFile::properties(path_string, text)),
            Some("yaml") => config_files.push(ConfigFile::yaml(path_string, text)),
            _ => {}
        }
    }

    Some((sources, config_files))
}
