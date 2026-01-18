use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use rkyv::ser::serializers::{
    AllocScratch, CompositeSerializer, FallbackScratch, HeapScratch, SharedSerializeMap,
    WriteSerializer,
};
use rkyv::ser::Serializer as _;

use crate::header::{ArtifactKind, Compression, StorageHeader, HEADER_LEN};
use crate::persisted::StorageError;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub type FileArchiveSerializer = CompositeSerializer<
    WriteSerializer<fs::File>,
    FallbackScratch<HeapScratch<256>, AllocScratch>,
    SharedSerializeMap,
>;

/// Trait alias for values that can be serialized by `nova-storage`'s streaming
/// writer.
///
/// This hides the internal `rkyv` serializer type from downstream crates so
/// they can write generic helpers without repeating an unwieldy type in their
/// bounds.
pub trait WritableArchive: rkyv::Archive + rkyv::Serialize<FileArchiveSerializer> {}

impl<T> WritableArchive for T where T: rkyv::Archive + rkyv::Serialize<FileArchiveSerializer> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteCompression {
    #[default]
    None,
    Zstd {
        level: i32,
    },
    /// Automatically selects compression based on the uncompressed archive size.
    ///
    /// If the archived payload is at least `threshold` bytes, zstd compression is
    /// used with the default zstd level (0). Otherwise, no compression is used.
    Auto {
        threshold: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteArchiveOptions {
    pub compression: WriteCompression,
    /// When enabled, re-reads the temp file and verifies payload size and
    /// content hash (streaming decompress for zstd).
    pub validate_after_write: bool,
}

pub fn write_archive_atomic<T>(
    path: &Path,
    kind: ArtifactKind,
    schema_version: u32,
    value: &T,
    compression: Compression,
) -> Result<(), StorageError>
where
    T: WritableArchive,
{
    let options = WriteArchiveOptions {
        compression: match compression {
            Compression::None => WriteCompression::None,
            Compression::Zstd => WriteCompression::Zstd { level: 0 },
        },
        validate_after_write: false,
    };

    write_archive_atomic_with_options(path, kind, schema_version, value, options)
}

pub fn write_archive_atomic_with_options<T>(
    path: &Path,
    kind: ArtifactKind,
    schema_version: u32,
    value: &T,
    options: WriteArchiveOptions,
) -> Result<(), StorageError>
where
    T: WritableArchive,
{
    let parent = path
        .parent()
        .ok_or(StorageError::InvalidHeader("missing parent directory"))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    fs::create_dir_all(parent)?;

    let (tmp_path, file) = open_unique_tmp_file(path, parent)?;

    let result = (|| -> Result<(), StorageError> {
        let (mut file, uncompressed_len) = write_uncompressed_archive(file, value)?;

        enum SelectedCompression {
            None,
            Zstd { level: i32 },
        }

        let selected = match options.compression {
            WriteCompression::None => SelectedCompression::None,
            WriteCompression::Zstd { level } => SelectedCompression::Zstd { level },
            WriteCompression::Auto { threshold } => {
                if uncompressed_len >= threshold {
                    SelectedCompression::Zstd { level: 0 }
                } else {
                    SelectedCompression::None
                }
            }
        };

        match selected {
            SelectedCompression::None => {
                let content_hash = hash_uncompressed_payload(&mut file, uncompressed_len)?;
                let header = StorageHeader::new(
                    kind,
                    schema_version,
                    Compression::None,
                    uncompressed_len,
                    uncompressed_len,
                    content_hash,
                );

                file.seek(SeekFrom::Start(0))?;
                file.write_all(&header.encode())?;
                file.sync_all()?;
                drop(file);

                if options.validate_after_write {
                    validate_written_file(&tmp_path, &header)?;
                }

                rename_overwrite(&tmp_path, path).map_err(StorageError::from)?;
                sync_dir_best_effort(parent, "storage.write_archive.sync_parent_dir");
                Ok(())
            }
            SelectedCompression::Zstd { level } => {
                drop(file);

                let (compressed_path, compressed_file) = open_unique_tmp_file(path, parent)?;

                let compressed_result = (|| -> Result<(), StorageError> {
                    let (mut compressed_file, payload_len, content_hash) =
                        compress_uncompressed_tmp(
                            &tmp_path,
                            compressed_file,
                            uncompressed_len,
                            level,
                        )?;

                    let header = StorageHeader::new(
                        kind,
                        schema_version,
                        Compression::Zstd,
                        payload_len,
                        uncompressed_len,
                        content_hash,
                    );

                    compressed_file.seek(SeekFrom::Start(0))?;
                    compressed_file.write_all(&header.encode())?;
                    compressed_file.sync_all()?;
                    drop(compressed_file);

                    if options.validate_after_write {
                        validate_written_file(&compressed_path, &header)?;
                    }

                    rename_overwrite(&compressed_path, path).map_err(StorageError::from)
                })();

                // Best-effort cleanup of the intermediate uncompressed temp file.
                if let Err(remove_err) = fs::remove_file(&tmp_path) {
                    if remove_err.kind() != std::io::ErrorKind::NotFound {
                        tracing::debug!(
                            target = "nova.storage",
                            path = %tmp_path.display(),
                            error = %remove_err,
                            "failed to remove temporary file after compression"
                        );
                    }
                }

                if let Err(err) = compressed_result {
                    if let Err(remove_err) = fs::remove_file(&compressed_path) {
                        if remove_err.kind() != std::io::ErrorKind::NotFound {
                            tracing::debug!(
                                target = "nova.storage",
                                path = %compressed_path.display(),
                                error = %remove_err,
                                "failed to remove compressed file after write failure"
                            );
                        }
                    }
                    return Err(err);
                }

                sync_dir_best_effort(parent, "storage.write_archive.sync_parent_dir");
                Ok(())
            }
        }
    })();

    if let Err(err) = result {
        if let Err(remove_err) = fs::remove_file(&tmp_path) {
            if remove_err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.storage",
                    path = %tmp_path.display(),
                    error = %remove_err,
                    "failed to remove temporary file after write failure"
                );
            }
        }
        return Err(err);
    }

