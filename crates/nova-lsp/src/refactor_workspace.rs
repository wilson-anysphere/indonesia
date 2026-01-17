use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use lsp_types::Uri;
use nova_index::Index;
use nova_project::ProjectError;
use nova_refactor::{FileId, RefactorDatabase, RefactorJavaDatabase};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Error)]
pub enum RefactorWorkspaceSnapshotError {
    #[error("expected a file:// URI, got `{0}`")]
    InvalidFileUri(String),

    #[error("failed to convert `{path}` to a file:// URI: {message}")]
    PathToUri { path: PathBuf, message: String },

    #[error("failed to read `{path}`: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFile {
    pub path: PathBuf,
    pub disk_mtime: Option<SystemTime>,
    pub is_overlay: bool,
}

/// A point-in-time view of the Java files in a workspace, with open-document overlays applied.
///
/// This snapshot is intended for multi-file refactorings. It owns a [`RefactorDatabase`]
/// implementation so downstream refactors can be executed without reaching back into the LSP
/// server's document store.
pub struct RefactorWorkspaceSnapshot {
    project_root: PathBuf,
    files: BTreeMap<FileId, WorkspaceFile>,
    db: RefactorJavaDatabase,
}

impl RefactorWorkspaceSnapshot {
    pub fn project_root_for_uri(uri: &Uri) -> Result<PathBuf, RefactorWorkspaceSnapshotError> {
        let path = path_from_uri(uri)?;
        Ok(crate::find_project_root(&path))
    }

    /// Build a snapshot rooted at the project that contains `uri`.
    ///
    /// Overlay precedence:
    /// 1. If a file is present in `overlays`, that text is used.
    /// 2. Otherwise the file is read from disk.
    pub fn build(
        uri: &Uri,
        overlays: &HashMap<String, Arc<str>>,
    ) -> Result<Self, RefactorWorkspaceSnapshotError> {
        let focus_uri = uri.to_string();
        let focus_path = path_from_uri(uri)?;

        // Reuse Nova's project-root heuristics.
        let project_root = crate::find_project_root(&focus_path);

        // Only scan the filesystem when we have a credible project root.
        //
        // For ad-hoc "file:///Foo.java" documents (common in tests / snippets), `find_project_root`
        // falls back to the filesystem root which would make a recursive scan disastrous.
        let should_scan =
            project_root.parent().is_some() && crate::looks_like_project_root(&project_root);

        let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
        paths.insert(focus_path.clone());

        // Always include relevant overlay files.
        //
        // When `should_scan == false` we intentionally avoid scanning the filesystem (e.g. for
        // ad-hoc `file:///Foo.java` documents where `find_project_root` may be `/`). In that mode
        // we still want multi-file refactors (like rename) to be able to operate across other
        // open Java documents, so we pull those from the overlay map instead of the disk.
        let focus_parent = focus_path.parent();
        for (overlay_uri, _) in overlays {
            let overlay_path = match nova_core::file_uri_to_path(overlay_uri) {
                Ok(path) => path.into_path_buf(),
                Err(err) => {
                    if overlay_uri.ends_with(".java") {
                        tracing::debug!(
                            target = "nova.lsp",
                            uri = overlay_uri.as_str(),
                            error = ?err,
                            "refactor workspace snapshot received non-file overlay uri; skipping"
                        );
                    }
                    continue;
                }
            };

            if !is_java_file(&overlay_path) {
                continue;
            }

            // Best-effort locality: keep the snapshot constrained to the focus file's directory
            // or the computed project root. This avoids dragging in unrelated open files while
            // still making ad-hoc multi-file operations work.
            let shares_parent =
                focus_parent.is_some_and(|parent| overlay_path.parent() == Some(parent));
            let within_project_root = overlay_path.starts_with(&project_root);
            if shares_parent || within_project_root {
                paths.insert(overlay_path);
            }
        }

        if should_scan {
            for path in project_java_files(&project_root) {
                paths.insert(path);
            }

            // Note: overlay files are already included above. We still scan the filesystem here to
            // pick up non-open files on disk.
        }

        let mut files = BTreeMap::new();
        let mut db_files: Vec<(FileId, Arc<str>)> = Vec::new();

        for path in paths {
            if !is_java_file(&path) {
                continue;
            }

            let uri_string = if path == focus_path {
                focus_uri.clone()
            } else {
                match uri_string_for_path(&path) {
                    Ok(uri) => uri,
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            path = %path.display(),
                            error = ?err,
                            "failed to convert refactor workspace path to URI; skipping file"
                        );
                        continue;
                    }
                }
            };

