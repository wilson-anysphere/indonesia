use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::header::{ArtifactKind, Compression, StorageHeader, HEADER_LEN};
use crate::persisted::StorageError;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCompression {
    None,
    Zstd { level: i32 },
    /// Automatically selects compression based on the uncompressed archive size.
    ///
    /// If the archived payload is at least `threshold` bytes, zstd compression is
    /// used with the default zstd level (0). Otherwise, no compression is used.
    Auto { threshold: u64 },
}

impl Default for WriteCompression {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteArchiveOptions {
    pub compression: WriteCompression,
    /// When enabled, re-reads the temp file and verifies payload size and
    /// content hash (streaming decompress for zstd).
    pub validate_after_write: bool,
}

impl Default for WriteArchiveOptions {
    fn default() -> Self {
        Self {
            compression: WriteCompression::None,
            validate_after_write: false,
        }
    }
}

pub fn write_archive_atomic<T>(
    path: &Path,
    kind: ArtifactKind,
    schema_version: u32,
    value: &T,
    compression: Compression,
) -> Result<(), StorageError>
where
    T: rkyv::Archive + rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<256>>,
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
    T: rkyv::Archive + rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<256>>,
{
    let parent = path
        .parent()
        .ok_or(StorageError::InvalidHeader("missing parent directory"))?;
    fs::create_dir_all(parent)?;

    // Note: `rkyv::to_bytes` allocates the final archive, but we avoid
    // additional full-sized buffers by streaming the payload to disk and using
    // zstd's streaming encoder (rather than `bulk::compress`).
    let archived =
        rkyv::to_bytes::<_, 256>(value).map_err(|e| StorageError::Validation(e.to_string()))?;
    let uncompressed = archived.as_slice();
    let uncompressed_len = uncompressed.len() as u64;

    let (compression, zstd_level) = match options.compression {
        WriteCompression::None => (Compression::None, None),
        WriteCompression::Zstd { level } => (Compression::Zstd, Some(level)),
        WriteCompression::Auto { threshold } => {
            if uncompressed_len >= threshold {
                (Compression::Zstd, Some(0))
            } else {
                (Compression::None, None)
            }
        }
    };

    let content_hash = content_hash(uncompressed);

    let (tmp_path, file) = open_unique_tmp_file(path, parent)?;

    let result = (|| -> Result<(), StorageError> {
        let (mut file, payload_len) = write_payload(file, uncompressed, compression, zstd_level)?;

        let header = StorageHeader::new(
            kind,
            schema_version,
            compression,
            payload_len,
            uncompressed_len,
            content_hash,
        );

        // Overwrite the placeholder header now that we know the final metadata.
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header.encode())?;
        file.sync_all()?;
        drop(file);

        if options.validate_after_write {
            validate_written_file(&tmp_path, &header)?;
        }

        rename_overwrite(&tmp_path, path).map_err(StorageError::from)
    })();

    if let Err(err) = result {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

fn write_payload(
    mut file: fs::File,
    payload: &[u8],
    compression: Compression,
    zstd_level: Option<i32>,
) -> Result<(fs::File, u64), StorageError> {
    file.write_all(&[0u8; HEADER_LEN])?;

    match compression {
        Compression::None => {
            file.write_all(payload)?;
            Ok((file, payload.len() as u64))
        }
        Compression::Zstd => {
            let level = zstd_level.unwrap_or(0);
            let mut encoder = zstd::stream::write::Encoder::new(file, level)
                .map_err(|e| StorageError::Decompression(e.to_string()))?;
            encoder.write_all(payload)?;
            let mut file = encoder
                .finish()
                .map_err(|e| StorageError::Decompression(e.to_string()))?;
            file.seek(SeekFrom::End(0))?;
            let end = file.stream_position()?;
            let payload_len = end
                .checked_sub(HEADER_LEN as u64)
                .ok_or(StorageError::InvalidHeader("payload length underflow"))?;
            Ok((file, payload_len))
        }
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
    let found =
        u64::from_le_bytes(hash_bytes.as_bytes()[..8].try_into().expect("hash slice"));
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
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists || dest.exists() => {
                // On Windows, `rename` doesn't overwrite. Under concurrent writers,
                // multiple `remove + rename` sequences can race; retry until we win.
                let _ = fs::remove_file(dest);

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

fn content_hash(payload: &[u8]) -> u64 {
    let hash_bytes = blake3::hash(payload);
    u64::from_le_bytes(hash_bytes.as_bytes()[..8].try_into().expect("hash slice"))
}
