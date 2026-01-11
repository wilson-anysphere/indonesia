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
}

impl ArchiveReader for NovaArchiveReader {
    fn read_to_string(&self, path: &ArchivePath) -> io::Result<String> {
        if !path.archive.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("archive not found ({path})"),
            ));
        }

        let entry = Self::normalize_entry(&path.entry);
        let archive = Archive::new(path.archive.clone());
        let bytes = archive
            .read(entry.as_ref())
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
        match bytes {
            Some(bytes) => String::from_utf8(bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("archive entry not found ({path})"),
            )),
        }
    }

    fn exists(&self, path: &ArchivePath) -> bool {
        if !path.archive.exists() {
            return false;
        }

        let entry = Self::normalize_entry(&path.entry);
        if path.archive.is_dir() {
            return path.archive.join(entry.as_ref()).exists();
        }

        let archive = Archive::new(path.archive.clone());
        archive.read(entry.as_ref()).ok().flatten().is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::FileOptions;

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
}