    Ok(())
}

#[track_caller]
fn sync_dir_best_effort(dir: &Path, reason: &'static str) {
    #[cfg(unix)]
    static SYNC_DIR_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    #[cfg(unix)]
    {
        match fs::File::open(dir).and_then(|dir| dir.sync_all()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                if SYNC_DIR_ERROR_LOGGED.set(()).is_ok() {
                    let loc = std::panic::Location::caller();
                    tracing::debug!(
                        target = "nova.storage",
                        dir = %dir.display(),
                        reason,
                        file = loc.file(),
                        line = loc.line(),
                        column = loc.column(),
                        error = %err,
                        "failed to sync directory (best effort)"
                    );
                }
            }
        }
    }

    #[cfg(not(unix))]
    let _ = (dir, reason);
}

fn write_uncompressed_archive<T>(
    mut file: fs::File,
    value: &T,
) -> Result<(fs::File, u64), StorageError>
where
    T: rkyv::Serialize<FileArchiveSerializer>,
{
    // Placeholder header.
    file.write_all(&[0u8; HEADER_LEN])?;

    let mut serializer = CompositeSerializer::new(
        WriteSerializer::with_pos(file, 0),
        FallbackScratch::new(HeapScratch::<256>::new(), AllocScratch::default()),
        SharedSerializeMap::new(),
    );

    serializer.serialize_value(value).map_err(map_rkyv_error)?;

    let uncompressed_len = serializer.pos() as u64;

    let file = serializer.into_serializer().into_inner();

    Ok((file, uncompressed_len))
}

fn hash_uncompressed_payload(
    file: &mut fs::File,
    uncompressed_len: u64,
) -> Result<u64, StorageError> {
    file.seek(SeekFrom::Start(HEADER_LEN as u64))?;
    let mut limited = file.take(uncompressed_len);

    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 128 * 1024];
    let mut remaining = uncompressed_len;
    while remaining > 0 {
        let to_read = (remaining.min(buf.len() as u64)) as usize;
        let n = limited.read(&mut buf[..to_read])?;
        if n == 0 {
            break;
        }
        remaining -= n as u64;
        hasher.update(&buf[..n]);
    }
    if remaining != 0 {
        return Err(StorageError::Truncated {
            expected: uncompressed_len as usize,
            found: (uncompressed_len - remaining) as usize,
        });
    }

    let hash_bytes = hasher.finalize();
    Ok(u64::from_le_bytes(
        hash_bytes.as_bytes()[..8].try_into().expect("hash slice"),
    ))
}

