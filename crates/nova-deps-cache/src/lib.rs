use fs2::FileExt as _;
use sha2::Digest as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

/// Schema version for dependency index bundles stored in the global deps cache.
pub const DEPS_INDEX_SCHEMA_VERSION: u32 = 1;

const BUNDLE_FILE_NAME: &str = "classpath.idx";
const LOCK_FILE_NAME: &str = "classpath.lock";

#[derive(Debug, thiserror::Error)]
pub enum DepsCacheError {
    #[error(transparent)]
    Cache(#[from] nova_cache::CacheError),
    #[error(transparent)]
    Storage(#[from] nova_storage::StorageError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("archive error: {0}")]
    Archive(String),
}

#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub struct DepsFieldStub {
    pub name: String,
    pub descriptor: String,
    pub signature: Option<String>,
    pub access_flags: u16,
    pub annotations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub struct DepsMethodStub {
    pub name: String,
    pub descriptor: String,
    pub signature: Option<String>,
    pub access_flags: u16,
    pub annotations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub struct DepsClassStub {
    pub binary_name: String,
    pub internal_name: String,
    pub access_flags: u16,
    pub super_binary_name: Option<String>,
    pub interfaces: Vec<String>,
    pub signature: Option<String>,
    pub annotations: Vec<String>,
    pub fields: Vec<DepsFieldStub>,
    pub methods: Vec<DepsMethodStub>,
}

/// Compact trigram index (ASCII-case-folded).
#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub struct TrigramIndexData {
    pub keys: Vec<u32>,
    /// Offsets into `values`. Always length `keys.len() + 1`.
    pub offsets: Vec<u32>,
    pub values: Vec<u32>,
}

impl TrigramIndexData {
    pub fn empty() -> Self {
        Self {
            keys: Vec::new(),
            offsets: vec![0],
            values: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub struct DependencyIndexBundle {
    /// Hex-encoded SHA-256 of the dependency artifact bytes.
    pub jar_sha256: String,
    /// Class stubs extracted from `.class` entries.
    pub classes: Vec<DepsClassStub>,
    /// Distinct packages (dot-separated, sorted).
    pub packages: Vec<String>,
    /// All package prefixes (dot-separated, sorted).
    pub package_prefixes: Vec<String>,
    /// Sorted binary class names corresponding to `classes`.
    pub binary_names_sorted: Vec<String>,
    /// Optional trigram index for fuzzy class name lookup. (May be empty.)
    pub trigram_index: TrigramIndexData,
}

pub fn sha256_hex(path: &Path) -> Result<String, DepsCacheError> {
    let mut file = File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[derive(Debug, Clone)]
pub struct DependencyIndexStore {
    root: PathBuf,
}

impl DependencyIndexStore {
    /// Create a store rooted at an explicit path (useful for tests).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Create a store rooted in the global cache directory.
    ///
    /// Defaults to `~/.nova/cache/deps` (but respects `NOVA_CACHE_DIR`).
    pub fn from_env() -> Result<Self, DepsCacheError> {
        let config = nova_cache::CacheConfig::from_env();
        let root = nova_cache::deps_cache_dir(&config)?;
        Ok(Self::new(root))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn bundle_dir(&self, jar_sha256: &str) -> PathBuf {
        self.root.join(jar_sha256)
    }

    pub fn bundle_path(&self, jar_sha256: &str) -> PathBuf {
        self.bundle_dir(jar_sha256).join(BUNDLE_FILE_NAME)
    }

    fn lock_path(&self, jar_sha256: &str) -> PathBuf {
        self.bundle_dir(jar_sha256).join(LOCK_FILE_NAME)
    }

    /// Load a dependency index bundle, returning `Ok(None)` for cache misses,
    /// incompatibility, or corruption.
    pub fn try_load(
        &self,
        jar_sha256: &str,
    ) -> Result<Option<DependencyIndexBundle>, DepsCacheError> {
        let path = self.bundle_path(jar_sha256);

        let archive = match nova_storage::PersistedArchive::<DependencyIndexBundle>::open_optional(
            &path,
            nova_storage::ArtifactKind::DepsIndexBundle,
            DEPS_INDEX_SCHEMA_VERSION,
        ) {
            Ok(Some(archive)) => archive,
            Ok(None) => return Ok(None),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
        };

        let bundle = match archive.to_owned() {
            Ok(bundle) => bundle,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
        };

        if bundle.jar_sha256 != jar_sha256 {
            return Ok(None);
        }

        Ok(Some(bundle))
    }

    /// Store a dependency index bundle.
    ///
    /// Uses a per-artifact lock file + atomic rename to avoid corruption under
    /// concurrent writes.
    pub fn store(&self, bundle: &DependencyIndexBundle) -> Result<(), DepsCacheError> {
        let jar_sha256 = bundle.jar_sha256.as_str();
        let dir = self.bundle_dir(jar_sha256);
        std::fs::create_dir_all(&dir)?;

        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.lock_path(jar_sha256))?;
        lock_file.lock_exclusive()?;

        if self.try_load(jar_sha256)?.is_some() {
            let _ = lock_file.unlock();
            return Ok(());
        }

        nova_storage::write_archive_atomic(
            &self.bundle_path(jar_sha256),
            nova_storage::ArtifactKind::DepsIndexBundle,
            DEPS_INDEX_SCHEMA_VERSION,
            bundle,
            nova_storage::Compression::None,
        )?;

        lock_file.unlock()?;
        Ok(())
    }

    pub fn pack(&self, output: &Path) -> Result<(), DepsCacheError> {
        if let Some(parent) = output.parent() {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            std::fs::create_dir_all(parent)?;
        }

        let file = File::create(output)?;
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);

        if self.root.exists() {
            for entry in walkdir::WalkDir::new(&self.root).into_iter() {
                let entry = entry.map_err(|e| DepsCacheError::Archive(e.to_string()))?;
                let path = entry.path();
                let rel = path
                    .strip_prefix(&self.root)
                    .map_err(|e| DepsCacheError::Archive(e.to_string()))?;
                if rel.as_os_str().is_empty() {
                    continue;
                }
                if rel.ends_with(LOCK_FILE_NAME) {
                    continue;
                }
                // Skip crashed atomic-write temp files from `write_archive_atomic`, which
                // uses unique names like `<dest>.tmp.<pid>.<counter>`.
                if rel
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".tmp") || name.contains(".tmp."))
                {
                    continue;
                }
                if entry.file_type().is_dir() {
                    tar.append_dir(rel, path)
                        .map_err(|e| DepsCacheError::Archive(e.to_string()))?;
                } else if entry.file_type().is_file() {
                    tar.append_path_with_name(path, rel)
                        .map_err(|e| DepsCacheError::Archive(e.to_string()))?;
                }
            }
        }

        tar.finish()
            .map_err(|e| DepsCacheError::Archive(e.to_string()))?;
        let encoder = tar
            .into_inner()
            .map_err(|e| DepsCacheError::Archive(e.to_string()))?;
        encoder
            .finish()
            .map_err(|e| DepsCacheError::Archive(e.to_string()))?;
        Ok(())
    }

    pub fn install(&self, archive_path: &Path) -> Result<(), DepsCacheError> {
        let file = File::open(archive_path)?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);

        // We keep installation deliberately simple: unpack files into the deps
        // root. Compatibility is enforced on load via `nova-storage` headers.
        //
        // Use `unpack_in` to guard against path traversal in archives.
        for entry in archive
            .entries()
            .map_err(|e| DepsCacheError::Archive(e.to_string()))?
        {
            let mut entry = entry.map_err(|e| DepsCacheError::Archive(e.to_string()))?;
            entry
                .unpack_in(&self.root)
                .map_err(|e| DepsCacheError::Archive(e.to_string()))?;
        }
        Ok(())
    }
}

fn fold_byte(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

fn pack_trigram(a: u8, b: u8, c: u8) -> u32 {
    ((a as u32) << 16) | ((b as u32) << 8) | (c as u32)
}

fn trigrams(text: &str, out: &mut Vec<u32>) {
    let bytes = text.as_bytes();
    if bytes.len() < 3 {
        return;
    }

    out.clear();
    out.reserve(bytes.len().saturating_sub(2));

    let mut a = fold_byte(bytes[0]);
    let mut b = fold_byte(bytes[1]);
    for &c_raw in &bytes[2..] {
        let c = fold_byte(c_raw);
        out.push(pack_trigram(a, b, c));
        a = b;
        b = c;
    }
}

pub fn build_trigram_index(binary_names_sorted: &[String]) -> TrigramIndexData {
    if binary_names_sorted.is_empty() {
        return TrigramIndexData::empty();
    }

    let mut pairs: Vec<u64> = Vec::new(); // (trigram << 32) | id
    let mut scratch: Vec<u32> = Vec::new();

    for (id, name) in binary_names_sorted.iter().enumerate() {
        trigrams(name, &mut scratch);
        if scratch.is_empty() {
            continue;
        }
        scratch.sort_unstable();
        scratch.dedup();
        let id = id as u32;
        pairs.extend(scratch.iter().map(|&g| ((g as u64) << 32) | id as u64));
    }

    pairs.sort_unstable();
    pairs.dedup();

    if pairs.is_empty() {
        return TrigramIndexData::empty();
    }

    let mut keys: Vec<u32> = Vec::new();
    let mut offsets: Vec<u32> = Vec::new();
    let mut values: Vec<u32> = Vec::new();

    offsets.push(0);

    let mut cur_key: Option<u32> = None;
    for pair in pairs {
        let trigram = (pair >> 32) as u32;
        let id = pair as u32;
        match cur_key {
            Some(k) if k == trigram => {
                values.push(id);
            }
            Some(k) => {
                keys.push(k);
                offsets.push(values.len() as u32);
                values.push(id);
                cur_key = Some(trigram);
            }
            None => {
                values.push(id);
                cur_key = Some(trigram);
            }
        }
    }

    if let Some(k) = cur_key {
        keys.push(k);
        offsets.push(values.len() as u32);
    }

    debug_assert_eq!(offsets.len(), keys.len() + 1);

    TrigramIndexData {
        keys,
        offsets,
        values,
    }
}
