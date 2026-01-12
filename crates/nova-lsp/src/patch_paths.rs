use std::path::{Component, Path, PathBuf};

use lsp_types::Uri;

/// Derive `(root_uri, file_rel)` for patch-based AI edits.
///
/// Patch paths must be workspace-relative and use forward slashes (`/`). LSP
/// requests typically provide file URIs, but the server may not have a stable
/// workspace root (e.g. `initialize.rootUri` is missing). This helper produces
/// a stable URI base and a relative file key that can be fed into:
/// - [`crate::workspace_edit::join_uri`] for LSP edits
/// - `nova_ai::workspace::VirtualWorkspace` keys and patch safety checks
///
/// Behavior:
/// - If `project_root` is `Some` and `doc_path` is under it, the returned
///   `root_uri` is the project root directory URI and `file_rel` is the path
///   relative to it.
/// - Otherwise, fall back to the document's parent directory URI and the
///   document's basename.
///
/// The returned `file_rel` always uses forward slashes.
pub fn patch_root_uri_and_file_rel(project_root: Option<&Path>, doc_path: &Path) -> (Uri, String) {
    if let Some(root) = project_root {
        if let Ok(rel) = doc_path.strip_prefix(root) {
            if let Some(file_rel) = path_to_forward_slash_rel(rel) {
                return (uri_for_path(root), file_rel);
            }
        }
    }

    let parent = doc_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = doc_path
        .file_name()
        .unwrap_or_else(|| doc_path.as_os_str())
        .to_string_lossy()
        .to_string();

    (uri_for_path(parent), file_name)
}

fn uri_for_path(path: &Path) -> Uri {
    // Prefer Nova's own file URI encoding so we round-trip with
    // `nova_core::file_uri_to_path` (and match `workspace_edit::join_uri`).
    let abs = match nova_core::AbsPathBuf::new(path.to_path_buf()) {
        Ok(abs) => abs,
        Err(_) => {
            // Best-effort: make relative paths absolute to avoid panicking in
            // downstream `.parse::<Uri>()`.
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            nova_core::AbsPathBuf::new(cwd.join(path))
                .expect("failed to create an absolute path for URI conversion")
        }
    };

    nova_core::path_to_file_uri(&abs)
        .expect("failed to convert path to file URI")
        .parse()
        .expect("file URI should parse as lsp_types::Uri")
}

fn path_to_forward_slash_rel(path: &Path) -> Option<String> {
    // Only accept canonical relative paths; patch safety rejects `.`/`..`,
    // absolute paths, and backslashes. If we see any non-normal components,
    // fail closed and let callers fall back to basename mode.
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(seg) => parts.push(seg.to_string_lossy().to_string()),
            // Skip `.` segments.
            Component::CurDir => {}
            // Reject any other component kinds (e.g. `..`, prefixes).
            _ => return None,
        }
    }

    if parts.is_empty() {
        return None;
    }

    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    fn uri_for_abs_path(path: &Path) -> Uri {
        let abs = nova_core::AbsPathBuf::new(path.to_path_buf()).expect("absolute path");
        nova_core::path_to_file_uri(&abs)
            .expect("path to URI")
            .parse()
            .expect("valid URI")
    }

    #[test]
    fn under_root_returns_forward_slash_relative_path() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("workspace");
        let doc_path = root.join("src").join("Main.java");

        let (root_uri, file_rel) = patch_root_uri_and_file_rel(Some(&root), &doc_path);

        assert_eq!(file_rel, "src/Main.java");
        assert_eq!(root_uri, uri_for_abs_path(&root));

        let joined = crate::workspace_edit::join_uri(&root_uri, Path::new(&file_rel));
        assert_eq!(joined, uri_for_abs_path(&doc_path));
    }

    #[test]
    fn outside_root_falls_back_to_basename() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("workspace");
        let external_dir = temp.path().join("other");
        let doc_path = external_dir.join("Main.java");

        let (root_uri, file_rel) = patch_root_uri_and_file_rel(Some(&root), &doc_path);

        assert_eq!(file_rel, "Main.java");
        assert_eq!(root_uri, uri_for_abs_path(&external_dir));

        let joined = crate::workspace_edit::join_uri(&root_uri, Path::new(&file_rel));
        assert_eq!(joined, uri_for_abs_path(&doc_path));
    }

    #[test]
    fn spaces_and_percent_encoded_paths_round_trip() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("My Workspace");
        let doc_path = root.join("src").join("My File.java");

        // Simulate incoming LSP URI with percent-encoding.
        let doc_uri = uri_for_abs_path(&doc_path);
        assert!(
            doc_uri.as_str().contains("%20"),
            "expected percent-encoded URI, got {}",
            doc_uri.as_str()
        );

        let decoded_doc_path = nova_core::file_uri_to_path(doc_uri.as_str())
            .expect("decode doc URI")
            .into_path_buf();

        let (root_uri, file_rel) = patch_root_uri_and_file_rel(Some(&root), &decoded_doc_path);

        assert_eq!(file_rel, "src/My File.java");
        assert!(
            root_uri.as_str().contains("%20"),
            "expected percent-encoded root URI, got {}",
            root_uri.as_str()
        );

        let joined = crate::workspace_edit::join_uri(&root_uri, Path::new(&file_rel));
        assert_eq!(joined, doc_uri);
    }
}

