//! Abstractions for reading dependency archives (JARs) and exploded directories.
//!
//! In the full Nova system, this would support classpath caching and efficient
//! archive access. For configuration metadata indexing we only need best-effort
//! file reads.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use zip::ZipArchive;

/// Maximum number of bytes we will read from any single archive entry.
///
/// `nova-archive` is used to read small files (e.g. `.class` files, manifests,
/// Spring configuration metadata JSON) out of *untrusted* third-party
/// dependencies.
///
/// A JAR/zip "zip bomb" can be a tiny compressed blob that expands to gigabytes
/// of output. Without a hard cap, `read_to_end` will keep growing the buffer
/// until we OOM.
///
/// 16MiB is intentionally generous for the kinds of entries we read today while
/// still providing a strong safety net against decompression bombs.
const MAX_ARCHIVE_ENTRY_BYTES: usize = 16 * 1024 * 1024;

const JMOD_HEADER_LEN: u64 = 4;

struct OffsetReader<R> {
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

#[derive(Clone, Debug)]
pub struct Archive {
    path: PathBuf,
}

impl Archive {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read a file from the archive.
    ///
    /// Returns `Ok(None)` when the file isn't present.
    pub fn read(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        if self.path.is_dir() {
            let candidate = self.path.join(name);
            if !candidate.exists() {
                return Ok(None);
            }

            let len = candidate
                .metadata()
                .with_context(|| format!("failed to stat {}", candidate.display()))?
                .len();
            if len > MAX_ARCHIVE_ENTRY_BYTES as u64 {
                bail!(
                    "refusing to read {} bytes from {} (max allowed: {} bytes)",
                    len,
                    candidate.display(),
                    MAX_ARCHIVE_ENTRY_BYTES
                );
            }

            let mut file = File::open(&candidate)
                .with_context(|| format!("failed to open {}", candidate.display()))?;

            // Defense in depth: even if the file changes while we read it (or the
            // metadata is otherwise misleading), never read more than MAX+1 bytes.
            let mut limited = (&mut file).take((MAX_ARCHIVE_ENTRY_BYTES as u64) + 1);
            let mut buf = Vec::with_capacity(len as usize);
            limited
                .read_to_end(&mut buf)
                .with_context(|| format!("failed to read {}", candidate.display()))?;
            if buf.len() > MAX_ARCHIVE_ENTRY_BYTES {
                bail!(
                    "refusing to read more than {} bytes from {} (read: {} bytes)",
                    MAX_ARCHIVE_ENTRY_BYTES,
                    candidate.display(),
                    buf.len()
                );
            }

            return Ok(Some(buf));
        }

        fn read_from_zip<R: Read + Seek>(
            zip: &mut ZipArchive<R>,
            archive_path: &Path,
            name: &str,
        ) -> anyhow::Result<Option<Vec<u8>>> {
            let result = match zip.by_name(name) {
                Ok(mut entry) => {
                    let declared_size = entry.size();
                    if declared_size > MAX_ARCHIVE_ENTRY_BYTES as u64 {
                        bail!(
                            "refusing to read {} from {}: declared uncompressed size is {} bytes (max allowed: {} bytes)",
                            name,
                            archive_path.display(),
                            declared_size,
                            MAX_ARCHIVE_ENTRY_BYTES
                        );
                    }

                    // Allocate only up to the declared size (which is already bounded)
                    // to avoid oversized allocations from untrusted metadata.
                    let mut buf = Vec::with_capacity(declared_size as usize);

                    // Defense in depth: even if the declared size is wrong or missing,
                    // never read more than MAX+1 bytes from the decompressor.
                    let mut limited = (&mut entry).take((MAX_ARCHIVE_ENTRY_BYTES as u64) + 1);
                    limited.read_to_end(&mut buf).with_context(|| {
                        format!("failed to read {} from {}", name, archive_path.display())
                    })?;
                    if buf.len() > MAX_ARCHIVE_ENTRY_BYTES {
                        bail!(
                            "refusing to read more than {} bytes from {}:{} (read: {} bytes)",
                            MAX_ARCHIVE_ENTRY_BYTES,
                            archive_path.display(),
                            name,
                            buf.len()
                        );
                    }

                    Ok(Some(buf))
                }
                Err(zip::result::ZipError::FileNotFound) => Ok(None),
                Err(err) => Err(err).with_context(|| {
                    format!(
                        "failed to read {} from zip {}",
                        name,
                        archive_path.display()
                    )
                }),
            };

            result
        }

        let mut file = File::open(&self.path)
            .with_context(|| format!("failed to open archive {}", self.path.display()))?;
        let mut header = [0u8; 2];
        let is_jmod_magic = file.read_exact(&mut header).is_ok() && header == *b"JM";
        file.seek(SeekFrom::Start(0))
            .with_context(|| format!("failed to seek {}", self.path.display()))?;

        // Prefer opening as a standard zip first. This works for:
        // - regular jar/zip archives
        // - `.jmod` archives that are also valid zips (including those with a preamble where zip
        //   offsets are relative to the actual file start).
        //
        // If the archive starts with the JMOD magic (`JM`) but can't be read as a standard zip,
        // fall back to treating it as a zip whose offsets are relative to the zip payload that
        // begins after the 4-byte header.
        match ZipArchive::new(file) {
            Ok(mut zip) => match read_from_zip(&mut zip, &self.path, name) {
                Ok(res) => return Ok(res),
                Err(err) if !is_jmod_magic => return Err(err),
                Err(_) => {}
            },
            Err(err) if !is_jmod_magic => {
                return Err(err).with_context(|| format!("failed to read zip {}", self.path.display()))
            }
            Err(_) => {}
        }

        // JMOD fallback: interpret the zip offsets relative to the start of the embedded zip
        // payload (after the `JM<version>` header).
        let file = File::open(&self.path)
            .with_context(|| format!("failed to open archive {}", self.path.display()))?;
        let reader = OffsetReader::new(file, JMOD_HEADER_LEN)
            .with_context(|| format!("failed to seek {}", self.path.display()))?;
        let mut zip = ZipArchive::new(reader)
            .with_context(|| format!("failed to read zip {}", self.path.display()))?;
        read_from_zip(&mut zip, &self.path, name)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Write;

    use tempfile::TempDir;
    use zip::write::FileOptions;

    use super::{Archive, MAX_ARCHIVE_ENTRY_BYTES};

    #[test]
    fn rejects_oversized_directory_entries_via_metadata() {
        let tmp = TempDir::new().unwrap();

        let path = tmp.path().join("too-large.bin");
        let file = std::fs::File::create(&path).unwrap();

        // Sparse file; does not actually allocate MAX+1 bytes on disk.
        file.set_len((MAX_ARCHIVE_ENTRY_BYTES as u64) + 1).unwrap();

        let archive = Archive::new(tmp.path());
        let err = archive.read("too-large.bin").unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("refusing to read"));
        assert!(msg.contains("max allowed"));
        assert!(msg.contains("too-large.bin"));
    }