            let (text, is_overlay) = if let Some(text) = overlays.get(&uri_string) {
                (text.clone(), true)
            } else {
                let content = match fs::read_to_string(&path) {
                    Ok(content) => content,
                    Err(source) => {
                        // The active document must be available; other files are best-effort.
                        if path == focus_path {
                            return Err(RefactorWorkspaceSnapshotError::ReadFile {
                                path: path.clone(),
                                source,
                            });
                        }
                        continue;
                    }
                };
                (Arc::<str>::from(content), false)
            };

            let disk_mtime = match fs::metadata(&path).and_then(|m| m.modified()) {
                Ok(mtime) => Some(mtime),
                Err(err) => {
                    if path == focus_path {
                        tracing::debug!(
                            target = "nova.lsp",
                            path = %path.display(),
                            err = %err,
                            "failed to read focus file metadata; skipping mtime tracking"
                        );
                    }
                    None
                }
            };
            let file_id = FileId::new(uri_string);

            files.insert(
                file_id.clone(),
                WorkspaceFile {
                    path,
                    disk_mtime,
                    is_overlay,
                },
            );
            db_files.push((file_id, text));
        }

        let db = RefactorJavaDatabase::new_shared(db_files);

        Ok(Self {
            project_root,
            files,
            db,
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn files(&self) -> &BTreeMap<FileId, WorkspaceFile> {
        &self.files
    }

    pub fn db(&self) -> &RefactorJavaDatabase {
        &self.db
    }

    pub fn refactor_db(&self) -> &dyn RefactorDatabase {
        &self.db
    }

    pub fn is_disk_uptodate(&self) -> bool {
        for (_file, meta) in &self.files {
            let Some(expected) = meta.disk_mtime else {
                continue;
            };

            let current = match fs::metadata(&meta.path).and_then(|m| m.modified()) {
                Ok(current) => current,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return false;
                }
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        path = %meta.path.display(),
                        err = %err,
                        "failed to stat workspace file; treating snapshot as out-of-date"
                    );
                    return false;
                }
            };
            if current != expected {
                return false;
            }
        }
        true
    }

    /// Produce the file map expected by [`nova_index::Index`].
    pub fn files_for_index(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for file in self.files.keys() {
            if let Some(text) = self.db.file_text(file) {
                out.insert(file.0.clone(), text.to_string());
            }
        }
        out
    }

    /// Produce the file map expected by move refactors (`PathBuf` keys).
    pub fn files_for_move_refactors(&self) -> BTreeMap<PathBuf, String> {
        let mut out = BTreeMap::new();
        for (file, meta) in &self.files {
            if let Some(text) = self.db.file_text(file) {
                out.insert(meta.path.clone(), text.to_string());
            }
        }
        out
    }

    pub fn build_index(&self) -> Index {
        Index::new(self.files_for_index())
    }
}

fn path_from_uri(uri: &Uri) -> Result<PathBuf, RefactorWorkspaceSnapshotError> {
    nova_core::file_uri_to_path(uri.as_str())
        .map(|p| p.into_path_buf())
        .map_err(|_| RefactorWorkspaceSnapshotError::InvalidFileUri(uri.to_string()))
}

