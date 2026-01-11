use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::header::{ArtifactKind, Compression, StorageHeader};
use crate::persisted::StorageError;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    let dir = path
        .parent()
        .ok_or(StorageError::InvalidHeader("missing parent directory"))?;
    fs::create_dir_all(dir)?;

    let archived =
        rkyv::to_bytes::<_, 256>(value).map_err(|e| StorageError::Validation(e.to_string()))?;
    let uncompressed = archived.as_slice();

    let hash_bytes = blake3::hash(uncompressed);
    let content_hash =
        u64::from_le_bytes(hash_bytes.as_bytes()[..8].try_into().expect("hash slice"));

    let (payload_bytes, uncompressed_len) = match compression {
        Compression::None => (uncompressed.to_vec(), uncompressed.len() as u64),
        Compression::Zstd => {
            let compressed = zstd::bulk::compress(uncompressed, 0)
                .map_err(|e| StorageError::Decompression(e.to_string()))?;
            (compressed, uncompressed.len() as u64)
        }
    };

    let header = StorageHeader::new(
        kind,
        schema_version,
        compression,
        payload_bytes.len() as u64,
        uncompressed_len,
        content_hash,
    );

    atomic_write(path, &header.encode(), &payload_bytes)
}

fn atomic_write(dest: &Path, header: &[u8], payload: &[u8]) -> Result<(), StorageError> {
    let parent = dest
        .parent()
        .ok_or(StorageError::InvalidHeader("missing parent directory"))?;
    let (tmp_path, mut file) = open_unique_tmp_file(dest, parent)?;

    if let Err(err) = file
        .write_all(header)
        .and_then(|()| file.write_all(payload))
        .and_then(|()| file.sync_all())
        .and_then(|()| Ok(()))
    {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(StorageError::from(err));
    }
    drop(file);

    const MAX_RENAME_ATTEMPTS: usize = 1024;
    let rename_result = (|| -> io::Result<()> {
        let mut attempts = 0usize;
        loop {
            match fs::rename(&tmp_path, dest) {
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
    })();

    match rename_result {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp_path);
            Err(StorageError::from(err))
        }
    }
}

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let file_name = dest.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "destination path has no file name")
    })?;
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
