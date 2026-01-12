//! Spring applicability detection.

use std::fs::File;
use std::path::Path;

use nova_build_model::{ClasspathEntry, ProjectConfig};
use zip::ZipArchive;

/// Returns `true` when a project configuration suggests Spring is in use.
///
/// Preference order:
/// 1) Build-tool dependencies (`ProjectConfig.dependencies`)
/// 2) Classpath/module-path scan fallback (`ProjectConfig.classpath`, `ProjectConfig.module_path`)
pub fn is_spring_applicable(config: &ProjectConfig) -> bool {
    if config
        .dependencies
        .iter()
        .any(|d| d.group_id.starts_with("org.springframework"))
    {
        return true;
    }

    // Fallback: scan classpath for `org/springframework/**`.
    config
        .classpath
        .iter()
        .chain(config.module_path.iter())
        .any(|entry| classpath_has_spring(entry))
}

fn classpath_has_spring(entry: &ClasspathEntry) -> bool {
    let path = entry.path.as_path();
    if path.is_dir() {
        return dir_has_spring(path);
    }
    if path.is_file() {
        return archive_has_spring(path);
    }
    false
}

fn dir_has_spring(root: &Path) -> bool {
    // Use a handful of stable marker classes rather than recursively scanning.
    const MARKERS: &[&str] = &[
        "org/springframework/context/ApplicationContext.class",
        "org/springframework/beans/factory/annotation/Autowired.class",
        "org/springframework/stereotype/Component.class",
    ];
    MARKERS.iter().any(|rel| root.join(rel).exists())
}

fn archive_has_spring(path: &Path) -> bool {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
    {
        Some(ext) if ext == "jar" || ext == "jmod" => {}
        _ => return false,
    }

    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut zip = match ZipArchive::new(file) {
        Ok(zip) => zip,
        Err(_) => return false,
    };

    const MARKERS: &[&str] = &[
        "org/springframework/context/ApplicationContext.class",
        "org/springframework/beans/factory/annotation/Autowired.class",
        "org/springframework/stereotype/Component.class",
        // JMODs typically place class files under `classes/`.
        "classes/org/springframework/context/ApplicationContext.class",
        "classes/org/springframework/beans/factory/annotation/Autowired.class",
        "classes/org/springframework/stereotype/Component.class",
    ];

    MARKERS.iter().any(|name| zip.by_name(name).is_ok())
}