    #[test]
    fn reads_entries_from_jmod_archives() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("demo.jmod");

        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            zip.start_file("classes/module-info.class", FileOptions::<()>::default())
                .unwrap();
            zip.write_all(b"hello-jmod").unwrap();
            zip.finish().unwrap();
        }
        let zip_bytes = cursor.into_inner();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"JM\x01\x00");
        bytes.extend_from_slice(&zip_bytes);
        std::fs::write(&path, bytes).unwrap();

        let archive = Archive::new(&path);
        let contents = archive
            .read("classes/module-info.class")
            .unwrap()
            .expect("module-info.class");
        assert_eq!(contents, b"hello-jmod");
    }

    fn patch_zip_entry_uncompressed_size(zip_bytes: &mut [u8], name: &str, new_size: u32) {
        const CENTRAL_SIG: [u8; 4] = [0x50, 0x4b, 0x01, 0x02]; // PK\x01\x02
        const LOCAL_SIG: [u8; 4] = [0x50, 0x4b, 0x03, 0x04]; // PK\x03\x04

        // Locate the central directory entry for `name` and patch its uncompressed
        // size. Use its "relative offset of local header" to patch the local
        // header as well.
        for i in 0..zip_bytes.len().saturating_sub(4) {
            if zip_bytes[i..i + 4] != CENTRAL_SIG {
                continue;
            }

            // Central directory header fixed-size portion is 46 bytes.
            if i + 46 > zip_bytes.len() {
                continue;
            }

            let file_name_len = u16::from_le_bytes([zip_bytes[i + 28], zip_bytes[i + 29]]) as usize;
            let extra_len = u16::from_le_bytes([zip_bytes[i + 30], zip_bytes[i + 31]]) as usize;
            let comment_len = u16::from_le_bytes([zip_bytes[i + 32], zip_bytes[i + 33]]) as usize;

            let name_start = i + 46;
            let name_end = name_start.saturating_add(file_name_len);
            let header_end = name_end
                .saturating_add(extra_len)
                .saturating_add(comment_len);
            if header_end > zip_bytes.len() {
                continue;
            }

            if &zip_bytes[name_start..name_end] != name.as_bytes() {
                continue;
            }

            // Patch central directory uncompressed size (offset 24 from header start).
            zip_bytes[i + 24..i + 28].copy_from_slice(&new_size.to_le_bytes());

            // Read relative offset of local header (offset 42).
            let local_offset = u32::from_le_bytes([
                zip_bytes[i + 42],
                zip_bytes[i + 43],
                zip_bytes[i + 44],
                zip_bytes[i + 45],
            ]) as usize;

            // Patch local header uncompressed size (offset 22 from local header start).
            if local_offset + 30 <= zip_bytes.len()
                && zip_bytes[local_offset..local_offset + 4] == LOCAL_SIG
            {
                zip_bytes[local_offset + 22..local_offset + 26]
                    .copy_from_slice(&new_size.to_le_bytes());
            }

            // Only patch the first matching entry.
            break;
        }
    }

    #[test]
    fn rejects_oversized_zip_entries_via_declared_size() {
        use zip::write::FileOptions;

        let tmp = TempDir::new().unwrap();
        let jar_path = tmp.path().join("test.jar");
        let name = "META-INF/spring-configuration-metadata.json";

        // Create a tiny valid jar.
        {
            let file = std::fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options =
                FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);

            zip.start_file(name, options).unwrap();
            zip.write_all(b"{}").unwrap();
            zip.finish().unwrap();
        }

        // Patch the declared uncompressed size to MAX+1 without writing an actual
        // MAX+1 byte payload.
        let mut bytes = std::fs::read(&jar_path).unwrap();
        patch_zip_entry_uncompressed_size(&mut bytes, name, (MAX_ARCHIVE_ENTRY_BYTES as u32) + 1);
        std::fs::write(&jar_path, bytes).unwrap();

        let archive = Archive::new(&jar_path);
        let err = archive.read(name).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("declared uncompressed size"));
        assert!(msg.contains("max allowed"));
    }
}
