//! Memory-mapped, zero-copy storage backend for Nova persisted artifacts.
//!
//! ## Format
//! Each persisted artifact is stored as:
//! - a fixed-size header (64 bytes, little-endian)
//! - a payload containing an `rkyv` archived root object
//!
//! The header embeds:
//! - schema version
//! - Nova version
//! - endianness and pointer-width compatibility checks
//! - compression flag (currently whole-payload zstd or none)
//!
//! ## Compatibility limitations
//! `rkyv` archives are not portable across:
//! - endianness (little vs big)
//! - pointer width (32-bit vs 64-bit) because container lengths are archived as
//!   `usize`.
//!
//! Nova detects these mismatches and treats the artifact as incompatible.

mod header;
mod persisted;
mod write;

pub use header::{ArtifactKind, Compression, StorageHeader, HEADER_LEN};
pub use persisted::{CheckableArchived, PersistedArchive, StorageError};
pub use write::write_archive_atomic;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
    #[archive(check_bytes)]
    struct Sample {
        a: u32,
        b: String,
        values: Vec<u64>,
    }

    #[test]
    fn round_trip_uncompressed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sample.bin");

        let value = Sample {
            a: 42,
            b: "hello".to_string(),
            values: vec![1, 2, 3, 4],
        };

        write_archive_atomic(
            &path,
            ArtifactKind::AstArtifacts,
            1,
            &value,
            Compression::None,
        )
        .unwrap();

        let loaded = PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap();
        assert_eq!(loaded.header().schema_version, 1);

        assert_eq!(loaded.to_owned().unwrap(), value);
    }

    #[test]
    fn truncated_file_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sample.bin");

        let value = Sample {
            a: 1,
            b: "x".to_string(),
            values: vec![9],
        };

        write_archive_atomic(
            &path,
            ArtifactKind::AstArtifacts,
            1,
            &value,
            Compression::None,
        )
        .unwrap();

        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len((HEADER_LEN - 1) as u64).unwrap();

        let err = PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::Truncated { .. } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn corrupted_payload_is_hash_mismatch() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sample.bin");

        let value = Sample {
            a: 123,
            b: "hello".to_string(),
            values: vec![1, 2, 3, 4],
        };

        write_archive_atomic(
            &path,
            ArtifactKind::AstArtifacts,
            1,
            &value,
            Compression::None,
        )
        .unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > HEADER_LEN);

        let payload = &bytes[HEADER_LEN..];

        // Corrupt a byte in a `u64` value so `rkyv` validation still succeeds, and the
        // content hash is what reliably detects the corruption.
        let mut aligned = rkyv::util::AlignedVec::with_capacity(payload.len());
        aligned.extend_from_slice(payload);
        let archived = rkyv::check_archived_root::<Sample>(&aligned).unwrap();

        let element_ptr = &archived.values[0] as *const u64 as *const u8;
        let payload_ptr = aligned.as_ptr();
        let offset = unsafe { element_ptr.offset_from(payload_ptr) as usize };
        bytes[HEADER_LEN + offset] ^= 0x01;

        std::fs::write(&path, &bytes).unwrap();

        let corrupted_payload = &bytes[HEADER_LEN..];
        let mut aligned_corrupted = rkyv::util::AlignedVec::with_capacity(corrupted_payload.len());
        aligned_corrupted.extend_from_slice(corrupted_payload);
        assert!(rkyv::check_archived_root::<Sample>(&aligned_corrupted).is_ok());

        let err = PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::HashMismatch { expected, found } => {
                assert_ne!(expected, found);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
