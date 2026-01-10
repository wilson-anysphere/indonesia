//! JPA applicability detection.

use std::fs::File;
use std::path::Path;

use zip::ZipArchive;

/// Returns `true` when JPA is likely in use.
///
/// The upstream Nova project can look at build tooling/classpaths. In this kata
/// repo we use a lightweight heuristic:
///
/// - Known dependency coordinates contain `jakarta.persistence` or
///   `javax.persistence`
/// - Source files reference those packages
pub fn is_jpa_applicable(dependencies: &[&str], sources: &[&str]) -> bool {
    let dep_hit = dependencies.iter().any(|dep| {
        dep.contains("jakarta.persistence")
            || dep.contains("javax.persistence")
            || dep.contains("jakarta.persistence-api")
            || dep.contains("javax.persistence-api")
    });
    if dep_hit {
        return true;
    }

    sources.iter().any(|src| {
        src.contains("jakarta.persistence.")
            || src.contains("javax.persistence.")
            || src.contains("@Entity")
            || src.contains("@javax.persistence.Entity")
            || src.contains("@jakarta.persistence.Entity")
    })
}

/// Returns `true` when JPA is likely in use, considering dependencies, sources,
/// and the classpath.
///
/// This is a best-effort scan that looks for `jakarta.persistence.Entity` /
/// `javax.persistence.Entity` markers inside:
/// - classpath directories
/// - `.jar` archives (zip)
/// - `.jmod` archives (zip, marker under `classes/`)
pub fn is_jpa_applicable_with_classpath(
    dependencies: &[&str],
    classpath: &[&Path],
    sources: &[&str],
) -> bool {
    if is_jpa_applicable(dependencies, sources) {
        return true;
    }

    classpath.iter().any(|entry| classpath_entry_has_jpa(entry))
}

fn classpath_entry_has_jpa(path: &Path) -> bool {
    if path.is_dir() {
        return dir_has_jpa(path);
    }
    if path.is_file() {
        return archive_has_jpa(path);
    }
    false
}

fn dir_has_jpa(root: &Path) -> bool {
    const MARKERS: &[&str] = &[
        "jakarta/persistence/Entity.class",
        "javax/persistence/Entity.class",
    ];

    MARKERS.iter().any(|rel| root.join(rel).exists())
}

fn archive_has_jpa(path: &Path) -> bool {
    // Only attempt zip parsing for the most common archive types.
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
        "jakarta/persistence/Entity.class",
        "javax/persistence/Entity.class",
        // JMODs typically place class files under `classes/`.
        "classes/jakarta/persistence/Entity.class",
        "classes/javax/persistence/Entity.class",
    ];

    MARKERS.iter().any(|name| zip.by_name(name).is_ok())
}
