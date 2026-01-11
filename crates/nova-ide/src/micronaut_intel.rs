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

#[derive(Clone)]
struct CachedAnalysis {
    signature: u64,
    analysis: Option<Arc<AnalysisResult>>,
}

static ANALYSIS_CACHE: Lazy<Mutex<HashMap<PathBuf, CachedAnalysis>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static PROJECT_CONFIG_CACHE: Lazy<Mutex<HashMap<PathBuf, Option<Arc<ProjectConfig>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn analysis_for_file(db: &dyn Database, file: FileId) -> Option<Arc<AnalysisResult>> {
    let path = db.file_path(file)?;
    let root = project_root_for_path(path);

    let (sources, config_files, signature) = gather_workspace_inputs(db, &root);

    // Fast path: cache hit.
    {
        let cache = ANALYSIS_CACHE
            .lock()
            .expect("micronaut analysis cache poisoned");
        if let Some(entry) = cache.get(&root).filter(|e| e.signature == signature) {
            return entry.analysis.clone();
        }
    }

    let config = project_config_for_root(&root);
    let applicable = is_applicable(&config, &sources);
    let analysis = if applicable {
        Some(Arc::new(
            nova_framework_micronaut::analyze_sources_with_config(&sources, &config_files),
        ))
    } else {
        None
    };

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

fn project_config_for_root(root: &Path) -> Option<Arc<ProjectConfig>> {
    {
        let cache = PROJECT_CONFIG_CACHE
            .lock()
            .expect("project config cache poisoned");
        if let Some(entry) = cache.get(root) {
            return entry.clone();
        }
    }

    let loaded = nova_project::load_project(root).ok().map(Arc::new);
    let mut cache = PROJECT_CONFIG_CACHE
        .lock()
        .expect("project config cache poisoned");
    cache.insert(root.to_path_buf(), loaded.clone());
    loaded
}

fn project_root_for_path(path: &Path) -> PathBuf {
    if path.exists() {
        if let Some(root) = find_build_root(path) {
            return root;
        }
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

fn find_build_root(path: &Path) -> Option<PathBuf> {
    const MARKERS: &[&str] = &[
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        "WORKSPACE",
        "WORKSPACE.bazel",
        "MODULE.bazel",
    ];

    let mut dir = if path.is_dir() { path } else { path.parent()? };

    loop {
        if MARKERS.iter().any(|marker| dir.join(marker).is_file()) {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

fn gather_workspace_inputs(
    db: &dyn Database,
    root: &Path,
) -> (Vec<JavaSource>, Vec<ConfigFile>, u64) {
    let mut paths: Vec<(String, FileId)> = db
        .all_file_ids()
        .into_iter()
        .filter_map(|id| {
            let path = db.file_path(id)?;
            if !path.starts_with(root) {
                return None;
            }
            Some((path.to_string_lossy().to_string(), id))
        })
        .collect();
    paths.sort_by(|a, b| a.0.cmp(&b.0));

    let mut sources = Vec::new();
    let mut config_files = Vec::new();

    let mut hasher = DefaultHasher::new();
    for (path_string, id) in paths {
        let Some(path) = db.file_path(id) else {
            continue;
        };
        let is_java = path.extension().and_then(|e| e.to_str()) == Some("java");
        let config_kind = match path.file_name().and_then(|n| n.to_str()) {
            Some("application.properties") => {
                Some("properties")
            }
            Some("application.yml") | Some("application.yaml") => {
                Some("yaml")
            }
            _ => None,
        };

        if !is_java && config_kind.is_none() {
            continue;
        }

        let text = db.file_content(id).to_string();

        path_string.hash(&mut hasher);
        text.hash(&mut hasher);

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

    (sources, config_files, hasher.finish())
}
