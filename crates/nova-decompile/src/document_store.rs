use crate::{SymbolKey, SymbolRange};
use nova_cache::{deps_cache_dir, CacheConfig, CacheError, Fingerprint};
#[cfg(not(unix))]
use nova_cache::atomic_write;
use nova_core::{Position, Range};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Persistent, content-addressed store for canonical ADR0006 decompiled virtual documents.
///
/// Canonical decompiled URIs have the form:
/// `nova:///decompiled/<content-hash>/<binary-name>.java`.
///
/// This store persists the *rendered* decompiled text keyed by the same `(content_hash,
/// binary_name)` segments so clients (e.g. `nova-lsp`) can warm-start decompiled buffers without
/// recomputing them.
///
/// ## On-disk layout
///
/// By default (`from_env`), documents are stored under Nova's global dependency cache:
/// `<cache_root>/deps/decompiled/<hash>/<safe-stem>.java`.
///
/// `safe-stem` is a SHA-256 hex digest of the document's `binary_name`. This ensures the store is
/// robust to Windows-invalid filename characters and reserved device names (e.g. `CON`, `NUL`),
/// while keeping the external key as `(content_hash, binary_name)`.
#[derive(Debug, Clone)]
pub struct DecompiledDocumentStore {
    root: PathBuf,
}

/// Best-effort policy for garbage-collecting decompiled virtual documents stored on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecompiledStoreGcPolicy {
    /// Maximum total disk usage allowed for decompiled documents (bytes).
    pub max_total_bytes: u64,
    /// Optional maximum age for decompiled documents (milliseconds).
    ///
    /// Entries older than `now - max_age_ms` are deleted first.
    pub max_age_ms: Option<u64>,
}

/// Summary of a decompiled-store GC pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecompiledStoreGcReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub deleted_files: usize,
    pub deleted_bytes: u64,
}

impl DecompiledDocumentStore {
    /// Construct the store using Nova's default cache location.
    ///
    /// This uses [`CacheConfig::from_env`] (respecting `NOVA_CACHE_DIR`) and stores documents
    /// under the global deps cache (`.../deps/decompiled`).
    pub fn from_env() -> Result<Self, CacheError> {
        let config = CacheConfig::from_env();
        let deps_root = deps_cache_dir(&config)?;
        Ok(Self::new(deps_root.join("decompiled")))
    }

    /// Construct a store rooted at `root`.
    ///
    /// This is primarily intended for tests; callers should usually prefer [`Self::from_env`].
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Persist decompiled source text for a canonical `(content_hash, binary_name)` identity.
    ///
    /// Writes are atomic and safe under concurrent writers.
    pub fn store_text(
        &self,
        content_hash: &str,
        binary_name: &str,
        text: &str,
    ) -> Result<(), CacheError> {
        let path = self.path_for(content_hash, binary_name)?;
        ensure_dir_safe(&self.root)?;
        if let Some(parent) = path.parent() {
            ensure_dir_safe(parent)?;
        }
        atomic_write_store_file(&path, text.as_bytes())
    }

    /// Persist decompiled source text *and* decompiler symbol mappings.
    ///
    /// The decompiled text is written to `<safe-stem>.java` (same as [`Self::store_text`]).
    /// Mappings are written to a JSON sidecar file next to it:
    /// `<safe-stem>.meta.json`.
    pub fn store_document(
        &self,
        content_hash: &str,
        binary_name: &str,
        text: &str,
        mappings: &[SymbolRange],
    ) -> Result<(), CacheError> {
        self.store_text(content_hash, binary_name, text)?;

        let meta_path = self.meta_path_for(content_hash, binary_name)?;
        if let Some(parent) = meta_path.parent() {
            ensure_dir_safe(parent)?;
        }
        let stored = StoredDecompiledMappings::from_mappings(mappings);
        let bytes = serde_json::to_vec(&stored)?;
        atomic_write_store_file(&meta_path, &bytes)
    }