fn uri_string_for_path(path: &Path) -> Result<String, RefactorWorkspaceSnapshotError> {
    // Prefer Nova's URI encoding so we round-trip with `nova_core::file_uri_to_path`.
    let abs = nova_core::AbsPathBuf::new(path.to_path_buf()).map_err(|_| {
        RefactorWorkspaceSnapshotError::PathToUri {
            path: path.to_path_buf(),
            message: "path is not absolute".to_string(),
        }
    })?;
    nova_core::path_to_file_uri(&abs).map_err(|err| RefactorWorkspaceSnapshotError::PathToUri {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn project_java_files(project_root: &Path) -> Vec<PathBuf> {
    let mut files = match nova_project::load_project(project_root) {
        Ok(config) => {
            let mut files = Vec::new();
            for root in config.source_roots {
                files.extend(java_files_in(&root.path));
            }
            files
        }
        Err(ProjectError::UnknownProjectType { .. }) => {
            tracing::debug!(
                target = "nova.lsp",
                project_root = %project_root.display(),
                "unknown project type; falling back to scanning project root for java files"
            );
            // Fall back to scanning the project root.
            java_files_in(project_root)
        }
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                project_root = %project_root.display(),
                error = ?err,
                "failed to load project; falling back to scanning project root for java files"
            );
            // Best-effort: fall back to scanning rather than failing the refactor.
            java_files_in(project_root)
        }
    };
    files.sort();
    files.dedup();
    files
}

fn java_files_in(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut walk_errors = 0u64;
    let mut error_samples = 0u8;
    for entry in WalkDir::new(root).follow_links(true) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                walk_errors += 1;
                if error_samples < 3 {
                    error_samples += 1;
                    tracing::debug!(
                        target = "nova.lsp",
                        root = %root.display(),
                        path = ?err.path(),
                        error = ?err,
                        "failed to walk filesystem while scanning java files; skipping entry"
                    );
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if is_java_file(path) {
            files.push(path.to_path_buf());
        }
    }
    if walk_errors > 0 {
        tracing::debug!(
            target = "nova.lsp",
            root = %root.display(),
            walk_errors,
            "java file scan skipped some entries due to walk errors"
        );
    }
    files.sort();
    files
}

fn is_java_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;

    #[test]
    fn includes_overlay_java_files_when_not_scanning() {
        let dir = tempfile::tempdir().expect("tempdir");

        let focus_path = dir.path().join("Foo.java");
        let other_path = dir.path().join("Bar.java");

        let focus_uri_string = uri_string_for_path(&focus_path).expect("focus uri");
        let other_uri_string = uri_string_for_path(&other_path).expect("other uri");

        let focus_uri: Uri = focus_uri_string.parse().expect("parse focus uri");

        let overlays: HashMap<String, Arc<str>> = HashMap::from([
            (focus_uri_string, Arc::<str>::from("class Foo {}")),
            (other_uri_string, Arc::<str>::from("class Bar {}")),
        ]);

        let snapshot = RefactorWorkspaceSnapshot::build(&focus_uri, &overlays).expect("snapshot");

        let paths: BTreeSet<PathBuf> = snapshot
            .files()
            .values()
            .map(|file| file.path.clone())
            .collect();

        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&focus_path));
        assert!(paths.contains(&other_path));
    }

    #[test]
    fn includes_overlay_java_files_in_subdirs_when_not_scanning() {
        let dir = tempfile::tempdir().expect("tempdir");

        let focus_path = dir.path().join("Foo.java");
        let other_path = dir.path().join("nested").join("Bar.java");

        let focus_uri_string = uri_string_for_path(&focus_path).expect("focus uri");
        let other_uri_string = uri_string_for_path(&other_path).expect("other uri");

        let focus_uri: Uri = focus_uri_string.parse().expect("parse focus uri");

        let overlays: HashMap<String, Arc<str>> = HashMap::from([
            (focus_uri_string, Arc::<str>::from("class Foo {}")),
            (other_uri_string, Arc::<str>::from("class Bar {}")),
        ]);

        let snapshot = RefactorWorkspaceSnapshot::build(&focus_uri, &overlays).expect("snapshot");

        let paths: BTreeSet<PathBuf> = snapshot
            .files()
            .values()
            .map(|file| file.path.clone())
            .collect();

        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&focus_path));
        assert!(paths.contains(&other_path));
    }

    #[test]
    fn does_not_include_disk_java_files_when_not_scanning() {
        let dir = tempfile::tempdir().expect("tempdir");

        let focus_path = dir.path().join("Foo.java");
        let other_path = dir.path().join("Bar.java");
        let disk_path = dir.path().join("OnDisk.java");

        fs::write(&disk_path, "class OnDisk {}").expect("write disk file");

        let focus_uri_string = uri_string_for_path(&focus_path).expect("focus uri");
        let other_uri_string = uri_string_for_path(&other_path).expect("other uri");

        let focus_uri: Uri = focus_uri_string.parse().expect("parse focus uri");

        let overlays: HashMap<String, Arc<str>> = HashMap::from([
            (focus_uri_string, Arc::<str>::from("class Foo {}")),
            (other_uri_string, Arc::<str>::from("class Bar {}")),
        ]);

        let snapshot = RefactorWorkspaceSnapshot::build(&focus_uri, &overlays).expect("snapshot");

        let paths: BTreeSet<PathBuf> = snapshot
            .files()
            .values()
            .map(|file| file.path.clone())
            .collect();

        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&focus_path));
        assert!(paths.contains(&other_path));
        assert!(!paths.contains(&disk_path));
    }
}
