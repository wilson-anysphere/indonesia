#![allow(dead_code)]

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use thiserror::Error;
use zip::ZipArchive;

const META_INF_SYM_PREFIX: &str = "META-INF/sym/";

#[derive(Debug, Error)]
pub enum CtSymError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CtSymExt {
    Sig,
    Class,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CtSymEntry {
    pub(crate) release: u32,
    pub(crate) module: String,
    pub(crate) internal_name: String,
    pub(crate) zip_path: String,
    pub(crate) ext: CtSymExt,
}

pub(crate) fn parse_entry_name(name: &str) -> Option<CtSymEntry> {
    static CT_SYM_RELEASE_PARSE_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    let name = name.trim_start_matches('/');
    if name.is_empty() || name.ends_with('/') {
        return None;
    }

    let (ext, name_without_ext) = if let Some(stripped) = name.strip_suffix(".sig") {
        (CtSymExt::Sig, stripped)
    } else if let Some(stripped) = name.strip_suffix(".class") {
        (CtSymExt::Class, stripped)
    } else {
        return None;
    };

    let normalized = name_without_ext
        .strip_prefix(META_INF_SYM_PREFIX)
        .unwrap_or(name_without_ext);

    let mut parts = normalized.splitn(3, '/');
    let release_str = parts.next()?;
    let module = parts.next()?;
    let internal = parts.next()?;

    if release_str.is_empty() || module.is_empty() || internal.is_empty() {
        return None;
    }

    let release = match release_str.parse::<u32>() {
        Ok(release) => release,
        Err(err) => {
            if CT_SYM_RELEASE_PARSE_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.jdk",
                    release = %release_str,
                    zip_path = %name,
                    error = %err,
                    "failed to parse ct.sym release; ignoring entry (best effort)"
                );
            }
            return None;
        }
    };

    Some(CtSymEntry {
        release,
        module: module.to_owned(),
        internal_name: internal.to_owned(),
        zip_path: name.to_owned(),
        ext,
    })
}

pub(crate) fn open_archive(ct_sym_path: &Path) -> Result<ZipArchive<File>, CtSymError> {
    let file = File::open(ct_sym_path)?;
    Ok(ZipArchive::new(file)?)
}

pub(crate) fn read_entry_bytes(
    ct_sym_path: &Path,
    zip_path: &str,
) -> Result<Option<Vec<u8>>, CtSymError> {
    let file = File::open(ct_sym_path)?;
    let mut archive = ZipArchive::new(file)?;
    read_entry_bytes_from_archive(&mut archive, zip_path)
}

pub(crate) fn read_entry_bytes_from_archive(
    archive: &mut ZipArchive<File>,
    zip_path: &str,
) -> Result<Option<Vec<u8>>, CtSymError> {
    let mut try_read = |name: &str| -> Result<Option<Vec<u8>>, CtSymError> {
        match archive.by_name(name) {
            Ok(mut zf) => {
                let mut bytes = Vec::with_capacity(zf.size() as usize);
                zf.read_to_end(&mut bytes)?;
                Ok(Some(bytes))
            }
            Err(zip::result::ZipError::FileNotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    };

    if let Some(bytes) = try_read(zip_path)? {
        return Ok(Some(bytes));
    }

    let normalized = zip_path.trim_start_matches('/');
    if normalized != zip_path {
        return try_read(normalized);
    }

    let alt = format!("/{zip_path}");
    try_read(&alt)
}
