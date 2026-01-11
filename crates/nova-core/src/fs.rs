use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Recursively collect files under `root` that have `extension`.
///
/// Missing directories are treated as empty.
pub fn collect_files_with_extension(root: &Path, extension: &str) -> io::Result<Vec<PathBuf>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };

        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();

            if file_type.is_dir() {
                pending.push(path);
                continue;
            }

            if file_type.is_file() && path.extension().is_some_and(|ext| ext == extension) {
                files.push(path);
            }
        }
    }

    Ok(files)
}

pub fn collect_java_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    collect_files_with_extension(root, "java")
}

pub fn max_modified_time(
    paths: impl IntoIterator<Item = PathBuf>,
) -> io::Result<Option<SystemTime>> {
    let mut max_time = None;

    for path in paths {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let modified = metadata.modified()?;
        max_time = Some(match max_time {
            Some(existing) if existing >= modified => existing,
            _ => modified,
        });
    }

    Ok(max_time)
}
