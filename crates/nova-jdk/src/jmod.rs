use std::fs::File;
use std::io::Read;
use std::path::Path;

use thiserror::Error;
use zip::ZipArchive;

const CLASSES_PREFIX: &str = "classes/";

#[derive(Debug, Error)]
pub enum JmodError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

pub fn class_entry_name(internal_name: &str) -> String {
    format!("{CLASSES_PREFIX}{internal_name}.class")
}

pub fn entry_to_internal_name(entry_name: &str) -> Option<&str> {
    if !entry_name.starts_with(CLASSES_PREFIX) || !entry_name.ends_with(".class") {
        return None;
    }

    entry_name
        .strip_prefix(CLASSES_PREFIX)
        .and_then(|s| s.strip_suffix(".class"))
}

pub fn read_class_bytes(jmod_path: &Path, internal_name: &str) -> Result<Option<Vec<u8>>, JmodError> {
    let file = File::open(jmod_path)?;
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

pub fn open_archive(jmod_path: &Path) -> Result<ZipArchive<File>, JmodError> {
    let file = File::open(jmod_path)?;
    Ok(ZipArchive::new(file)?)
}
