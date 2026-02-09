use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Namespace string included in the cache key digest.
///
/// Bump this when the key structure or on-disk encoding changes in an
/// incompatible way.
pub(crate) const DISK_CACHE_NAMESPACE_V1: &str = "nova-ai-embeddings-disk-cache-v1";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct EmbeddingCacheKey([u8; 32]);

impl EmbeddingCacheKey {
    pub(crate) fn new(
        namespace: &str,
        backend_id: &str,
        endpoint: &str,
        model: &str,
        input: &[u8],
    ) -> Self {
        let mut hasher = Sha256::new();
        push_field(&mut hasher, namespace.as_bytes());
        push_field(&mut hasher, backend_id.as_bytes());
        push_field(&mut hasher, endpoint.as_bytes());
        push_field(&mut hasher, model.as_bytes());
        push_field(&mut hasher, input);
        let digest = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }

    fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for b in self.0 {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }
}

impl fmt::Debug for EmbeddingCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EmbeddingCacheKey")
            .field(&format_args!("{}…", &self.to_hex()[..8]))
            .finish()
    }
}

fn push_field(hasher: &mut Sha256, value: &[u8]) {
    let len: u64 = value
        .len()
        .try_into()
        .expect("field length should fit in u64");
    hasher.update(len.to_le_bytes());
    hasher.update(value);
}

pub(crate) struct DiskEmbeddingCache {
    root: PathBuf,
}

impl DiskEmbeddingCache {
    pub(crate) fn new(root: PathBuf) -> io::Result<Self> {
        if root.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "model_dir must be non-empty",
            ));
        }
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn entry_path(&self, key: EmbeddingCacheKey) -> PathBuf {
        let hex = key.to_hex();
        let prefix = &hex[..2];
        let rest = &hex[2..];
        self.root
            .join(prefix)
            .join(format!("{rest}.bin"))
    }

    pub(crate) fn load(&self, key: EmbeddingCacheKey) -> io::Result<Option<Vec<f32>>> {
        let path = self.entry_path(key);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };

        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        match decode_embedding(&buf) {
            Some(vec) => Ok(Some(vec)),
            None => {
                // Best-effort cleanup: corrupted entry shouldn't break future lookups.
                let _ = fs::remove_file(&path);
                Ok(None)
            }
        }
    }

    pub(crate) fn store(&self, key: EmbeddingCacheKey, embedding: &[f32]) -> io::Result<()> {
        let path = self.entry_path(key);
        let Some(parent) = path.parent() else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "cache entry missing parent directory",
            ));
        };
        fs::create_dir_all(parent)?;

        let data = encode_embedding(embedding)?;

        // Atomic write: write to a temp file in the same directory, then rename.
        let tmp_path = unique_tmp_path(parent, path.file_name().unwrap_or_default());
        write_atomic(&tmp_path, &path, &data)
    }
}

fn encode_embedding(embedding: &[f32]) -> io::Result<Vec<u8>> {
    const MAGIC: &[u8; 8] = b"NOVAEMBC";
    const VERSION: u32 = 1;

    let dims: u32 = embedding.len().try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "embedding dimension must fit in u32",
        )
    })?;

    let mut out = Vec::with_capacity(8 + 4 + 4 + embedding.len().saturating_mul(4));
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&dims.to_le_bytes());
    for v in embedding {
        out.extend_from_slice(&v.to_le_bytes());
    }
    Ok(out)
}

fn decode_embedding(bytes: &[u8]) -> Option<Vec<f32>> {
    const MAGIC: &[u8; 8] = b"NOVAEMBC";
    const VERSION: u32 = 1;
    const HEADER_LEN: usize = 8 + 4 + 4;

    if bytes.len() < HEADER_LEN {
        return None;
    }
    if &bytes[..8] != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let dims = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    let expected_len = HEADER_LEN.checked_add(dims.checked_mul(4)?)?;
    if expected_len != bytes.len() {
        return None;
    }

    let mut out = Vec::with_capacity(dims);
    for chunk in bytes[HEADER_LEN..].chunks_exact(4) {
        out.push(f32::from_le_bytes(chunk.try_into().ok()?));
    }
    Some(out)
}

fn unique_tmp_path(dir: &Path, final_name: &std::ffi::OsStr) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let pid = std::process::id();
    let mut attempt = 0usize;
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let suffix = format!("{}.{}.{}", pid, n, attempt);
        let mut tmp_name = final_name.to_os_string();
        tmp_name.push(".tmp.");
        tmp_name.push(suffix);
        let tmp_path = dir.join(tmp_name);
        if !tmp_path.exists() {
            return tmp_path;
        }
        attempt = attempt.saturating_add(1);
    }
}

fn write_atomic(tmp_path: &Path, final_path: &Path, data: &[u8]) -> io::Result<()> {
    {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(tmp_path)?;
        file.write_all(data)?;
        file.sync_all()?;
    }

    match fs::rename(tmp_path, final_path) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Another process may have beaten us to it; treat that as success.
            if final_path.exists() {
                let _ = fs::remove_file(tmp_path);
                return Ok(());
            }

            let _ = fs::remove_file(tmp_path);
            Err(err)
        }
    }
}
