use std::fs::File;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};
use nova_core::Endian;
use rkyv::Deserialize;
use thiserror::Error;

use crate::header::{ArtifactKind, Compression, StorageHeader, HEADER_LEN};

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
    #[error("payload size {payload_len} does not fit into addressable memory")]
    OversizedPayload { payload_len: u64 },
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
        rkyv::Archived<T>:
            rkyv::Deserialize<T, rkyv::de::deserializers::SharedDeserializeMap>,
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
        let file = File::open(path)?;
        let file_len = file.metadata()?.len() as usize;
        if file_len < HEADER_LEN {
            return Err(StorageError::Truncated {
                expected: HEADER_LEN,
                found: file_len,
            });
        }

        // mmap is the fast path. If it fails, fall back to reading the file.
        match unsafe { MmapOptions::new().map(&file) } {
            Ok(mmap) => Self::from_mmap(mmap, expected_kind, expected_schema),
            Err(_) => {
                let bytes = std::fs::read(path)?;
                Self::from_owned_bytes(bytes, expected_kind, expected_schema)
            }
        }
    }

    fn from_mmap(
        mmap: Mmap,
        expected_kind: ArtifactKind,
        expected_schema: u32,
    ) -> Result<Self, StorageError> {
        let header = StorageHeader::decode(&mmap[..HEADER_LEN])?;
        validate_header(&header, expected_kind, expected_schema)?;

        let payload_offset = header.payload_offset as usize;
        let payload_len = header.payload_len as usize;
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
                let decompressed = decompress(payload, header.uncompressed_len)?;
                let aligned = aligned_bytes(&decompressed);
                Self::from_backing(header, Backing::Owned(aligned))
            }
        }
    }

    fn from_owned_bytes(
        bytes: Vec<u8>,
        expected_kind: ArtifactKind,
        expected_schema: u32,
    ) -> Result<Self, StorageError> {
        if bytes.len() < HEADER_LEN {
            return Err(StorageError::Truncated {
                expected: HEADER_LEN,
                found: bytes.len(),
            });
        }

        let header = StorageHeader::decode(&bytes[..HEADER_LEN])?;
        validate_header(&header, expected_kind, expected_schema)?;

        let payload_offset = header.payload_offset as usize;
        let payload_len = header.payload_len as usize;
        ensure_file_bounds(bytes.len(), payload_offset, payload_len)?;

        let payload = &bytes[payload_offset..payload_offset + payload_len];
        let uncompressed = match header.compression {
            Compression::None => payload.to_vec(),
            Compression::Zstd => decompress(payload, header.uncompressed_len)?,
        };

        let aligned = aligned_bytes(&uncompressed);
        Self::from_backing(header, Backing::Owned(aligned))
    }

    fn from_backing(header: StorageHeader, backing: Backing) -> Result<Self, StorageError> {
        let payload = backing.payload();

        let required = std::mem::align_of::<rkyv::Archived<T>>();
        let got = payload.as_ptr() as usize;
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

fn decompress(payload: &[u8], uncompressed_len: u64) -> Result<Vec<u8>, StorageError> {
    let len: usize = uncompressed_len
        .try_into()
        .map_err(|_| StorageError::OversizedPayload {
            payload_len: uncompressed_len,
        })?;
    zstd::bulk::decompress(payload, len).map_err(|e| StorageError::Decompression(e.to_string()))
}

fn aligned_bytes(bytes: &[u8]) -> rkyv::util::AlignedVec {
    let mut aligned = rkyv::util::AlignedVec::with_capacity(bytes.len());
    aligned.extend_from_slice(bytes);
    aligned
}

fn verify_payload_hash(header: &StorageHeader, payload: &[u8]) -> Result<(), StorageError> {
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
