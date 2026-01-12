use std::fs::File;
use std::io::Read;
use std::path::Path;

use thiserror::Error;
use zip::ZipArchive;

#[derive(Debug, Error)]
pub enum JarError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

pub fn class_entry_name(internal_name: &str) -> String {
    format!("{internal_name}.class")
}

pub fn entry_to_internal_name(entry_name: &str) -> Option<&str> {
    if !entry_name.ends_with(".class") {
        return None;
    }

    entry_name
        .strip_prefix('/')
        .unwrap_or(entry_name)
        .strip_suffix(".class")
}

pub fn read_class_bytes(jar_path: &Path, internal_name: &str) -> Result<Option<Vec<u8>>, JarError> {
    let file = File::open(jar_path)?;
    let mut archive = ZipArchive::new(file)?;
    let entry_name = class_entry_name(internal_name);

    let res = match archive.by_name(&entry_name) {
        Ok(mut zf) => {
            let mut bytes = Vec::with_capacity(zf.size() as usize);
            zf.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(e.into()),
    };

    res
}

pub fn open_archive(jar_path: &Path) -> Result<ZipArchive<File>, JarError> {
    let file = File::open(jar_path)?;
    Ok(ZipArchive::new(file)?)
}
