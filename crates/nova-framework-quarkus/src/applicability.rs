//! Quarkus applicability detection.

use std::fs::File;
use std::path::Path;

use nova_core::ProjectId;
use nova_framework::Database;
use zip::ZipArchive;

/// Returns `true` when Quarkus is likely in use.
pub fn is_quarkus_applicable(dependencies: &[&str], sources: &[&str]) -> bool {
    if dependencies.iter().any(|dep| dep.contains("io.quarkus")) {
        return true;
    }

    sources.iter().any(|src| {
        src.contains("io.quarkus.")
            || src.contains("@QuarkusMain")
            || src.contains("@io.quarkus.runtime.annotations.QuarkusMain")
            || src.contains("quarkus.")
    })
}

/// Returns `true` when Quarkus is likely in use, considering dependencies,
/// sources, and the classpath.
pub fn is_quarkus_applicable_with_classpath(
    dependencies: &[&str],
    classpath: &[&Path],
    sources: &[&str],
) -> bool {
    if is_quarkus_applicable(dependencies, sources) {
        return true;
    }

    classpath.iter().any(|entry| classpath_entry_has_quarkus(entry))
}

/// Applicability check wired into the `nova-framework` database abstraction.
pub fn is_quarkus_applicable_with_db(db: &dyn Database, project: ProjectId) -> bool {
    const GROUP: &str = "io.quarkus";
    const COMMON_ARTIFACTS: &[&str] = &[
        "quarkus-arc",
        "quarkus-resteasy",
        "quarkus-resteasy-reactive",
        "quarkus-resteasy-jackson",
        "quarkus-resteasy-reactive-jackson",
        "quarkus-rest",
        "quarkus-rest-jackson",
        "quarkus-smallrye-config",
    ];

    if COMMON_ARTIFACTS
        .iter()
        .any(|artifact| db.has_dependency(project, GROUP, artifact))
    {
        return true;
    }

    db.has_class_on_classpath_prefix(project, "io.quarkus.")
        || db.has_class_on_classpath_prefix(project, "io/quarkus/")
}

fn classpath_entry_has_quarkus(path: &Path) -> bool {
    if path.is_dir() {
        return dir_has_quarkus(path);
    }
    if path.is_file() {
        return archive_has_quarkus(path);
    }
    false
}

fn dir_has_quarkus(root: &Path) -> bool {
    const MARKERS: &[&str] = &[
        "io/quarkus/runtime/Quarkus.class",
        "io/quarkus/arc/Arc.class",
    ];

    MARKERS.iter().any(|rel| root.join(rel).exists())
}

fn archive_has_quarkus(path: &Path) -> bool {
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
        "io/quarkus/runtime/Quarkus.class",
        "io/quarkus/arc/Arc.class",
        "classes/io/quarkus/runtime/Quarkus.class",
        "classes/io/quarkus/arc/Arc.class",
    ];

    MARKERS.iter().any(|name| zip.by_name(name).is_ok())
}
