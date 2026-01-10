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

        let payload_start = HEADER_LEN;
        let payload_len = bytes.len() - payload_start;
        let header = StorageHeader::decode(&bytes[..HEADER_LEN]).unwrap();
        let offset = (header.content_hash as usize) % payload_len;
        bytes[payload_start + offset] ^= 0x01;

        std::fs::write(&path, &bytes).unwrap();

        let err = PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::HashMismatch { expected, found } => {
                assert_ne!(expected, found);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