    /// Load previously-persisted decompiled source text for a canonical `(content_hash,
    /// binary_name)` identity.
    ///
    /// This is best-effort: missing files or obvious corruption (non-file, symlink, invalid
    /// UTF-8) return `Ok(None)`.
    pub fn load_text(
        &self,
        content_hash: &str,
        binary_name: &str,
    ) -> Result<Option<String>, CacheError> {
        // Treat invalid keys as a cache miss (best-effort store).
        let Ok(path) = self.path_for(content_hash, binary_name) else {
            return Ok(None);
        };

        let Some(bytes) = read_cache_file_bytes(&path)? else {
            return Ok(None);
        };

        match String::from_utf8(bytes) {
            Ok(text) => Ok(Some(text)),
            Err(_) => {
                remove_corrupt_store_leaf_best_effort(&path);
                Ok(None)
            }
        }
    }

    /// Load previously-persisted decompiled source text and symbol mappings for a canonical
    /// `(content_hash, binary_name)` identity.
    ///
    /// This returns `Ok(None)` when:
    /// - the stored text file is missing or invalid
    /// - the mapping sidecar file is missing or invalid
    pub fn load_document(
        &self,
        content_hash: &str,
        binary_name: &str,
    ) -> Result<Option<(String, Vec<SymbolRange>)>, CacheError> {
        let Some(text) = self.load_text(content_hash, binary_name)? else {
            return Ok(None);
        };

        // Treat invalid keys as a cache miss (best-effort store).
        let Ok(meta_path) = self.meta_path_for(content_hash, binary_name) else {
            return Ok(None);
        };
        let Some(meta_bytes) = read_cache_file_bytes(&meta_path)? else {
            return Ok(None);
        };

        let stored: StoredDecompiledMappings = match serde_json::from_slice(&meta_bytes) {
            Ok(value) => value,
            Err(_) => {
                remove_corrupt_store_leaf_best_effort(&meta_path);
                return Ok(None);
            }
        };

        Ok(Some((text, stored.into_mappings())))
    }

    /// Returns whether the decompiled document exists on disk.
    ///
    /// Invalid `(content_hash, binary_name)` inputs return `false`.
    pub fn exists(&self, content_hash: &str, binary_name: &str) -> bool {
        let Ok(path) = self.path_for(content_hash, binary_name) else {
            return false;
        };

        // Treat any unexpected filesystem state as corruption: delete and report
        // a cache miss.
        if let Ok(meta) = std::fs::symlink_metadata(&self.root) {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                remove_corrupt_path(&self.root);
                return false;
            }
        }

