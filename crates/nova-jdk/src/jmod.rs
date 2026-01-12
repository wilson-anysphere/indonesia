use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use thiserror::Error;
use zip::ZipArchive;

const JMOD_HEADER_LEN: u64 = 4;
const CLASSES_PREFIX: &str = "classes/";
const MODULE_INFO_INTERNAL_NAME: &str = "module-info";

pub(crate) struct OffsetReader<R> {
    inner: R,
    base: u64,
}

impl<R> OffsetReader<R>
where
    R: Seek,
{
    fn new(mut inner: R, base: u64) -> std::io::Result<Self> {
        inner.seek(SeekFrom::Start(base))?;
        Ok(Self { inner, base })
    }
}

impl<R> Read for OffsetReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R> Seek for OffsetReader<R>
where
    R: Seek,
{
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let base = self.base;
        let adjusted = match pos {
            SeekFrom::Start(offset) => {
                SeekFrom::Start(offset.checked_add(base).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek overflow")
                })?)
            }
            SeekFrom::End(offset) => SeekFrom::End(offset),
            SeekFrom::Current(offset) => SeekFrom::Current(offset),
        };

        let absolute = self.inner.seek(adjusted)?;
        absolute.checked_sub(base).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before archive start",
            )
        })
    }
}

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

pub fn read_class_bytes(
    jmod_path: &Path,
    internal_name: &str,
) -> Result<Option<Vec<u8>>, JmodError> {
    let mut archive = open_archive(jmod_path)?;
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

pub fn read_module_info_class_bytes(jmod_path: &Path) -> Result<Option<Vec<u8>>, JmodError> {
    read_class_bytes(jmod_path, MODULE_INFO_INTERNAL_NAME)
}

pub fn open_archive(jmod_path: &Path) -> Result<ZipArchive<OffsetReader<File>>, JmodError> {
    let mut file = File::open(jmod_path)?;
    let mut header = [0u8; 2];
    let is_jmod_magic = file.read_exact(&mut header).is_ok() && header == *b"JM";
    file.seek(SeekFrom::Start(0))?;

    // First attempt: interpret as a normal zip. This works for many `.jmod` files, even if they
    // have a `JM<version>` preamble (zip archives can legally have a prefix before the first local
    // file header as long as the central directory offsets are relative to the file start).
    let reader = OffsetReader::new(file, 0)?;
    match ZipArchive::new(reader) {
        Ok(mut archive) => {
            if !is_jmod_magic {
                return Ok(archive);
            }

            // Some `.jmod` files store zip offsets relative to the embedded zip payload (after the
            // 4-byte `JM<version>` header). If local header parsing fails, retry with an offset
            // reader.
            if archive.is_empty() || archive.by_index(0).is_ok() {
                return Ok(archive);
            }
        }
        Err(err) => {
            if !is_jmod_magic {
                return Err(err.into());
            }
        }
    }

    // JMOD fallback: interpret zip offsets relative to the embedded zip payload.
    let file = File::open(jmod_path)?;
    let reader = OffsetReader::new(file, JMOD_HEADER_LEN)?;
    Ok(ZipArchive::new(reader)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::io::Write;
    use tempfile::TempDir;
    use zip::write::FileOptions;

    #[test]
    fn reads_entries_from_jmod_archives_with_magic_header() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("demo.jmod");

        // Create a zip payload where offsets are relative to the start of the payload.
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            zip.start_file("classes/module-info.class", FileOptions::default())
                .unwrap();
            zip.write_all(b"hello-jmod").unwrap();
            zip.finish().unwrap();
        }
        let zip_bytes = cursor.into_inner();

        // Prefix with `JM<version>` so the zip payload starts at offset 4.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"JM\x01\x00");
        bytes.extend_from_slice(&zip_bytes);
        std::fs::write(&path, bytes).unwrap();

        let contents = read_module_info_class_bytes(&path)
            .unwrap()
            .expect("module-info.class");
        assert_eq!(contents, b"hello-jmod");
    }
}
