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
//!
//! ## Safety limits and integrity checks
//! Persisted headers are validated before any large allocation or decompression to
//! guard against corrupted cache files causing OOMs.
//!
//! The default caps can be overridden via environment variables:
//! - `NOVA_STORAGE_MAX_PAYLOAD_LEN_BYTES` (default: [`MAX_PAYLOAD_LEN_BYTES`])
//! - `NOVA_STORAGE_MAX_UNCOMPRESSED_LEN_BYTES` (default: [`MAX_UNCOMPRESSED_LEN_BYTES`])
//!
//! The header also stores a truncated blake3 hash of the uncompressed payload:
//! - always validated for zstd-compressed artifacts
//! - for uncompressed artifacts, validation is opt-in via `NOVA_STORAGE_VALIDATE_HASH=1`

mod header;
mod persisted;
mod write;

pub use header::{ArtifactKind, Compression, StorageHeader, HEADER_LEN};
pub use persisted::{
    CheckableArchived, PersistedArchive, StorageError, MAX_PAYLOAD_LEN_BYTES,
    MAX_UNCOMPRESSED_LEN_BYTES,
};
pub use write::{
    write_archive_atomic, write_archive_atomic_with_options, FileArchiveSerializer,
    WritableArchive, WriteArchiveOptions, WriteCompression,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::io::{Seek, SeekFrom, Write};
    use std::panic::Location;
    use std::sync::{Arc, Barrier, Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[track_caller]
    fn env_lock() -> MutexGuard<'static, ()> {
        match ENV_LOCK.lock() {
            Ok(guard) => guard,
            Err(err) => {
                let loc = Location::caller();
                tracing::error!(
                    target = "nova.storage.tests",
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    error = %err,
                    "env lock poisoned; continuing with recovered guard"
                );
                err.into_inner()
            }
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }

        fn remove(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

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

        let loaded =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap();
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

        let err =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::Truncated { .. } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn round_trip_zstd() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sample-zstd.bin");

        let value = Sample {
            a: 7,
            b: "compressed".to_string(),
            values: (0..128).collect(),
        };

        write_archive_atomic_with_options(
            &path,
            ArtifactKind::AstArtifacts,
            1,
            &value,
            WriteArchiveOptions {
                compression: WriteCompression::Zstd { level: 0 },
                validate_after_write: true,
            },
        )
        .unwrap();

        let loaded =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap();
        assert_eq!(loaded.header().compression, Compression::Zstd);
        assert_eq!(loaded.to_owned().unwrap(), value);
    }

    #[test]
    fn corrupted_payload_is_hash_mismatch() {
        let _lock = env_lock();
        let _hash_guard = EnvVarGuard::set("NOVA_STORAGE_VALIDATE_HASH", "1");
        // Ensure size-related env vars from the environment don't make this test flaky.
        let _payload_guard = EnvVarGuard::remove("NOVA_STORAGE_MAX_PAYLOAD_LEN_BYTES");
        let _uncompressed_guard = EnvVarGuard::remove("NOVA_STORAGE_MAX_UNCOMPRESSED_LEN_BYTES");

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

        let err =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::HashMismatch { expected, found } => {
                assert_ne!(expected, found);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn corrupted_compressed_payload_is_hash_mismatch() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sample.bin");

        let value = Sample {
            a: 456,
            b: "hello".to_string(),
            values: vec![1, 2, 3, 4],
        };

        write_archive_atomic(
            &path,
            ArtifactKind::AstArtifacts,
            1,
            &value,
            Compression::Zstd,
        )
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > HEADER_LEN);

        let mut header = StorageHeader::decode(&bytes[..HEADER_LEN]).unwrap();
        assert_eq!(header.compression, Compression::Zstd);

        let payload_offset = header.payload_offset as usize;
        let payload_len = header.payload_len as usize;
        let compressed_payload = &bytes[payload_offset..payload_offset + payload_len];

        let mut uncompressed = zstd::bulk::decompress(
            compressed_payload,
            header.uncompressed_len.try_into().unwrap(),
        )
        .unwrap();

        // Corrupt a byte in a `u64` value so `rkyv` validation still succeeds, and the
        // content hash is what reliably detects the corruption.
        let mut aligned = rkyv::util::AlignedVec::with_capacity(uncompressed.len());
        aligned.extend_from_slice(&uncompressed);
        let archived = rkyv::check_archived_root::<Sample>(&aligned).unwrap();

        let element_ptr = &archived.values[0] as *const u64 as *const u8;
        let payload_ptr = aligned.as_ptr();
        let offset = unsafe { element_ptr.offset_from(payload_ptr) as usize };
        uncompressed[offset] ^= 0x01;

        let mut aligned_corrupted = rkyv::util::AlignedVec::with_capacity(uncompressed.len());
        aligned_corrupted.extend_from_slice(&uncompressed);
        assert!(rkyv::check_archived_root::<Sample>(&aligned_corrupted).is_ok());

        let compressed_corrupted = zstd::bulk::compress(&uncompressed, 0).unwrap();
        header.payload_len = compressed_corrupted.len() as u64;

        let mut output = Vec::with_capacity(HEADER_LEN + compressed_corrupted.len());
        output.extend_from_slice(&header.encode());
        output.extend_from_slice(&compressed_corrupted);
        std::fs::write(&path, &output).unwrap();

        let err =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::HashMismatch { expected, found } => {
                assert_ne!(expected, found);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn oversized_decompressed_len_is_error() {
        let _lock = env_lock();
        let _payload_guard = EnvVarGuard::remove("NOVA_STORAGE_MAX_PAYLOAD_LEN_BYTES");
        let _uncompressed_guard = EnvVarGuard::remove("NOVA_STORAGE_MAX_UNCOMPRESSED_LEN_BYTES");

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
            Compression::Zstd,
        )
        .unwrap();

        // Patch the `uncompressed_len` field in the header to exceed the cap. This should
        // fail fast without attempting to allocate a multi-hundred-MB output buffer.
        //
        // Header layout (little-endian):
        //   payload_len @ 40..48
        //   uncompressed_len @ 48..56
        const UNCOMPRESSED_LEN_OFFSET: u64 = 48;
        let oversized = MAX_UNCOMPRESSED_LEN_BYTES + 1;
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(UNCOMPRESSED_LEN_OFFSET)).unwrap();
        file.write_all(&oversized.to_le_bytes()).unwrap();
        file.sync_all().unwrap();

        let err =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::TooLarge { kind, bytes, limit } => {
                assert_eq!(kind, "uncompressed payload");
                assert_eq!(bytes, oversized);
                assert_eq!(limit, MAX_UNCOMPRESSED_LEN_BYTES);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn oversized_payload_len_is_error() {
        let _lock = env_lock();
        let _payload_limit_guard = EnvVarGuard::set("NOVA_STORAGE_MAX_PAYLOAD_LEN_BYTES", "1024");
        let _uncompressed_guard = EnvVarGuard::remove("NOVA_STORAGE_MAX_UNCOMPRESSED_LEN_BYTES");

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("corrupt.bin");

        let header = StorageHeader::new(
            ArtifactKind::AstArtifacts,
            1,
            Compression::None,
            u64::MAX,
            0,
            0,
        );
        std::fs::write(&path, header.encode()).unwrap();

        let err =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::TooLarge { kind, bytes, limit } => {
                assert_eq!(kind, "payload");
                assert_eq!(bytes, u64::MAX);
                assert_eq!(limit, 1024);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn oversized_uncompressed_len_is_error_for_uncompressed_artifact() {
        let _lock = env_lock();
        let _payload_guard = EnvVarGuard::remove("NOVA_STORAGE_MAX_PAYLOAD_LEN_BYTES");
        let _uncompressed_guard = EnvVarGuard::remove("NOVA_STORAGE_MAX_UNCOMPRESSED_LEN_BYTES");

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

        // Patch the `uncompressed_len` field in the header to exceed the cap. Even though the
        // artifact isn't compressed, we should still reject absurd sizes early.
        const UNCOMPRESSED_LEN_OFFSET: u64 = 48;
        let oversized = MAX_UNCOMPRESSED_LEN_BYTES + 1;
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(UNCOMPRESSED_LEN_OFFSET)).unwrap();
        file.write_all(&oversized.to_le_bytes()).unwrap();
        file.sync_all().unwrap();

        let err =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap_err();
        match err {
            StorageError::TooLarge { kind, bytes, limit } => {
                assert_eq!(kind, "uncompressed payload");
                assert_eq!(bytes, oversized);
                assert_eq!(limit, MAX_UNCOMPRESSED_LEN_BYTES);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn oversized_mmap_fallback_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("large.bin");

        // Create a sparse file larger than the fallback read cap.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        let file_len = crate::persisted::MAX_MMAP_FALLBACK_BYTES + 1;
        file.set_len(file_len).unwrap();

        let err =
            PersistedArchive::<Sample>::open_without_mmap(&path, ArtifactKind::AstArtifacts, 1)
                .unwrap_err();
        match err {
            StorageError::TooLargeForFallbackRead { file_len: got, cap } => {
                assert_eq!(got, file_len);
                assert_eq!(cap, crate::persisted::MAX_MMAP_FALLBACK_BYTES);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn write_archive_atomic_is_safe_under_concurrent_writers() {
        fn render_bytes(value: &Sample) -> Vec<u8> {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("expected.bin");
            write_archive_atomic(
                &path,
                ArtifactKind::AstArtifacts,
                1,
                value,
                Compression::None,
            )
            .unwrap();
            std::fs::read(&path).unwrap()
        }

        let dir = tempfile::TempDir::new().unwrap();
        let path = Arc::new(dir.path().join("concurrent.bin"));

        let value_a = Arc::new(Sample {
            a: 1,
            b: "value-a".to_string(),
            values: vec![0xA5; 4096],
        });
        let value_b = Arc::new(Sample {
            a: 2,
            b: "value-b".to_string(),
            values: vec![0x5A; 4096],
        });

        let expected_a = render_bytes(&value_a);
        let expected_b = render_bytes(&value_b);

        let threads = 8;
        let iterations = 32;
        let barrier = Arc::new(Barrier::new(threads));

        let mut handles = Vec::with_capacity(threads);
        for idx in 0..threads {
            let path = path.clone();
            let value = if idx % 2 == 0 {
                value_a.clone()
            } else {
                value_b.clone()
            };
            let barrier = barrier.clone();

            handles.push(std::thread::spawn(move || -> Result<(), StorageError> {
                let mut error: Option<StorageError> = None;
                for _ in 0..iterations {
                    barrier.wait();
                    if error.is_none() {
                        if let Err(err) = write_archive_atomic(
                            path.as_path(),
                            ArtifactKind::AstArtifacts,
                            1,
                            &*value,
                            Compression::None,
                        ) {
                            error = Some(err);
                        }
                    }
                }
                if let Some(err) = error {
                    Err(err)
                } else {
                    Ok(())
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let bytes = std::fs::read(path.as_path()).unwrap();
        assert!(
            bytes == expected_a || bytes == expected_b,
            "final file payload corrupted (len={})",
            bytes.len()
        );
    }

    #[cfg(windows)]
    #[test]
    fn write_archive_atomic_overwrites_existing_file_on_windows() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("overwrite.bin");

        let first = Sample {
            a: 1,
            b: "before".to_string(),
            values: vec![1],
        };
        let second = Sample {
            a: 2,
            b: "after".to_string(),
            values: vec![2],
        };

        write_archive_atomic(
            &path,
            ArtifactKind::AstArtifacts,
            1,
            &first,
            Compression::None,
        )
        .unwrap();
        write_archive_atomic(
            &path,
            ArtifactKind::AstArtifacts,
            1,
            &second,
            Compression::None,
        )
        .unwrap();

        let loaded =
            PersistedArchive::<Sample>::open(&path, ArtifactKind::AstArtifacts, 1).unwrap();
        assert_eq!(loaded.to_owned().unwrap(), second);
    }
}