        if let Some(parent) = path.parent() {
            if let Ok(meta) = std::fs::symlink_metadata(parent) {
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    remove_corrupt_path(parent);
                    return false;
                }
            }
        }

        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => return false,
        };

        if meta.file_type().is_symlink() || !meta.is_file() {
            remove_corrupt_store_leaf_best_effort(&path);
            return false;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if meta.nlink() > 1 {
                remove_corrupt_store_leaf_best_effort(&path);
                return false;
            }
        }

        const MAX_DOC_BYTES: u64 = nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64;
        if meta.len() > MAX_DOC_BYTES {
            remove_corrupt_store_leaf_best_effort(&path);
            return false;
        }

        true
    }

    /// Convenience wrapper around [`Self::store_text`] that takes a canonical `nova:///...` URI.
    pub fn store_uri(&self, uri: &str, text: &str) -> Result<(), CacheError> {
        let parsed = crate::parse_decompiled_uri(uri)
            .ok_or_else(|| io::Error::other("invalid decompiled virtual document URI"))?;
        self.store_text(&parsed.content_hash, &parsed.binary_name, text)
    }

    /// Convenience wrapper around [`Self::load_text`] that takes a canonical `nova:///...` URI.
    ///
    /// Invalid URIs return `Ok(None)` (best-effort store).
    pub fn load_uri(&self, uri: &str) -> Result<Option<String>, CacheError> {
        let Some(parsed) = crate::parse_decompiled_uri(uri) else {
            return Ok(None);
        };
        self.load_text(&parsed.content_hash, &parsed.binary_name)
    }

    /// Best-effort garbage collection for decompiled documents stored under this store's root.
    ///
    /// GC operates on all regular files under the store root (e.g. `.java` documents and any
    /// metadata sidecars). Symlinks are ignored and never followed.
    pub fn gc(
        &self,
        policy: &DecompiledStoreGcPolicy,
    ) -> Result<DecompiledStoreGcReport, CacheError> {
        let before_files = enumerate_regular_files(&self.root)?;
        let before_bytes: u64 = before_files
            .iter()
            .fold(0u64, |acc, f| acc.saturating_add(f.size_bytes));

        // Fast path: nothing to do.
        if before_files.is_empty() {
            return Ok(DecompiledStoreGcReport {
                before_bytes: 0,
                after_bytes: 0,
                deleted_files: 0,
                deleted_bytes: 0,
            });
        }

        let mut size_by_path = HashMap::<PathBuf, u64>::with_capacity(before_files.len());
        for entry in &before_files {
            size_by_path.insert(entry.path.clone(), entry.size_bytes);
        }

        let now_ms = nova_cache::now_millis();
        let mut deleted_paths = HashSet::<PathBuf>::new();
        let mut deleted_files = 0usize;
        let mut deleted_bytes = 0u64;
        let mut remaining_estimate = before_bytes;

        // 1) Age-based deletion.
        if let Some(max_age_ms) = policy.max_age_ms {
            for entry in &before_files {
                if !is_older_than(entry.modified_millis, now_ms, max_age_ms) {
                    continue;
                }
                delete_with_companion_best_effort(
                    &self.root,
                    &entry.path,
                    &size_by_path,
                    &mut deleted_paths,
                    &mut deleted_files,
                    &mut deleted_bytes,
                    &mut remaining_estimate,
                );
            }
        }

        // 2) Size-based deletion (oldest first) if still above budget.
        if remaining_estimate > policy.max_total_bytes {
            let mut remaining: Vec<&FileEntry> = before_files
                .iter()
                .filter(|e| !deleted_paths.contains(&e.path))
                .collect();

            remaining.sort_by(|a, b| {
                let a_ts = a.modified_millis.unwrap_or(0);
                let b_ts = b.modified_millis.unwrap_or(0);
                a_ts.cmp(&b_ts).then_with(|| a.path.cmp(&b.path))
            });

            for entry in remaining {
                if remaining_estimate <= policy.max_total_bytes {
                    break;
                }
                delete_with_companion_best_effort(
                    &self.root,
                    &entry.path,
                    &size_by_path,
                    &mut deleted_paths,
                    &mut deleted_files,
                    &mut deleted_bytes,
                    &mut remaining_estimate,
                );
            }
        }

        // Recompute actual post-GC size (best-effort, no follow).
        let after_bytes = enumerate_regular_files(&self.root)?
            .iter()
            .fold(0u64, |acc, f| acc.saturating_add(f.size_bytes));

        Ok(DecompiledStoreGcReport {
            before_bytes,
            after_bytes,
            deleted_files,
            deleted_bytes,
        })
    }

    fn path_for(&self, content_hash: &str, binary_name: &str) -> Result<PathBuf, CacheError> {
        validate_content_hash(content_hash)?;
        validate_binary_name(binary_name)?;
        let safe_stem = safe_binary_name_stem(binary_name);
        Ok(self
            .root
            .join(content_hash)
            .join(format!("{safe_stem}.java")))
    }

    fn meta_path_for(&self, content_hash: &str, binary_name: &str) -> Result<PathBuf, CacheError> {
        validate_content_hash(content_hash)?;
        validate_binary_name(binary_name)?;
        let safe_stem = safe_binary_name_stem(binary_name);
        Ok(self
            .root
            .join(content_hash)
            .join(format!("{safe_stem}.meta.json")))
    }
}

fn safe_binary_name_stem(binary_name: &str) -> Fingerprint {
    // Hash the binary name to produce an on-disk filename component that:
    // - is deterministic for a given `binary_name`
    // - never contains Windows-invalid filename characters (`<>:"/\\|?*`)
    // - never collides with Windows reserved device names (`CON`, `PRN`, `NUL`, ...), since it's a
    //   64-character hex digest.
    Fingerprint::from_bytes(binary_name.as_bytes())
}

