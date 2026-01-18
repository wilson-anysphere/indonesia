use std::sync::{Mutex, OnceLock};

use nova_workspace::Workspace;

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn with_cache_dir<T>(cache_dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());

    let prior = std::env::var_os("NOVA_CACHE_DIR");
    std::env::set_var("NOVA_CACHE_DIR", cache_dir);
    let out = f();
    match prior {
        Some(value) => std::env::set_var("NOVA_CACHE_DIR", value),
        None => std::env::remove_var("NOVA_CACHE_DIR"),
    }
    out
}

#[test]
fn warm_start_indexes_only_changed_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(project_root.join("src")).expect("create src");
    std::fs::write(project_root.join("src/A.java"), "class A {}\n").expect("write A");
    std::fs::write(project_root.join("src/B.java"), "class B {}\n").expect("write B");

    let cache_root = tempfile::tempdir().expect("cache dir");

    with_cache_dir(cache_root.path(), || {
        let ws = Workspace::open(&project_root).expect("open workspace");

        let first = ws.index_and_write_cache().expect("first index");
        assert_eq!(first.metrics.files_total, 2);
        assert_eq!(first.metrics.files_indexed, 2);
        assert_eq!(first.metrics.files_invalidated, 2);
        assert!(first.metrics.symbols_indexed > 0);

        let second = ws.index_and_write_cache().expect("second index");
        assert_eq!(second.metrics.files_total, 2);
        assert_eq!(second.metrics.files_indexed, 0);
        assert_eq!(second.metrics.files_invalidated, 0);
        assert_eq!(
            second.metrics.symbols_indexed,
            first.metrics.symbols_indexed
        );

        // Touch one file (len+mtime changes) and ensure only that file is re-indexed.
        std::fs::write(project_root.join("src/A.java"), "class A { void m() {} }\n")
            .expect("touch A");

        let third = ws.index_and_write_cache().expect("third index");
        assert_eq!(third.metrics.files_total, 2);
        assert_eq!(third.metrics.files_indexed, 1);
        assert_eq!(third.metrics.files_invalidated, 1);
    });
}
