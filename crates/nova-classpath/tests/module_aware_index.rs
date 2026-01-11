use std::path::PathBuf;

use nova_classpath::{ClasspathEntry, ModuleAwareClasspathIndex, ModuleNameKind};
use nova_deps_cache::DependencyIndexStore;
use tempfile::TempDir;

fn test_dep_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/dep.jar")
}

fn test_class_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/classdir")
}

fn test_named_module_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/named-module.jar")
}

fn test_named_module_jmod() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/named-module.jmod")
}

#[test]
fn types_from_named_module_jar_are_assigned_to_explicit_module() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
    let index = ModuleAwareClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(test_named_module_jar())],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    let module = index.module_of("com.example.api.Api").unwrap();
    assert_eq!(module.as_str(), "example.mod");
    assert_eq!(
        index.module_kind_of("com.example.api.Api"),
        ModuleNameKind::Explicit
    );
}

#[test]
fn types_from_named_module_jmod_are_assigned_to_explicit_module() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
    let index = ModuleAwareClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jmod(test_named_module_jmod())],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    let module = index.module_of("com.example.api.Api").unwrap();
    assert_eq!(module.as_str(), "example.mod");
    assert_eq!(
        index.module_kind_of("com.example.api.Api"),
        ModuleNameKind::Explicit
    );
}

#[test]
fn types_from_regular_jar_are_assigned_to_automatic_module() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
    let index = ModuleAwareClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::Jar(test_dep_jar())],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    let module = index.module_of("com.example.dep.Foo").unwrap();
    assert_eq!(module.as_str(), "dep");
    assert_eq!(
        index.module_kind_of("com.example.dep.Foo"),
        ModuleNameKind::Automatic
    );
}

#[test]
fn class_directories_are_treated_as_unnamed_modules() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
    let index = ModuleAwareClasspathIndex::build_with_deps_store(
        &[ClasspathEntry::ClassDir(test_class_dir())],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    assert!(index.module_of("com.example.dep.Bar").is_none());
    assert_eq!(
        index.module_kind_of("com.example.dep.Bar"),
        ModuleNameKind::None
    );
}