#[derive(Debug, Clone)]
struct FileEntry {
    path: PathBuf,
    size_bytes: u64,
    modified_millis: Option<u64>,
}

fn enumerate_regular_files(root: &Path) -> Result<Vec<FileEntry>, CacheError> {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    // Never follow symlinks. If the root isn't a directory, treat as empty.
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    enumerate_regular_files_impl(root, &mut out);
    Ok(out)
}

fn enumerate_regular_files_impl(dir: &Path, out: &mut Vec<FileEntry>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };

        let ft = meta.file_type();
        if ft.is_symlink() {
            continue;
        }

        if meta.is_dir() {
            enumerate_regular_files_impl(&path, out);
            continue;
        }

        if !meta.is_file() {
            continue;
        }

        let modified_millis = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);

        out.push(FileEntry {
            path,
            size_bytes: meta.len(),
            modified_millis,
        });
    }
}

fn is_older_than(modified_millis: Option<u64>, now_ms: u64, max_age_ms: u64) -> bool {
    let Some(modified) = modified_millis else {
        // If we can't determine recency, treat it as stale so GC can clean it up.
        return true;
    };
    now_ms.saturating_sub(modified) > max_age_ms
}

fn delete_with_companion_best_effort(
    root: &Path,
    path: &Path,
    size_by_path: &HashMap<PathBuf, u64>,
    deleted: &mut HashSet<PathBuf>,
    deleted_files: &mut usize,
    deleted_bytes: &mut u64,
    remaining_estimate: &mut u64,
) {
    delete_single_path_best_effort(
        root,
        path,
        size_by_path,
        deleted,
        deleted_files,
        deleted_bytes,
        remaining_estimate,
    );

    let Some(companion) = companion_path(path) else {
        return;
    };
    delete_single_path_best_effort(
        root,
        &companion,
        size_by_path,
        deleted,
        deleted_files,
        deleted_bytes,
        remaining_estimate,
    );
}

fn delete_single_path_best_effort(
    root: &Path,
    path: &Path,
    size_by_path: &HashMap<PathBuf, u64>,
    deleted: &mut HashSet<PathBuf>,
    deleted_files: &mut usize,
    deleted_bytes: &mut u64,
    remaining_estimate: &mut u64,
) {
    if deleted.contains(path) {
        return;
    }

    // Lexical check only; do not follow symlinks.
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel,
        Err(_) => return,
    };

    // Best-effort: avoid deleting anything that isn't strictly under the store root.
    // (`strip_prefix` above is lexical, but we additionally reject any non-normal components.)
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return;
    }

    if !unlink_under_root_best_effort(root, rel) {
        return;
    }

    deleted.insert(path.to_path_buf());
    *deleted_files += 1;

    if let Some(size) = size_by_path.get(path) {
        *deleted_bytes = deleted_bytes.saturating_add(*size);
        *remaining_estimate = remaining_estimate.saturating_sub(*size);
    }
}

fn companion_path(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_str()?;
    if let Some(stem) = file_name.strip_suffix(".java") {
        Some(path.with_file_name(format!("{stem}.meta.json")))
    } else if let Some(stem) = file_name.strip_suffix(".meta.json") {
        Some(path.with_file_name(format!("{stem}.java")))
    } else {
        None
    }
}

