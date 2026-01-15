use nova_decompile::DecompiledDocumentStore;
use nova_vfs::{FileSystem, LocalFs, VfsPath};
use std::io;
use std::sync::Arc;

const ENV_DECOMPILED_STORE_GC: &str = "NOVA_DECOMPILED_STORE_GC";
const ENV_DECOMPILED_STORE_MAX_TOTAL_BYTES: &str = "NOVA_DECOMPILED_STORE_MAX_TOTAL_BYTES";
const ENV_DECOMPILED_STORE_MAX_AGE_MS: &str = "NOVA_DECOMPILED_STORE_MAX_AGE_MS";

pub(super) fn gc_decompiled_document_store_best_effort() {
    let enabled = !matches!(
        std::env::var(ENV_DECOMPILED_STORE_GC).as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    );
    if !enabled {
        return;
    }

    const DEFAULT_MAX_TOTAL_BYTES: u64 = 512 * nova_memory::MB;
    const DEFAULT_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000; // 30 days

    let max_total_bytes = std::env::var(ENV_DECOMPILED_STORE_MAX_TOTAL_BYTES)
        .ok()
        .and_then(|value| nova_memory::parse_byte_size(value.trim()).ok())
        .unwrap_or(DEFAULT_MAX_TOTAL_BYTES);

    let max_age_ms = match std::env::var(ENV_DECOMPILED_STORE_MAX_AGE_MS) {
        Ok(value) => value.trim().parse::<u64>().ok(),
        Err(_) => Some(DEFAULT_MAX_AGE_MS),
    };

    let policy = nova_decompile::DecompiledStoreGcPolicy {
        max_total_bytes,
        max_age_ms,
    };

    // Run GC asynchronously so we don't delay LSP initialization. This is best-effort: failures
    // should never prevent the server from starting.
    let _ = std::thread::Builder::new()
        .name("nova-decompiled-doc-gc".to_string())
        .spawn(move || {
            let store = match DecompiledDocumentStore::from_env() {
                Ok(store) => store,
                Err(err) => {
                    tracing::debug!("failed to open decompiled document store for GC: {err}");
                    return;
                }
            };

            match store.gc(&policy) {
                Ok(report) => tracing::debug!(
                    before_bytes = report.before_bytes,
                    after_bytes = report.after_bytes,
                    deleted_files = report.deleted_files,
                    deleted_bytes = report.deleted_bytes,
                    "decompiled document store GC complete"
                ),
                Err(err) => tracing::debug!("decompiled document store GC failed: {err}"),
            }
        });
}

pub(super) fn decompiled_store_from_env_best_effort() -> Arc<DecompiledDocumentStore> {
    match DecompiledDocumentStore::from_env() {
        Ok(store) => Arc::new(store),
        Err(err) => {
            // Best-effort fallback: if we can't resolve the normal cache directory
            // (e.g. missing HOME in a sandbox), fall back to a per-process temp dir.
            let fallback_root = std::env::temp_dir()
                .join(format!("nova-decompiled-docs-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&fallback_root);
            tracing::warn!(
                target = "nova.lsp",
                error = %err,
                fallback = %fallback_root.display(),
                "failed to initialize decompiled document store; using temp directory"
            );
            Arc::new(DecompiledDocumentStore::new(fallback_root))
        }
    }
}

/// LSP-facing filesystem adapter that makes canonical ADR0006 decompiled virtual documents
/// (`nova:///decompiled/<hash>/<binary-name>.java`) readable via [`nova_vfs::Vfs`].
///
/// All non-decompiled paths delegate to [`LocalFs`].
#[derive(Debug, Clone)]
pub(super) struct LspFs {
    base: LocalFs,
    decompiled_store: Arc<DecompiledDocumentStore>,
}

impl LspFs {
    pub(super) fn new(base: LocalFs, decompiled_store: Arc<DecompiledDocumentStore>) -> Self {
        Self {
            base,
            decompiled_store,
        }
    }
}

impl FileSystem for LspFs {
    fn read_bytes(&self, path: &VfsPath) -> io::Result<Vec<u8>> {
        match path {
            VfsPath::Decompiled { .. } => Ok(self.read_to_string(path)?.into_bytes()),
            _ => self.base.read_bytes(path),
        }
    }

    fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
        match path {
            VfsPath::Decompiled {
                content_hash,
                binary_name,
            } => match self.decompiled_store.load_text(content_hash, binary_name) {
                Ok(Some(text)) => Ok(text),
                Ok(None) => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("decompiled document not found: {path}"),
                )),
                Err(err) => Err(io::Error::new(io::ErrorKind::Other, err)),
            },
            _ => self.base.read_to_string(path),
        }
    }

    fn exists(&self, path: &VfsPath) -> bool {
        match path {
            VfsPath::Decompiled {
                content_hash,
                binary_name,
            } => self.decompiled_store.exists(content_hash, binary_name),
            _ => self.base.exists(path),
        }
    }

    fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
        match path {
            VfsPath::Decompiled { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("decompiled document metadata not supported ({path})"),
            )),
            _ => self.base.metadata(path),
        }
    }

    fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
        // Directory listing isn't needed by the LSP today; keep this deliberately small.
        self.base.read_dir(path)
    }
}

