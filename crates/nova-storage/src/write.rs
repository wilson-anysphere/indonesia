use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::header::{ArtifactKind, Compression, StorageHeader};
use crate::persisted::StorageError;

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
    let tmp_path = dest.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(header)?;
        file.write_all(payload)?;
        file.sync_all()?;
    }

    match fs::rename(&tmp_path, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists || dest.exists() => {
            // On Windows, rename doesn't overwrite. Try remove + rename.
            let _ = fs::remove_file(dest);
            fs::rename(&tmp_path, dest).map_err(StorageError::from)
        }
        Err(err) => Err(StorageError::from(err)),
    }
}