fn unlink_under_root_best_effort(root: &Path, rel: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::io::{AsRawFd as _, FromRawFd as _};

        let Ok(root_c) = CString::new(root.as_os_str().as_bytes()) else {
            return false;
        };
        let root_fd = unsafe {
            libc::open(
                root_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if root_fd < 0 {
            let err = io::Error::last_os_error();
            return err.raw_os_error() == Some(libc::ENOENT);
        }
        let mut dir = unsafe { std::fs::File::from_raw_fd(root_fd) };

        let mut components = rel.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(segment) = component else {
                return false;
            };
            let Ok(seg_c) = CString::new(segment.as_bytes()) else {
                return false;
            };

            if components.peek().is_none() {
                // Last component: unlink within current dir.
                let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), seg_c.as_ptr(), 0) };
                if rc == 0 {
                    return true;
                }
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ENOENT) {
                    return true;
                }
                if err.raw_os_error() == Some(libc::EISDIR) {
                    let rc = unsafe {
                        libc::unlinkat(dir.as_raw_fd(), seg_c.as_ptr(), libc::AT_REMOVEDIR)
                    };
                    if rc == 0 {
                        return true;
                    }
                    let err = io::Error::last_os_error();
                    return err.raw_os_error() == Some(libc::ENOENT);
                }
                return false;
            }

            // Intermediate component: open as a directory without following symlinks.
            let child_fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    seg_c.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
            };
            if child_fd < 0 {
                let err = io::Error::last_os_error();
                return err.raw_os_error() == Some(libc::ENOENT);
            }
            dir = unsafe { std::fs::File::from_raw_fd(child_fd) };
        }

        true
    }

    #[cfg(not(unix))]
    {
        let path = root.join(rel);
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }
}

fn read_cache_file_bytes(path: &Path) -> Result<Option<Vec<u8>>, CacheError> {
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let Some(root) = parent.parent() else {
        return Ok(None);
    };

    // Avoid following symlinked directories (e.g. `<root>/<hash>` being replaced
    // with a symlink to an arbitrary directory).
    //
    // Any unexpected filesystem state should degrade to a cache miss.
    let root_meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        remove_corrupt_path(root);
        return Ok(None);
    }

    let parent_meta = match std::fs::symlink_metadata(parent) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
        remove_corrupt_path(parent);
        return Ok(None);
    }

    // Avoid following symlinks out of the cache directory.
    //
    // Any unexpected filesystem state should degrade to a cache miss.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        remove_corrupt_store_leaf_best_effort(path);
        return Ok(None);
    }

    // Cap reads to avoid pathological allocations if the cache is corrupted.
    const MAX_DOC_BYTES: u64 = nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64;
    if meta.len() > MAX_DOC_BYTES {
        remove_corrupt_store_leaf_best_effort(path);
        return Ok(None);
    }

    let file = match open_cache_file_read(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            remove_corrupt_store_leaf_best_effort(path);
            return Ok(None);
        }
    };

    let file_meta = match file.metadata() {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            remove_corrupt_store_leaf_best_effort(path);
            return Ok(None);
        }
    };

    // Validate the opened file (defense-in-depth against TOCTOU swaps between the
    // `symlink_metadata` checks above and the `open`).
    if !file_meta.is_file() {
        remove_corrupt_store_leaf_best_effort(path);
        return Ok(None);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if file_meta.nlink() > 1 {
            remove_corrupt_store_leaf_best_effort(path);
            return Ok(None);
        }
    }

    if file_meta.len() > MAX_DOC_BYTES {
        remove_corrupt_store_leaf_best_effort(path);
        return Ok(None);
    }

    // Use `take` as a defense-in-depth cap against races where a cache file grows
    // after the `symlink_metadata` length check above.
    let mut bytes = Vec::with_capacity(file_meta.len() as usize);
    match file
        .take(MAX_DOC_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
    {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            remove_corrupt_store_leaf_best_effort(path);
            return Ok(None);
        }
    }
    if bytes.len() as u64 > MAX_DOC_BYTES {
        remove_corrupt_store_leaf_best_effort(path);
        return Ok(None);
    }

    Ok(Some(bytes))
}

