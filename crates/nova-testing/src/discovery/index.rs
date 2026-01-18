use crate::schema::TestItem;
use crate::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    size: u64,
}

impl FileStamp {
    fn from_metadata(metadata: &fs::Metadata, path: &Path, context: &'static str) -> Self {
        Self {
            modified: modified_best_effort(metadata, path, context),
            size: metadata.len(),
        }
    }
}

fn modified_best_effort(
    metadata: &fs::Metadata,
    path: &Path,
    context: &'static str,
) -> Option<SystemTime> {
    match metadata.modified() {
        Ok(time) => Some(time),
        Err(err) => {
            tracing::debug!(
                target = "nova.testing",
                context,
                path = %path.display(),
                error = %err,
                "failed to read file mtime"
            );
            None
        }
    }
}

#[derive(Clone, Debug)]
struct FileEntry {
    stamp: FileStamp,
    tests: Vec<TestItem>,
}

#[derive(Debug)]
pub struct TestDiscoveryIndex {
    workspace_root: PathBuf,
    source_roots: Vec<PathBuf>,
    files: HashMap<PathBuf, FileEntry>,
}

impl TestDiscoveryIndex {
    pub fn new(workspace_root: PathBuf, source_roots: Vec<PathBuf>) -> Self {
        Self {
            workspace_root,
            source_roots,
            files: HashMap::new(),
        }
    }

    pub fn set_source_roots(&mut self, roots: Vec<PathBuf>) {
        self.source_roots = roots;
    }

    pub fn refresh(&mut self) -> Result<()> {
        let current_files = self.enumerate_java_files()?;

        let current_paths: HashSet<PathBuf> = current_files.keys().cloned().collect();
        self.files.retain(|path, _| current_paths.contains(path));

        for (path, stamp) in current_files {
            let needs_parse = self
                .files
                .get(&path)
                .is_none_or(|entry| entry.stamp != stamp);

            if !needs_parse {
                continue;
            }

            // Parse into a temporary value so a failure doesn't invalidate an existing cached entry.
            match super::discover_tests_in_file(&self.workspace_root, &path) {
                Ok(tests) => {
                    self.files.insert(path, FileEntry { stamp, tests });
                }
                Err(_) => {
                    // Best-effort: a single unreadable/invalid file should not abort discovery for
                    // the whole workspace.
                    //
                    // - If we have a cached entry, keep it (do not update the stamp so we'll retry
                    //   parsing on the next refresh).
                    // - If this is a new file, skip it.
                }
            }
        }

        Ok(())
    }

    pub fn tests(&self) -> Vec<TestItem> {
        let mut out: Vec<TestItem> = self
            .files
            .values()
            .flat_map(|entry| entry.tests.iter().cloned())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn enumerate_java_files(&self) -> Result<HashMap<PathBuf, FileStamp>> {
        let mut out = HashMap::new();
        let mut logged_walk_error = false;
        let mut logged_metadata_error = false;

        for root in &self.source_roots {
            if !root.is_dir() {
                continue;
            }

            for entry in WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    if entry.depth() == 0 {
                        return true;
                    }

                    let name = entry.file_name().to_string_lossy();
                    !super::SKIP_DIRS.iter().any(|skip| skip == &name.as_ref())
                })
            {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        if !logged_walk_error {
                            tracing::debug!(
                                target = "nova.testing",
                                root = %root.display(),
                                error = %err,
                                "failed to walk source root while discovering tests"
                            );
                            logged_walk_error = true;
                        }
                        continue;
                    }
                };
                if !entry.file_type().is_file() {
                    continue;
                }

                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("java") {
                    continue;
                }

                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        if err
                            .io_error()
                            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                        {
                            continue;
                        }
                        if !logged_metadata_error {
                            tracing::debug!(
                                target = "nova.testing",
                                path = %path.display(),
                                error = %err,
                                "failed to read metadata while discovering tests"
                            );
                            logged_metadata_error = true;
                        }
                        continue;
                    }
                };
                out.insert(
                    path.to_path_buf(),
                    FileStamp::from_metadata(&metadata, path, "testing.discover_tests"),
                );
            }
        }

        Ok(out)
    }
}
