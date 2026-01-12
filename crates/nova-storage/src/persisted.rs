use std::fs::File;
use std::io::Read;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};
use nova_core::Endian;
use rkyv::Deserialize;
use thiserror::Error;

use crate::header::{ArtifactKind, Compression, StorageHeader, HEADER_LEN};

/// Maximum file size we'll accept when mmap is unavailable.
///
/// Nova runs many agents concurrently with a hard per-agent memory budget (see `AGENTS.md`).
/// Treating unexpectedly-large cache files as a miss is preferable to risking an OOM.
pub(crate) const MAX_MMAP_FALLBACK_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

/// Default maximum number of bytes an artifact payload may occupy on disk.
///
/// This is a safety limit to prevent corrupted headers from requesting huge
/// allocations or mappings.
///
/// Can be overridden via `NOVA_STORAGE_MAX_PAYLOAD_LEN_BYTES`.
pub const MAX_PAYLOAD_LEN_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Default maximum number of bytes a compressed artifact is allowed to decompress to.
///
/// This bounds `zstd::bulk::decompress*` output allocations based on the header's
/// `uncompressed_len` field. Corrupted or adversarial cache files must not be able to
/// trigger multi-gigabyte allocations.
///
/// Can be overridden via `NOVA_STORAGE_MAX_UNCOMPRESSED_LEN_BYTES`.
pub const MAX_UNCOMPRESSED_LEN_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB

/// Trait alias for archived roots that can be validated with `rkyv`.
///
/// We keep this behind a custom trait to avoid repeating the `for<'a>` bound
/// everywhere.
pub trait CheckableArchived:
    for<'a> rkyv::bytecheck::CheckBytes<rkyv::validation::validators::DefaultValidator<'a>>
{
}