fn remove_corrupt_store_leaf_best_effort(path: &Path) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::io::{AsRawFd as _, FromRawFd as _};

        let Some(parent) = path.parent() else {
            return;
        };
        let Some(root) = parent.parent() else {
            return;
        };

        let Ok(root_c) = CString::new(root.as_os_str().as_bytes()) else {
            return;
        };
        let root_fd = unsafe {
            libc::open(
                root_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if root_fd < 0 {
            return;
        }
        let root_dir = unsafe { std::fs::File::from_raw_fd(root_fd) };

        let Some(parent_name) = parent.file_name() else {
            return;
        };
        let Ok(parent_c) = CString::new(parent_name.as_bytes()) else {
            return;
        };
        let parent_fd = unsafe {
            libc::openat(
                root_dir.as_raw_fd(),
                parent_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if parent_fd < 0 {
            return;
        }
        let parent_dir = unsafe { std::fs::File::from_raw_fd(parent_fd) };

        let Some(file_name) = path.file_name() else {
            return;
        };
        let Ok(file_c) = CString::new(file_name.as_bytes()) else {
            return;
        };

        let rc = unsafe { libc::unlinkat(parent_dir.as_raw_fd(), file_c.as_ptr(), 0) };
        if rc == 0 {
            return;
        }

        // If it's a directory, try removing it as an (empty) directory.
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EISDIR) {
            let _ = unsafe { libc::unlinkat(parent_dir.as_raw_fd(), file_c.as_ptr(), libc::AT_REMOVEDIR) };
        }

        return;
    }

    #[cfg(not(unix))]
    {
        remove_corrupt_path(path);
    }
}

fn remove_corrupt_path(path: &Path) {
    let meta = std::fs::symlink_metadata(path).ok();

    // Never follow symlinks when deleting "corrupt" cache entries; treat them as
    // plain directory entries to unlink.
    if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
        // On some platforms, directory symlinks/junctions may require `remove_dir`
        // instead of `remove_file`. Try both, but never recurse.
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(path);
        return;
    }

    if meta.as_ref().is_some_and(|m| m.is_dir()) {
        let _ = std::fs::remove_dir_all(path);
        return;
    }

    let _ = std::fs::remove_file(path);
}

fn ensure_dir_safe(path: &Path) -> Result<(), CacheError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };

    if let Some(meta) = meta {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            remove_corrupt_path(path);
        } else {
            return Ok(());
        }
    }

    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        remove_corrupt_path(path);
        return Err(io::Error::other("failed to create safe cache directory").into());
    }

    Ok(())
}

