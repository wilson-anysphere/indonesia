use anyhow::{Context, Result};
use nova_cache::atomic_write;
use nova_refactor::{FileId, FileOp, WorkspaceEdit};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn canonicalize_best_effort(path: &Path, context: &'static str) -> PathBuf {
    match path.canonicalize() {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(err) => {
            tracing::debug!(
                target = "nova.cli",
                context,
                path = %path.display(),
                error = %err,
                "failed to canonicalize path"
            );
            path.to_path_buf()
        }
    }
}

pub(crate) struct JavaWorkspaceSnapshot {
    pub(crate) project_root: PathBuf,
    pub(crate) focus_file: FileId,
    pub(crate) files: BTreeMap<FileId, String>,
}

pub(crate) fn build_java_workspace_snapshot(focus_file: &Path) -> Result<JavaWorkspaceSnapshot> {
    let focus_path = canonicalize_best_effort(focus_file, "refactor_apply.focus_file");
    let focus_dir = focus_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let project_root =
        nova_project::workspace_root(focus_dir).unwrap_or_else(|| focus_dir.to_path_buf());
    let project_root = canonicalize_best_effort(&project_root, "refactor_apply.project_root");

    // Only scan the filesystem when we have a credible project root. For ad-hoc paths,
    // `workspace_root` can fall back to filesystem roots which would make recursive scanning
    // disastrous.
    let should_scan = project_root.parent().is_some() && looks_like_project_root(&project_root);

    let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
    paths.insert(focus_path.clone());

    if should_scan {
        for path in java_files_in(&project_root) {
            paths.insert(path);
        }
    }

    let mut files: BTreeMap<FileId, String> = BTreeMap::new();
    for path in paths {
        if !is_java_file(&path) {
            continue;
        }

        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let id_str = super::path_relative_to(&project_root, &path)
            .unwrap_or_else(|_| super::display_path(&path));
        files.insert(FileId::new(id_str), text);
    }

    let focus_id_str = super::path_relative_to(&project_root, &focus_path)
        .unwrap_or_else(|_| super::display_path(focus_file));
    let focus_file = FileId::new(focus_id_str);

    Ok(JavaWorkspaceSnapshot {
        project_root,
        focus_file,
        files,
    })
}

pub(crate) fn apply_workspace_edit_to_disk(
    project_root: &Path,
    edit: &WorkspaceEdit,
    changed_texts: &BTreeMap<FileId, String>,
) -> Result<()> {
    let mut normalized = edit.clone();
    normalized
        .remap_text_edits_across_renames()
        .map_err(|err| anyhow::anyhow!(err))?;
    normalized.normalize().map_err(|err| anyhow::anyhow!(err))?;

    let mut deletes: Vec<FileId> = Vec::new();

    // Apply renames + creates first. This matches `WorkspaceEdit` normalization ordering
    // (renames, creates, deletes) but also lets us run the more destructive deletes last.
    for op in &normalized.file_ops {
        match op {
            FileOp::Rename { from, to } => {
                let from_path = path_for_file_id(project_root, from);
                let to_path = path_for_file_id(project_root, to);
                rename_file(&from_path, &to_path)
                    .with_context(|| format!("rename {} -> {}", from.0, to.0))?;
            }
            FileOp::Create { file, contents } => {
                let path = path_for_file_id(project_root, file);
                if path.exists() {
                    anyhow::bail!("create destination {} already exists", path.display());
                }
                atomic_write(&path, contents.as_bytes())
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            FileOp::Delete { file } => deletes.push(file.clone()),
        }
    }

    // Apply content changes (text edits) after structural changes but before deletes.
    for (file, text) in changed_texts {
        let path = path_for_file_id(project_root, file);
        atomic_write(&path, text.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    for file in deletes {
        let path = path_for_file_id(project_root, &file);
        fs::remove_file(&path).with_context(|| format!("failed to delete {}", path.display()))?;
    }

    Ok(())
}

fn path_for_file_id(project_root: &Path, file: &FileId) -> PathBuf {
    let candidate = PathBuf::from(&file.0);
    if candidate.is_absolute() {
        candidate
    } else {
        project_root.join(candidate)
    }
}

fn rename_file(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        anyhow::bail!("rename destination {} already exists", dest.display());
    }
    if let Some(parent) = dest.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            fs::copy(src, dest).with_context(|| {
                format!("failed to copy {} to {}", src.display(), dest.display())
            })?;
            fs::remove_file(src).with_context(|| format!("failed to remove {}", src.display()))?;
            Ok(())
        }
        Err(err) => Err(err)
            .with_context(|| format!("failed to rename {} to {}", src.display(), dest.display())),
    }
}

fn java_files_in(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(true) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if is_java_file(path) {
            files.push(path.to_path_buf());
        }
    }
    files.sort();
    files.dedup();
    files
}

fn is_java_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
}

