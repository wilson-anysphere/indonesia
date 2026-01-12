use std::path::PathBuf;
use std::{fs, io::Read};

use nova_classpath::{ClasspathEntry, ModuleAwareClasspathIndex, ModuleNameKind};
use nova_deps_cache::DependencyIndexStore;
use tempfile::TempDir;
use zip::ZipArchive;

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

fn jar_bytes(path: &PathBuf, entry: &str) -> Vec<u8> {
    let file = fs::File::open(path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    let mut file = archive.by_name(entry).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
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

#[test]
fn classpath_jars_are_treated_as_unnamed_modules() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
    let index = ModuleAwareClasspathIndex::build_classpath_with_deps_store(
        &[ClasspathEntry::Jar(test_dep_jar())],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    assert!(index.module_of("com.example.dep.Foo").is_none());
    assert_eq!(
        index.module_kind_of("com.example.dep.Foo"),
        ModuleNameKind::None
    );
}

#[test]
fn module_path_class_directories_with_module_info_are_named_modules() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

    let jar = test_named_module_jar();
    let module_info = jar_bytes(&jar, "module-info.class");
    let api_class = jar_bytes(&jar, "com/example/api/Api.class");

    let dir = tmp.path().join("exploded-module");
    fs::create_dir_all(dir.join("com/example/api")).unwrap();
    fs::write(dir.join("module-info.class"), module_info).unwrap();
    fs::write(dir.join("com/example/api/Api.class"), api_class).unwrap();

    let index = ModuleAwareClasspathIndex::build_module_path_with_deps_store(
        &[ClasspathEntry::ClassDir(dir)],
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
fn module_path_class_directories_with_multi_release_module_info_are_named_modules() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

    let jar = test_named_module_jar();
    let module_info = jar_bytes(&jar, "module-info.class");
    let api_class = jar_bytes(&jar, "com/example/api/Api.class");

    let dir = tmp.path().join("mr-exploded-module");
    fs::create_dir_all(dir.join("META-INF/versions/9")).unwrap();
    fs::create_dir_all(dir.join("com/example/api")).unwrap();
    fs::write(
        dir.join("META-INF/versions/9/module-info.class"),
        module_info,
    )
    .unwrap();
    fs::write(dir.join("com/example/api/Api.class"), api_class).unwrap();

    let index = ModuleAwareClasspathIndex::build_module_path_with_deps_store(
        &[ClasspathEntry::ClassDir(dir)],
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
fn module_path_class_directories_without_module_info_become_automatic_modules() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
    let index = ModuleAwareClasspathIndex::build_module_path_with_deps_store(
        &[ClasspathEntry::ClassDir(test_class_dir())],
        None,
        Some(&deps_store),
        None,
    )
    .unwrap();

    let module = index.module_of("com.example.dep.Bar").unwrap();
    assert_eq!(module.as_str(), "classdir");
    assert_eq!(
        index.module_kind_of("com.example.dep.Bar"),
        ModuleNameKind::Automatic
    );
}

#[test]
fn mixed_index_assigns_modules_based_on_entry_kind() {
    let tmp = TempDir::new().unwrap();
    let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

    let index = ModuleAwareClasspathIndex::build_mixed_with_deps_store(
        &[ClasspathEntry::Jar(test_named_module_jar())],
        &[ClasspathEntry::Jar(test_dep_jar())],
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

    assert!(index.module_of("com.example.dep.Foo").is_none());
    assert_eq!(
        index.module_kind_of("com.example.dep.Foo"),
        ModuleNameKind::None
    );
}