impl<T> CheckableArchived for T where
    T: for<'a> rkyv::bytecheck::CheckBytes<rkyv::validation::validators::DefaultValidator<'a>>
{
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid header: {0}")]
    InvalidHeader(&'static str),
    #[error("{kind} too large: {bytes} bytes (limit {limit} bytes)")]
    TooLarge {
        kind: &'static str,
        bytes: u64,
        limit: u64,
    },
    #[error("incompatible artifact kind: expected {expected:?}, found {found:?}")]
    WrongArtifact {
        expected: ArtifactKind,
        found: ArtifactKind,
    },
    #[error("incompatible schema version: expected {expected}, found {found}")]
    WrongSchema { expected: u32, found: u32 },
    #[error("incompatible nova version: expected {expected}, found {found}")]
    WrongNovaVersion { expected: String, found: String },
    #[error("incompatible target endian: expected {expected:?}, found {found:?}")]
    WrongEndian { expected: Endian, found: Endian },
    #[error("incompatible pointer width: expected {expected}, found {found}")]
    WrongPointerWidth { expected: u8, found: u8 },
    #[error("truncated file: expected at least {expected} bytes, found {found}")]
    Truncated { expected: usize, found: usize },
    #[error("invalid payload alignment: required {required} bytes, got {got}")]
    Misaligned { required: usize, got: usize },
    #[error("archive validation failed: {0}")]
    Validation(String),
    #[error("decompression failed: {0}")]
    Decompression(String),
    #[error("unsupported compression tag {0}")]
    UnsupportedCompression(u8),
    #[error("payload size {payload_len} exceeds maximum supported {cap} bytes")]
    OversizedPayload { payload_len: u64, cap: u64 },
    #[error("file size {file_len} exceeds fallback read cap {cap} bytes")]
    TooLargeForFallbackRead { file_len: u64, cap: u64 },
    #[error("payload hash mismatch: expected {expected}, found {found}")]
    HashMismatch { expected: u64, found: u64 },
}

enum Backing {
    Mmap {
        mmap: Mmap,
        payload_offset: usize,
        payload_len: usize,
    },
    Owned(rkyv::util::AlignedVec),
}

impl Backing {
    fn payload(&self) -> &[u8] {
        match self {
            Backing::Mmap {
                mmap,
                payload_offset,
                payload_len,
            } => &mmap[*payload_offset..*payload_offset + *payload_len],
            Backing::Owned(bytes) => bytes.as_slice(),
        }
    }
}

/// A persisted `rkyv` archive backed by either an mmap region (preferred) or an
/// owned aligned buffer.
pub struct PersistedArchive<T>
where
    T: rkyv::Archive,
{
    header: StorageHeader,
    backing: Backing,
    archived: std::ptr::NonNull<rkyv::Archived<T>>,
    _marker: PhantomData<T>,
}

impl<T> std::fmt::Debug for PersistedArchive<T>
where
    T: rkyv::Archive,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistedArchive")
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl<T> PersistedArchive<T>
where
    T: rkyv::Archive,
    rkyv::Archived<T>: CheckableArchived,
{
    pub fn header(&self) -> &StorageHeader {
        &self.header
    }

    /// Returns the raw archived payload bytes (excluding the header).
    ///
    /// For compressed artifacts, this returns the decompressed payload.
    pub fn payload_bytes(&self) -> &[u8] {
        // Keep the backing alive; see `archived`.
        let _ = &self.backing;
        self.backing.payload()
    }

    /// Deserializes the archived payload into an owned value.
    ///
    /// This is intended as a fallback for callers that need a mutable copy.
    pub fn to_owned(&self) -> Result<T, StorageError>
    where
        rkyv::Archived<T>: rkyv::Deserialize<T, rkyv::de::deserializers::SharedDeserializeMap>,
    {
        let mut deserializer = rkyv::de::deserializers::SharedDeserializeMap::default();
        self.archived()
            .deserialize(&mut deserializer)
            .map_err(|e| StorageError::Validation(e.to_string()))
    }

    pub fn archived(&self) -> &rkyv::Archived<T> {
        // Ensure the backing storage is considered "used" so the compiler
        // doesn't warn about it being dead. The backing must stay alive for the
        // lifetime of `self` because `archived` points into it.
        let _ = &self.backing;

        // Safety: `archived` is produced by `rkyv::check_archived_root` and
        // points into `backing`, which is kept alive for the lifetime of `self`.
        unsafe { self.archived.as_ref() }
    }

    /// Opens a persisted archive, returning `Ok(None)` when the file does not exist.
    pub fn open_optional(
        path: &Path,
        expected_kind: ArtifactKind,
        expected_schema: u32,
    ) -> Result<Option<Self>, StorageError> {
        match Self::open(path, expected_kind, expected_schema) {
            Ok(archive) => Ok(Some(archive)),
            Err(StorageError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn open(
        path: &Path,
        expected_kind: ArtifactKind,
        expected_schema: u32,
    ) -> Result<Self, StorageError> {
        Self::open_with_mmap(path, expected_kind, expected_schema, true)
    }

    #[cfg(test)]
    pub(crate) fn open_without_mmap(
        path: &Path,
        expected_kind: ArtifactKind,
        expected_schema: u32,
    ) -> Result<Self, StorageError> {
        Self::open_with_mmap(path, expected_kind, expected_schema, false)
    }

    fn open_with_mmap(
        path: &Path,
        expected_kind: ArtifactKind,
        expected_schema: u32,
        prefer_mmap: bool,
    ) -> Result<Self, StorageError> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < HEADER_LEN as u64 {
            return Err(StorageError::Truncated {
                expected: HEADER_LEN,
                found: file_len as usize,
            });
        }

        // mmap is the fast path. If it fails, fall back to reading the file.
        if prefer_mmap {
            if let Ok(mmap) = unsafe { MmapOptions::new().map(&file) } {
                return Self::from_mmap(mmap, expected_kind, expected_schema);
            }
        }

        if file_len > MAX_MMAP_FALLBACK_BYTES {
            return Err(StorageError::TooLargeForFallbackRead {
                file_len,
                cap: MAX_MMAP_FALLBACK_BYTES,
            });
        }

        Self::from_file(file, file_len as usize, expected_kind, expected_schema)
    }

    fn checked_payload_len(header: &StorageHeader) -> Result<usize, StorageError> {
        let uncompressed_limit = max_uncompressed_len_bytes();
        if header.uncompressed_len > uncompressed_limit {
            return Err(StorageError::TooLarge {
                kind: "uncompressed payload",
                bytes: header.uncompressed_len,
                limit: uncompressed_limit,
            });
        }

        let limit = max_payload_len_bytes();
        if header.payload_len > limit {
            return Err(StorageError::TooLarge {
                kind: "payload",
                bytes: header.payload_len,
                limit,
            });
        }
        header
            .payload_len
            .try_into()
            .map_err(|_| StorageError::OversizedPayload {
                payload_len: header.payload_len,
                cap: usize::MAX as u64,
            })
    }

    fn from_file(
        mut file: File,
        file_len: usize,
        expected_kind: ArtifactKind,
        expected_schema: u32,
    ) -> Result<Self, StorageError> {
        let mut header_bytes = [0u8; HEADER_LEN];
        file.read_exact(&mut header_bytes)?;

        let header = StorageHeader::decode(&header_bytes)?;
        validate_header(&header, expected_kind, expected_schema)?;

        let payload_offset = header.payload_offset as usize;
        let payload_len = Self::checked_payload_len(&header)?;
        ensure_file_bounds(file_len, payload_offset, payload_len)?;

        let aligned = match header.compression {
            Compression::None => {
                let mut out = rkyv::util::AlignedVec::with_capacity(payload_len);
                out.resize(payload_len, 0);
                file.read_exact(&mut out)?;
                out
            }
            Compression::Zstd => {
                let mut compressed = vec![0u8; payload_len];
                file.read_exact(&mut compressed)?;
                decompress(&compressed, header.uncompressed_len)?
            }
        };

        Self::from_backing(header, Backing::Owned(aligned))
    }

    fn from_mmap(
        mmap: Mmap,
        expected_kind: ArtifactKind,
        expected_schema: u32,
    ) -> Result<Self, StorageError> {
        let header = StorageHeader::decode(&mmap[..HEADER_LEN])?;
        validate_header(&header, expected_kind, expected_schema)?;

        let payload_offset = header.payload_offset as usize;
        let payload_len = Self::checked_payload_len(&header)?;
        ensure_file_bounds(mmap.len(), payload_offset, payload_len)?;

        match header.compression {
            Compression::None => Self::from_backing(
                header,
                Backing::Mmap {
                    mmap,
                    payload_offset,
                    payload_len,
                },
            ),
            Compression::Zstd => {
                let payload = &mmap[payload_offset..payload_offset + payload_len];
                let aligned = decompress(payload, header.uncompressed_len)?;
                Self::from_backing(header, Backing::Owned(aligned))
            }
        }
    }

    fn from_backing(header: StorageHeader, backing: Backing) -> Result<Self, StorageError> {
        let payload = backing.payload();

        let required = std::mem::align_of::<rkyv::Archived<T>>();
        let got = payload.as_ptr().addr();
        if !got.is_multiple_of(required) {
            return Err(StorageError::Misaligned { required, got });
        }

        verify_payload_hash(&header, payload)?;

        let archived = rkyv::check_archived_root::<T>(payload)
            .map_err(|e| StorageError::Validation(e.to_string()))?;
        let archived = std::ptr::NonNull::from(archived);

        Ok(Self {
            header,
            backing,
            archived,
            _marker: PhantomData,
        })
    }
}

impl<T> Deref for PersistedArchive<T>
where
    T: rkyv::Archive,
    rkyv::Archived<T>: CheckableArchived,
{
    type Target = rkyv::Archived<T>;

    fn deref(&self) -> &Self::Target {
        self.archived()
    }
}

// Safety: `PersistedArchive` provides shared, immutable access to an archived
// root which is backed by an mmap or an owned, immutable buffer.
//
// The `NonNull` pointer is only used to avoid re-validating the archive on each
// access; it always points into `backing`, which remains alive for the lifetime
// of `self`.
unsafe impl<T> Send for PersistedArchive<T>
where
    T: rkyv::Archive,
    rkyv::Archived<T>: Sync,
{
}

unsafe impl<T> Sync for PersistedArchive<T>
where
    T: rkyv::Archive,
    rkyv::Archived<T>: Sync,
{
}

fn validate_header(
    header: &StorageHeader,
    expected_kind: ArtifactKind,
    expected_schema: u32,
) -> Result<(), StorageError> {
    if header.kind != expected_kind {
        return Err(StorageError::WrongArtifact {
            expected: expected_kind,
            found: header.kind,
        });
    }

    if header.schema_version != expected_schema {
        return Err(StorageError::WrongSchema {
            expected: expected_schema,
            found: header.schema_version,
        });
    }

    let expected_nova = nova_core::NOVA_VERSION.to_owned();
    if header.nova_version != expected_nova {
        return Err(StorageError::WrongNovaVersion {
            expected: expected_nova,
            found: header.nova_version.clone(),
        });
    }

    let expected_endian = nova_core::target_endian();
    if header.endian != expected_endian {
        return Err(StorageError::WrongEndian {
            expected: expected_endian,
            found: header.endian,
        });
    }

    let expected_pointer_width = nova_core::target_pointer_width();
    if header.pointer_width != expected_pointer_width {
        return Err(StorageError::WrongPointerWidth {
            expected: expected_pointer_width,
            found: header.pointer_width,
        });
    }

    if header.payload_offset as usize != HEADER_LEN {
        return Err(StorageError::InvalidHeader("unexpected payload offset"));
    }

    Ok(())
}

fn ensure_file_bounds(
    file_len: usize,
    payload_offset: usize,
    payload_len: usize,
) -> Result<(), StorageError> {
    let expected = payload_offset
        .checked_add(payload_len)
        .ok_or(StorageError::InvalidHeader("payload offset overflow"))?;
    if file_len < expected {
        return Err(StorageError::Truncated {
            expected,
            found: file_len,
        });
    }
    Ok(())
}

fn decompress(
    payload: &[u8],
    uncompressed_len: u64,
) -> Result<rkyv::util::AlignedVec, StorageError> {
    let limit = max_uncompressed_len_bytes();
    if uncompressed_len > limit {
        return Err(StorageError::TooLarge {
            kind: "uncompressed payload",
            bytes: uncompressed_len,
            limit,
        });
    }
    let len: usize = uncompressed_len
        .try_into()
        .map_err(|_| StorageError::OversizedPayload {
            payload_len: uncompressed_len,
            cap: usize::MAX as u64,
        })?;

    let mut out = rkyv::util::AlignedVec::with_capacity(len);
    out.resize(len, 0);
    let written = zstd::bulk::decompress_to_buffer(payload, &mut out)
        .map_err(|e| StorageError::Decompression(e.to_string()))?;
    if written != len {
        return Err(StorageError::Decompression(format!(
            "decompressed {written} bytes but header declared {len}"
        )));
    }
    Ok(out)
}

fn verify_payload_hash(header: &StorageHeader, payload: &[u8]) -> Result<(), StorageError> {
    let should_validate = match header.compression {
        // Compressed artifacts already require a full decompression into memory,
        // so validating the content hash is a cheap extra integrity check.
        Compression::Zstd => true,
        // For uncompressed (typically mmap-backed) artifacts, allow opting into
        // hashing via env var since it requires touching the full payload.
        Compression::None => env_flag_enabled("NOVA_STORAGE_VALIDATE_HASH"),
    };

    if !should_validate {
        return Ok(());
    }

    let found = content_hash(payload);
    if found != header.content_hash {
        return Err(StorageError::HashMismatch {
            expected: header.content_hash,
            found,
        });
    }
    Ok(())
}

fn content_hash(payload: &[u8]) -> u64 {
    let hash_bytes = blake3::hash(payload);
    u64::from_le_bytes(hash_bytes.as_bytes()[..8].try_into().expect("hash slice"))
}

fn max_payload_len_bytes() -> u64 {
    env_u64("NOVA_STORAGE_MAX_PAYLOAD_LEN_BYTES").unwrap_or(MAX_PAYLOAD_LEN_BYTES)
}

fn max_uncompressed_len_bytes() -> u64 {
    env_u64("NOVA_STORAGE_MAX_UNCOMPRESSED_LEN_BYTES").unwrap_or(MAX_UNCOMPRESSED_LEN_BYTES)
}

fn env_u64(key: &str) -> Option<u64> {
    let value = std::env::var(key).ok()?;
    value.parse::<u64>().ok()
}

fn env_flag_enabled(key: &str) -> bool {
    let Some(value) = std::env::var_os(key) else {
        return false;
    };
    let value = value.to_string_lossy();
    matches!(
        value.as_ref(),
        "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
    )
}
