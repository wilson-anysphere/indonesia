use std::path::Path;

use walkdir::WalkDir;

/// Best-effort heuristic for deciding whether `root` is a *safe* Java project root.
///
/// This is used to guard potentially expensive filesystem walks (e.g. refactor snapshots and
/// prompt-context enrichment). The heuristic is intentionally conservative: when it returns
/// `false`, callers should fall back to single-file (focus + overlays) behavior.
pub(crate) fn looks_like_project_root(root: &Path) -> bool {
    if !root.is_dir() {
        return false;
    }

    // Prefer explicit build-system / Nova workspace markers.
    const MARKERS: &[&str] = &[
        // Nova workspace config
        ".nova",
        "nova.toml",
        ".nova.toml",
        "nova.config.toml",
        // Maven / Gradle
        "pom.xml",
        "mvnw",
        "mvnw.cmd",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        "gradlew",
        "gradlew.bat",
        // Bazel
        "WORKSPACE",
        "WORKSPACE.bazel",
        "MODULE.bazel",
    ];

    if MARKERS.iter().any(|marker| root.join(marker).exists())
        // Conventional Java source layout.
        || root.join("src").join("main").join("java").is_dir()
        || root.join("src").join("test").join("java").is_dir()
    {
        return true;
    }

    let src = root.join("src");
    if !src.is_dir() {
        return false;
    }

    // "Simple" projects: accept a `src/` tree that actually contains Java source files
    // near the top-level. Cap the walk to keep this check cheap even for large trees.
    let mut inspected = 0usize;
    for entry in WalkDir::new(&src).max_depth(4) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        inspected += 1;
        if inspected > 2_000 {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
        {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn root_with_no_markers_is_not_project_root() {
        let temp = TempDir::new().unwrap();
        assert!(!looks_like_project_root(temp.path()));
    }

    #[test]
    fn src_without_java_is_not_project_root() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        assert!(!looks_like_project_root(temp.path()));
    }

    #[test]
    fn src_main_java_is_project_root() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("src").join("main").join("java")).unwrap();
        assert!(looks_like_project_root(temp.path()));
    }

    #[test]
    fn src_with_java_file_is_project_root() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Main.java"), "class Main {}").unwrap();
        assert!(looks_like_project_root(temp.path()));
    }
}