fn compress_uncompressed_tmp(
    uncompressed_path: &Path,
    mut compressed_file: fs::File,
    uncompressed_len: u64,
    level: i32,
) -> Result<(fs::File, u64, u64), StorageError> {
    compressed_file.write_all(&[0u8; HEADER_LEN])?;

    let mut src = fs::File::open(uncompressed_path)?;
    src.seek(SeekFrom::Start(HEADER_LEN as u64))?;
    let mut limited = src.take(uncompressed_len);

    let mut encoder = zstd::stream::write::Encoder::new(compressed_file, level)
        .map_err(|e| StorageError::Decompression(e.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 128 * 1024];
    let mut remaining = uncompressed_len;
    while remaining > 0 {
        let to_read = (remaining.min(buf.len() as u64)) as usize;
        let n = limited.read(&mut buf[..to_read])?;
        if n == 0 {
            break;
        }
        remaining -= n as u64;
        hasher.update(&buf[..n]);
        encoder.write_all(&buf[..n])?;
    }
    if remaining != 0 {
        return Err(StorageError::Truncated {
            expected: uncompressed_len as usize,
            found: (uncompressed_len - remaining) as usize,
        });
    }

    let mut compressed_file = encoder
        .finish()
        .map_err(|e| StorageError::Decompression(e.to_string()))?;
    compressed_file.seek(SeekFrom::End(0))?;
    let end = compressed_file.stream_position()?;
    let payload_len = end
        .checked_sub(HEADER_LEN as u64)
        .ok_or(StorageError::InvalidHeader("payload length underflow"))?;

    let content_hash = {
        let hash_bytes = hasher.finalize();
        u64::from_le_bytes(hash_bytes.as_bytes()[..8].try_into().expect("hash slice"))
    };

    Ok((compressed_file, payload_len, content_hash))
}

fn map_rkyv_error<C, H>(
    err: rkyv::ser::serializers::CompositeSerializerError<std::io::Error, C, H>,
) -> StorageError
where
    C: std::fmt::Display,
    H: std::fmt::Display,
{
    match err {
        rkyv::ser::serializers::CompositeSerializerError::SerializerError(err) => {
            StorageError::Io(err)
        }
        other => StorageError::Validation(other.to_string()),
    }
}

fn validate_written_file(path: &Path, expected_header: &StorageHeader) -> Result<(), StorageError> {
    let mut file = fs::File::open(path)?;
    let metadata_len = file.metadata()?.len();
    if metadata_len < HEADER_LEN as u64 {
        return Err(StorageError::Truncated {
            expected: HEADER_LEN,
            found: metadata_len as usize,
        });
    }

    let mut header_bytes = [0u8; HEADER_LEN];
    file.read_exact(&mut header_bytes)?;
    let header = StorageHeader::decode(&header_bytes)?;

    // Ensure the header we wrote matches what we intended to write.
    if &header != expected_header {
        return Err(StorageError::InvalidHeader("header mismatch after write"));
    }

    let expected_file_len = (HEADER_LEN as u64)
        .checked_add(header.payload_len)
        .ok_or(StorageError::InvalidHeader("payload length overflow"))?;
    if metadata_len < expected_file_len {
        return Err(StorageError::Truncated {
            expected: expected_file_len as usize,
            found: metadata_len as usize,
        });
    }

    file.seek(SeekFrom::Start(HEADER_LEN as u64))?;
    let mut payload_reader: Box<dyn Read> = match header.compression {
        Compression::None => Box::new(file.take(header.payload_len)),
        Compression::Zstd => {
            let take = file.take(header.payload_len);
            let decoder = zstd::stream::read::Decoder::new(take)
                .map_err(|e| StorageError::Decompression(e.to_string()))?;
            Box::new(decoder)
        }
    };

    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 128 * 1024];
    let mut decompressed_len: u64 = 0;
    loop {
        let n = payload_reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        decompressed_len += n as u64;
    }

    if decompressed_len != header.uncompressed_len {
        return Err(StorageError::InvalidHeader("uncompressed length mismatch"));
    }

    let hash_bytes = hasher.finalize();
    let found = u64::from_le_bytes(hash_bytes.as_bytes()[..8].try_into().expect("hash slice"));
    if found != header.content_hash {
        return Err(StorageError::HashMismatch {
            expected: header.content_hash,
            found,
        });
    }

    Ok(())
}

fn rename_overwrite(tmp_path: &Path, dest: &Path) -> io::Result<()> {
    const MAX_RENAME_ATTEMPTS: usize = 1024;
    let mut attempts = 0usize;

    loop {
        match fs::rename(tmp_path, dest) {
            Ok(()) => return Ok(()),
            Err(err)
                if cfg!(windows)
                    && (err.kind() == io::ErrorKind::AlreadyExists || dest.exists()) =>
            {
                // On Windows, `rename` doesn't overwrite. Under concurrent writers,
                // multiple `remove + rename` sequences can race; retry until we win.
                match fs::remove_file(dest) {
                    Ok(()) => {}
                    Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                    Err(remove_err) => return Err(remove_err),
                }

                attempts += 1;
                if attempts >= MAX_RENAME_ATTEMPTS {
                    return Err(err);
                }

                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| io::Error::other("destination path has no file name"))?;
    let pid = std::process::id();

    loop {
        let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp.{pid}.{counter}"));
        let tmp_path = parent.join(tmp_name);

        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}
