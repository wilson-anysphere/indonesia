use std::path::PathBuf;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_deps_cache::DependencyIndexStore;
use tempfile::TempDir;

fn test_dep_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/dep.jar")
}

#[test]
fn iter_binary_names_is_sorted_and_contains_known_fixtures() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
    let index = ClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(test_dep_jar())],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    let mut prev: Option<&str> = None;
    let mut saw_bar = false;
    let mut saw_foo = false;

    let mut count = 0usize;
    for name in index.iter_binary_names() {
        if let Some(prev) = prev {
            assert!(
                prev <= name,
                "expected iter_binary_names() to be sorted, but saw `{prev}` before `{name}`"
            );
        }
        prev = Some(name);

        saw_bar |= name == "com.example.dep.Bar";
        saw_foo |= name == "com.example.dep.Foo";
        count += 1;
    }

    assert_eq!(count, index.len());
    assert!(saw_bar, "expected dep.jar to contain com.example.dep.Bar");
    assert!(saw_foo, "expected dep.jar to contain com.example.dep.Foo");
}

