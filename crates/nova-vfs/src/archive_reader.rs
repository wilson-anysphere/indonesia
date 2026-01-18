use std::borrow::Cow;
use std::io;

use nova_archive::Archive;

use crate::archive::{ArchivePath, ArchiveReader};

/// `ArchiveReader` backed by `nova-archive`.
///
/// Supports reading entries from:
/// - `.jar`/zip files
/// - exploded directory archives
#[derive(Debug, Default)]
pub struct NovaArchiveReader;

impl NovaArchiveReader {
    fn normalize_entry(entry: &str) -> Cow<'_, str> {
        let entry = entry.trim_start_matches(['/', '\\']);
        if entry.contains('\\') {
            Cow::Owned(entry.replace('\\', "/"))
        } else {
            Cow::Borrowed(entry)
        }
    }

    fn is_valid_entry(entry: &str) -> bool {
        let bytes = entry.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return false;
        }
        if entry.contains("//") {
            return false;
        }
        !entry.split('/').any(|segment| segment == "..")
    }
}

impl ArchiveReader for NovaArchiveReader {
    fn read_bytes(&self, path: &ArchivePath) -> io::Result<Vec<u8>> {
        if !path.archive.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("archive not found ({path})"),
            ));
        }

        let entry = Self::normalize_entry(&path.entry);
        if !Self::is_valid_entry(entry.as_ref()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid archive entry ({path})"),
            ));
        }
        let archive = Archive::new(path.archive.clone());
        let bytes = archive.read(entry.as_ref()).map_err(io::Error::other)?;
        match bytes {
            Some(bytes) => Ok(bytes),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("archive entry not found ({path})"),
            )),
        }
    }

    fn read_to_string(&self, path: &ArchivePath) -> io::Result<String> {
        let bytes = self.read_bytes(path)?;
        String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    fn exists(&self, path: &ArchivePath) -> bool {
        if !path.archive.exists() {
            return false;
        }

        let entry = Self::normalize_entry(&path.entry);
        if !Self::is_valid_entry(entry.as_ref()) {
            return false;
        }
        if path.archive.is_dir() {
            return path.archive.join(entry.as_ref()).exists();
        }

        let archive = Archive::new(path.archive.clone());
        match archive.read(entry.as_ref()) {
            Ok(Some(_bytes)) => true,
            Ok(None) => false,
            Err(err) => {
                tracing::debug!(
                    target = "nova.vfs",
                    archive_path = %path.archive.display(),
                    entry = %entry,
                    error = %err,
                    "failed to read archive entry while checking existence"
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::FileOptions;

    use crate::{ArchiveKind, ArchivePath};
    use crate::{FileSystem, LocalFs, VfsPath};

    fn write_zip_file(path: &std::path::Path, name: &str, contents: &str) {
        let mut jar = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = FileOptions::<()>::default();
        jar.start_file(name, options).unwrap();
        jar.write_all(contents.as_bytes()).unwrap();
        jar.finish().unwrap();
    }

    #[test]
    fn reads_from_jar_archive() {
        let dir = tempfile::tempdir().unwrap();
        let jar_path = dir.path().join("test.jar");
        write_zip_file(&jar_path, "com/example/A.java", "class A {}");

        let fs = LocalFs::new();
        let entry_path = VfsPath::jar(jar_path.clone(), "com/example/A.java");
        assert_eq!(fs.read_to_string(&entry_path).unwrap(), "class A {}");
        assert!(fs.exists(&entry_path));

        let missing_path = VfsPath::jar(jar_path.clone(), "com/example/Missing.java");
        assert!(!fs.exists(&missing_path));
    }

    #[test]
    fn reads_from_exploded_directory_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("exploded.jar");
        let entry_parent = archive_dir.join("com/example");
        std::fs::create_dir_all(&entry_parent).unwrap();
        std::fs::write(entry_parent.join("A.java"), "class A {}").unwrap();

        let fs = LocalFs::new();
        let entry_path = VfsPath::jar(archive_dir.clone(), "com/example/A.java");
        assert_eq!(fs.read_to_string(&entry_path).unwrap(), "class A {}");
        assert!(fs.exists(&entry_path));

        let missing_path = VfsPath::jar(archive_dir.clone(), "com/example/Missing.java");
        assert!(!fs.exists(&missing_path));
    }

    #[test]
    fn exploded_directory_archives_reject_entry_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("exploded.jar");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "do not read me").unwrap();

        let fs = LocalFs::new();
        let traversal = VfsPath::Archive(ArchivePath::new(
            ArchiveKind::Jar,
            archive_dir,
            "../secret.txt".to_string(),
        ));

        let err = fs.read_to_string(&traversal).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!fs.exists(&traversal));
    }
}