fn looks_like_project_root(root: &Path) -> bool {
    if !root.is_dir() {
        return false;
    }

    // This is intentionally conservative: if we can't find obvious signals that `root`
    // is a project boundary, we avoid a recursive filesystem walk (which could accidentally
    // scan enormous directories like `/` or a home directory).
    const MARKERS: &[&str] = &[
        // VCS
        ".git",
        ".hg",
        ".svn",
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
        // Simple projects
        "src",
        // Nova workspace config
        ".nova",
    ];

    if MARKERS.iter().any(|marker| root.join(marker).exists())
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
        if is_java_file(entry.path()) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_refactor::{apply_workspace_edit, WorkspaceTextEdit};
    use std::collections::BTreeMap;

    fn write_workspace(root: &Path, files: &BTreeMap<FileId, String>) {
        for (file, text) in files {
            let path = root.join(&file.0);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, text).unwrap();
        }
    }

    fn collect_disk_files(root: &Path) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for entry in WalkDir::new(root) {
            let entry = entry.unwrap();
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(root).unwrap();
            out.insert(rel.to_string_lossy().replace('\\', "/"));
        }
        out
    }

    fn compute_changed_texts(
        original: &BTreeMap<FileId, String>,
        edit: &WorkspaceEdit,
    ) -> BTreeMap<FileId, String> {
        let ops_only = WorkspaceEdit {
            file_ops: edit.file_ops.clone(),
            text_edits: Vec::new(),
        };
        let after_ops = apply_workspace_edit(original, &ops_only).unwrap();
        let final_ws = apply_workspace_edit(original, edit).unwrap();

        let mut changed = BTreeMap::new();
        for (file, text) in final_ws {
            if after_ops.get(&file) != Some(&text) {
                changed.insert(file, text);
            }
        }
        changed
    }

    #[test]
    fn create_then_text_edits_writes_final_contents() {
        let temp = assert_fs::TempDir::new().unwrap();
        let root = temp.path();

        let original: BTreeMap<FileId, String> = BTreeMap::new();
        write_workspace(root, &original);

        let created = FileId::new("src/New.java");
        let edit = WorkspaceEdit {
            file_ops: vec![FileOp::Create {
                file: created.clone(),
                contents: "hi".to_string(),
            }],
            text_edits: vec![WorkspaceTextEdit::insert(created.clone(), 2, "!")],
        };

        let changed = compute_changed_texts(&original, &edit);
        apply_workspace_edit_to_disk(root, &edit, &changed).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("src/New.java")).unwrap(),
            "hi!"
        );
        assert_eq!(
            collect_disk_files(root),
            BTreeSet::from(["src/New.java".to_string()])
        );
    }

    #[test]
    fn rename_across_directories_creates_parent_dirs_and_applies_edits() {
        let temp = assert_fs::TempDir::new().unwrap();
        let root = temp.path();

        let src = FileId::new("src/A.java");
        let original = BTreeMap::from([(src.clone(), "class A {}".to_string())]);
        write_workspace(root, &original);

        let dest = FileId::new("src/sub/B.java");
        let edit = WorkspaceEdit {
            file_ops: vec![FileOp::Rename {
                from: src.clone(),
                to: dest.clone(),
            }],
            text_edits: vec![WorkspaceTextEdit::insert(dest.clone(), 0, "// header\n")],
        };

        let changed = compute_changed_texts(&original, &edit);
        apply_workspace_edit_to_disk(root, &edit, &changed).unwrap();

        assert!(!root.join("src/A.java").exists());
        assert_eq!(
            fs::read_to_string(root.join("src/sub/B.java")).unwrap(),
            "// header\nclass A {}"
        );
        assert_eq!(
            collect_disk_files(root),
            BTreeSet::from(["src/sub/B.java".to_string()])
        );
    }

    #[test]
    fn delete_removes_file() {
        let temp = assert_fs::TempDir::new().unwrap();
        let root = temp.path();

        let file = FileId::new("src/A.java");
        let original = BTreeMap::from([(file.clone(), "class A {}".to_string())]);
        write_workspace(root, &original);

        let edit = WorkspaceEdit {
            file_ops: vec![FileOp::Delete { file: file.clone() }],
            text_edits: Vec::new(),
        };
        let changed = compute_changed_texts(&original, &edit);
        apply_workspace_edit_to_disk(root, &edit, &changed).unwrap();

        assert!(!root.join("src/A.java").exists());
        assert!(collect_disk_files(root).is_empty());
    }

    #[test]
    fn rename_errors_when_destination_exists() {
        let temp = assert_fs::TempDir::new().unwrap();
        let root = temp.path();

        let a = FileId::new("src/A.java");
        let b = FileId::new("src/B.java");
        // Simulate a rename destination that exists on disk but was not part of the in-memory
        // workspace snapshot (e.g. non-Java file, or a file outside the scanned roots).
        let original = BTreeMap::from([(a.clone(), "class A {}".to_string())]);
        write_workspace(root, &original);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/B.java"), "class B {}".to_string()).unwrap();

        let edit = WorkspaceEdit {
            file_ops: vec![FileOp::Rename {
                from: a.clone(),
                to: b.clone(),
            }],
            text_edits: Vec::new(),
        };
        let changed = compute_changed_texts(&original, &edit);
        let err = apply_workspace_edit_to_disk(root, &edit, &changed).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "{msg}");

        assert!(root.join("src/A.java").exists());
        assert!(root.join("src/B.java").exists());
        assert_eq!(
            collect_disk_files(root),
            BTreeSet::from(["src/A.java".to_string(), "src/B.java".to_string()])
        );
    }
}
