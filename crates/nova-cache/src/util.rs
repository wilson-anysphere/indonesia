use crate::error::CacheError;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "path has no parent").into());
    };

    std::fs::create_dir_all(parent)?;

    let tmp_path = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(_err) if path.exists() => {
            // On Windows, rename doesn't overwrite. Try remove + rename.
            std::fs::remove_file(path)?;
            std::fs::rename(&tmp_path, path).map_err(CacheError::from)
        }
        Err(err) => Err(CacheError::from(err)),
    }
}
