//! Micronaut applicability detection.

use std::fs::File;
use std::path::Path;

use zip::ZipArchive;

/// Returns `true` when Micronaut is likely in use.
///
/// Heuristics:
/// - dependency coordinate contains `io.micronaut`
/// - source references `io.micronaut.*` or common Micronaut HTTP annotations
pub fn is_micronaut_applicable(dependencies: &[&str], sources: &[&str]) -> bool {
    if dependencies.iter().any(|dep| dep.contains("io.micronaut")) {
        return true;
    }

    sources.iter().any(|src| {
        src.contains("io.micronaut.")
            || src.contains("@Controller")
            || src.contains("@io.micronaut.http.annotation.Controller")
    })
}

/// Returns `true` when Micronaut is likely in use, considering dependencies,
/// sources, and the classpath.
///
/// This best-effort scan checks for Micronaut marker classes inside:
/// - classpath directories
/// - `.jar` archives (zip)
/// - `.jmod` archives (zip, marker under `classes/`)
pub fn is_micronaut_applicable_with_classpath(
    dependencies: &[&str],
    classpath: &[&Path],
    sources: &[&str],
) -> bool {
    if is_micronaut_applicable(dependencies, sources) {
        return true;
    }

    classpath.iter().any(|entry| classpath_entry_has_micronaut(entry))
}

fn classpath_entry_has_micronaut(path: &Path) -> bool {
    if path.is_dir() {
        return dir_has_micronaut(path);
    }
    if path.is_file() {
        return archive_has_micronaut(path);
    }
    false
}

fn dir_has_micronaut(root: &Path) -> bool {
    const MARKERS: &[&str] = &[
        "io/micronaut/http/annotation/Controller.class",
        "io/micronaut/context/annotation/Singleton.class",
    ];

    MARKERS.iter().any(|rel| root.join(rel).exists())
}

fn archive_has_micronaut(path: &Path) -> bool {
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
        "io/micronaut/http/annotation/Controller.class",
        "io/micronaut/context/annotation/Singleton.class",
        "classes/io/micronaut/http/annotation/Controller.class",
        "classes/io/micronaut/context/annotation/Singleton.class",
    ];

    MARKERS.iter().any(|name| zip.by_name(name).is_ok())
}