fn open_cache_file_read(path: &Path) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::io::{AsRawFd as _, FromRawFd as _};

        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("cache path has no parent"))?;
        let root = parent
            .parent()
            .ok_or_else(|| io::Error::other("cache path has no store root"))?;

        let root_c = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("cache root path contains NUL"))?;
        let root_fd = unsafe {
            libc::open(
                root_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if root_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let root_dir = unsafe { std::fs::File::from_raw_fd(root_fd) };

        let parent_name = parent
            .file_name()
            .ok_or_else(|| io::Error::other("cache directory has no basename"))?;
        let parent_c = CString::new(parent_name.as_bytes())
            .map_err(|_| io::Error::other("cache directory basename contains NUL"))?;
        let parent_fd = unsafe {
            libc::openat(
                root_dir.as_raw_fd(),
                parent_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if parent_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let parent_dir = unsafe { std::fs::File::from_raw_fd(parent_fd) };

        let file_name = path
            .file_name()
            .ok_or_else(|| io::Error::other("cache file has no filename"))?;
        let file_c = CString::new(file_name.as_bytes())
            .map_err(|_| io::Error::other("filename contains NUL"))?;

        let fd = unsafe {
            libc::openat(
                parent_dir.as_raw_fd(),
                file_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    #[cfg(not(unix))]
    {
        std::fs::File::open(path)
    }
}

#[cfg(unix)]
static STORE_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn atomic_write_store_file(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::io::Write as _;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::io::{AsRawFd as _, FromRawFd as _};
        use std::sync::atomic::Ordering;

        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("cache file has no parent directory"))?;
        let root = parent
            .parent()
            .ok_or_else(|| io::Error::other("cache file has no store root"))?;

        let root_c = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("cache root path contains NUL"))?;
        let root_fd = unsafe {
            libc::open(
                root_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if root_fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let root_dir = unsafe { std::fs::File::from_raw_fd(root_fd) };

        let hash_dir_name = parent
            .file_name()
            .ok_or_else(|| io::Error::other("cache directory has no basename"))?;
        let hash_dir_c = CString::new(hash_dir_name.as_bytes())
            .map_err(|_| io::Error::other("cache directory basename contains NUL"))?;
        let hash_dir_fd = unsafe {
            libc::openat(
                root_dir.as_raw_fd(),
                hash_dir_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if hash_dir_fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let hash_dir = unsafe { std::fs::File::from_raw_fd(hash_dir_fd) };

        let file_name = path
            .file_name()
            .ok_or_else(|| io::Error::other("cache file has no filename"))?;

        let dest_c = CString::new(file_name.as_bytes())
            .map_err(|_| io::Error::other("cache filename contains NUL"))?;

        const MAX_ATTEMPTS: usize = 1024;
        for _ in 0..MAX_ATTEMPTS {
            let pid = std::process::id();
            let counter = STORE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);

            let mut tmp_bytes = file_name.as_bytes().to_vec();
            tmp_bytes.extend_from_slice(format!(".tmp.{pid}.{counter}").as_bytes());
            let tmp_c = CString::new(tmp_bytes)
                .map_err(|_| io::Error::other("tmp cache filename contains NUL"))?;

            let fd = unsafe {
                libc::openat(
                    hash_dir.as_raw_fd(),
                    tmp_c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CLOEXEC
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if fd < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(err.into());
            }

            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);

            let rename_rc = unsafe {
                libc::renameat(
                    hash_dir.as_raw_fd(),
                    tmp_c.as_ptr(),
                    hash_dir.as_raw_fd(),
                    dest_c.as_ptr(),
                )
            };
            if rename_rc < 0 {
                let _ = unsafe { libc::unlinkat(hash_dir.as_raw_fd(), tmp_c.as_ptr(), 0) };
                return Err(io::Error::last_os_error().into());
            }

            // Best-effort directory fsync (mirrors `nova_cache::atomic_write`).
            let _ = hash_dir.sync_all();
            return Ok(());
        }

        return Err(io::Error::other("failed to allocate unique temp file").into());
    }

    #[cfg(not(unix))]
    {
        atomic_write(path, bytes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDecompiledMappings {
    mappings: Vec<StoredSymbolRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSymbolRange {
    symbol: SymbolKey,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

impl StoredDecompiledMappings {
    fn from_mappings(mappings: &[SymbolRange]) -> Self {
        Self {
            mappings: mappings
                .iter()
                .map(|m| StoredSymbolRange {
                    symbol: m.symbol.clone(),
                    start_line: m.range.start.line,
                    start_character: m.range.start.character,
                    end_line: m.range.end.line,
                    end_character: m.range.end.character,
                })
                .collect(),
        }
    }

    fn into_mappings(self) -> Vec<SymbolRange> {
        self.mappings
            .into_iter()
            .map(|m| SymbolRange {
                symbol: m.symbol,
                range: Range::new(
                    Position::new(m.start_line, m.start_character),
                    Position::new(m.end_line, m.end_character),
                ),
            })
            .collect()
    }
}

fn validate_content_hash(content_hash: &str) -> Result<(), CacheError> {
    if content_hash.len() != 64 {
        return Err(io::Error::other("invalid decompiled content hash length").into());
    }

    // Fingerprints are stored/printed as lowercase hex (`nova_cache::Fingerprint`).
    if !content_hash
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err(io::Error::other(
            "invalid decompiled content hash (expected 64 lowercase hex characters)",
        )
        .into());
    }

    Ok(())
}

fn validate_binary_name(binary_name: &str) -> Result<(), CacheError> {
    if binary_name.is_empty() {
        return Err(io::Error::other("invalid decompiled binary name (empty)").into());
    }
    if binary_name.contains('/') || binary_name.contains('\\') {
        return Err(
            io::Error::other("invalid decompiled binary name (contains path separators)").into(),
        );
    }

    // Reject drive prefixes / absolute paths / dot segments.
    let mut components = Path::new(binary_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(io::Error::other("invalid decompiled binary name").into()),
    }
}
