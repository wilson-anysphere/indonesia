use nova_cache::{
    install_cache_package, pack_cache_package, CacheConfig, CacheDir, CacheMetadata,
    CachePackageInstallOutcome, ProjectSnapshot,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn write_fake_cache(cache_dir: &CacheDir) {
    std::fs::write(cache_dir.indexes_dir().join("symbols.idx"), b"symbols").unwrap();
    std::fs::write(cache_dir.queries_dir().join("types.cache"), b"types").unwrap();
    std::fs::write(cache_dir.ast_dir().join("metadata.bin"), b"ast-metadata").unwrap();
}

#[test]
fn install_uses_indexes_only_when_file_metadata_mismatches() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(project_root.join("src")).unwrap();

    let mut files = Vec::new();
    for i in 0..256u32 {
        let rel = PathBuf::from(format!("src/File{i}.java"));
        let full = project_root.join(&rel);
        std::fs::write(&full, format!("class File{i} {{}}\n")).unwrap();
        files.push(rel);
    }

    let cache_root = tmp.path().join("cache-root");
    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(cache_root.clone()),
        },
    )
    .unwrap();

    let snapshot = ProjectSnapshot::new(&project_root, files.clone()).unwrap();
    let metadata = CacheMetadata::new(&snapshot);
    metadata.save(cache_dir.metadata_path()).unwrap();
    write_fake_cache(&cache_dir);

    let package_path = tmp.path().join("cache.tar.zst");
    pack_cache_package(&cache_dir, &package_path).unwrap();

    // Simulate a fresh cache install into an empty cache directory.
    std::fs::remove_dir_all(cache_dir.root()).unwrap();
    let cache_dir2 = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(cache_root),
        },
    )
    .unwrap();

    // Change a file's size to force a metadata fingerprint mismatch.
    let changed = project_root.join(&files[128]);
    std::fs::write(&changed, b"class Changed { int x; }\n").unwrap();

    let outcome = install_cache_package(&cache_dir2, &package_path).unwrap();
    let CachePackageInstallOutcome::IndexesOnly { mismatched_files } = outcome else {
        panic!("expected indexes-only install, got {outcome:?}");
    };
    assert_eq!(mismatched_files, 1);

    assert!(cache_dir2.metadata_path().is_file());
    assert!(cache_dir2.indexes_dir().join("symbols.idx").is_file());
    assert!(!cache_dir2.queries_dir().join("types.cache").is_file());
    assert!(!cache_dir2.ast_dir().join("metadata.bin").is_file());
}

#[cfg(unix)]
fn mkfifo(path: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn mkfifo(pathname: *const c_char, mode: u32) -> i32;
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).expect("path must not contain NUL");
    let rc = unsafe { mkfifo(c_path.as_ptr(), 0o644) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Regression test: installing a cache package with mismatched fingerprints
/// should not read file contents. A FIFO would block any content hashing.
#[cfg(unix)]
#[test]
fn install_mismatch_does_not_read_file_contents() {
    use std::io::Write;
    use std::sync::mpsc;

    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(project_root.join("src")).unwrap();

    let blocking_rel = PathBuf::from("src/blocking.txt");
    let main_rel = PathBuf::from("src/Main.java");
    std::fs::write(project_root.join(&main_rel), b"class Main {}\n").unwrap();
    std::fs::write(project_root.join(&blocking_rel), b"initial\n").unwrap();

    let cache_root = tmp.path().join("cache-root");
    let cache_dir = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(cache_root.clone()),
        },
    )
    .unwrap();

    let snapshot =
        ProjectSnapshot::new(&project_root, vec![main_rel.clone(), blocking_rel.clone()]).unwrap();
    let metadata = CacheMetadata::new(&snapshot);
    metadata.save(cache_dir.metadata_path()).unwrap();
    write_fake_cache(&cache_dir);

    let package_path = tmp.path().join("cache.tar.zst");
    pack_cache_package(&cache_dir, &package_path).unwrap();

    std::fs::remove_dir_all(cache_dir.root()).unwrap();
    let cache_dir2 = CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(cache_root),
        },
    )
    .unwrap();

    // Replace the file with a FIFO. Any attempt to hash contents will block on open/read.
    let blocking_path = project_root.join(&blocking_rel);
    std::fs::remove_file(&blocking_path).unwrap();
    mkfifo(&blocking_path).unwrap();

    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn({
        let cache_dir2 = cache_dir2.clone();
        let package_path = package_path.clone();
        move || {
            let res = install_cache_package(&cache_dir2, &package_path);
            tx.send(res).unwrap();
        }
    });

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(res) => {
            handle.join().unwrap();
            let outcome = res.unwrap();
            assert!(matches!(
                outcome,
                CachePackageInstallOutcome::IndexesOnly {
                    mismatched_files: _
                }
            ));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Unblock any accidental FIFO reads and fail the test.
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&blocking_path)
                .and_then(|mut f| f.write_all(b"unblock"));
            let res = rx.recv_timeout(Duration::from_secs(5));
            if res.is_ok() {
                handle.join().unwrap();
            }
            panic!("install_cache_package appears to have read file contents; result after unblocking: {res:?}");
        }
        Err(err) => {
            handle.join().unwrap();
            panic!("unexpected recv error: {err:?}");
        }
    }
}
