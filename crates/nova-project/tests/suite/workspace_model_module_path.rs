use nova_project::{
    load_workspace_model_with_options, BuildSystem, ClasspathEntryKind, LoadOptions,
};
use std::io::Write;
use std::path::Path;
use tempfile::tempdir;
use zip::write::FileOptions;

#[test]
fn workspace_model_populates_module_path_for_jpms_workspaces() {
    let tmp = tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");

    std::fs::create_dir_all(root.join("src/main/java")).expect("mkdir src");
    std::fs::write(
        root.join("src/main/java/module-info.java"),
        "module mod.a { requires mod.b; }",
    )
    .expect("write module-info.java");

    let dep_dir = root.join("deps/mod-b");
    std::fs::create_dir_all(&dep_dir).expect("mkdir dep_dir");
    std::fs::write(dep_dir.join("module-info.class"), make_module_info_class())
        .expect("write module-info.class");

    let mut options = LoadOptions::default();
    options.classpath_overrides.push(dep_dir.clone());
    let model = load_workspace_model_with_options(&root, &options).expect("load workspace model");

    assert_eq!(model.build_system, BuildSystem::Simple);
    assert_eq!(model.modules.len(), 1);
    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|e| e.path == dep_dir && e.kind == ClasspathEntryKind::Directory),
        "dependency override directory should be placed on module-path when JPMS is enabled (dep_dir={})",
        dep_dir.display()
    );

    assert!(
        !module.classpath.iter().any(|e| e.path == dep_dir),
        "dependency override directory should not remain on the classpath when JPMS is enabled (dep_dir={})",
        dep_dir.display()
    );
}

#[test]
fn simple_workspace_model_puts_missing_jar_overrides_on_module_path_for_jpms_workspaces() {
    let tmp = tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");

    std::fs::create_dir_all(root.join("src/main/java")).expect("mkdir src");
    std::fs::write(
        root.join("src/main/java/module-info.java"),
        "module mod.a { requires mod.b; }",
    )
    .expect("write module-info.java");

    // Do not create the jar on disk: loaders often synthesize dependency jar paths without
    // downloading them, and JPMS needs jar deps on the module-path (automatic module name).
    // Use an uppercase extension to ensure classpath override kind detection is case-insensitive.
    let jar_path = root.join("deps/mod-b.JAR");

    let mut options = LoadOptions::default();
    options.classpath_overrides.push(jar_path.clone());
    let model = load_workspace_model_with_options(&root, &options).expect("load workspace model");

    assert_eq!(model.build_system, BuildSystem::Simple);
    assert_eq!(model.modules.len(), 1);
    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|e| e.path == jar_path && e.kind == ClasspathEntryKind::Jar),
        "missing jar override should be placed on module-path when JPMS is enabled (jar_path={})",
        jar_path.display()
    );
    assert!(
        !module.classpath.iter().any(|e| e.path == jar_path),
        "missing jar override should not remain on classpath when JPMS is enabled (jar_path={})",
        jar_path.display()
    );
}

#[test]
fn simple_workspace_model_puts_missing_jmod_overrides_on_module_path_for_jpms_workspaces() {
    let tmp = tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");

    std::fs::create_dir_all(root.join("src/main/java")).expect("mkdir src");
    std::fs::write(
        root.join("src/main/java/module-info.java"),
        "module mod.a { requires mod.b; }",
    )
    .expect("write module-info.java");

    // `.jmod` entries should also be treated as archive dependencies for JPMS-enabled workspaces.
    // Use an uppercase extension to ensure kind detection is case-insensitive.
    let jmod_path = root.join("deps/mod-b.JMOD");

    let mut options = LoadOptions::default();
    options.classpath_overrides.push(jmod_path.clone());
    let model = load_workspace_model_with_options(&root, &options).expect("load workspace model");

    assert_eq!(model.build_system, BuildSystem::Simple);
    assert_eq!(model.modules.len(), 1);
    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|e| e.path == jmod_path && e.kind == ClasspathEntryKind::Jar),
        "missing jmod override should be placed on module-path when JPMS is enabled (jmod_path={})",
        jmod_path.display()
    );
    assert!(
        !module.classpath.iter().any(|e| e.path == jmod_path),
        "missing jmod override should not remain on classpath when JPMS is enabled (jmod_path={})",
        jmod_path.display()
    );
}

#[test]
fn maven_workspace_model_places_automatic_module_name_jars_on_module_path() {
    let tmp = tempdir().expect("tempdir");
    let tmp_root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");
    let root = tmp_root.join("workspace");
    std::fs::create_dir_all(&root).expect("mkdir root");

    std::fs::write(
        root.join("pom.xml"),
        r#"
            <project xmlns="http://maven.apache.org/POM/4.0.0">
              <modelVersion>4.0.0</modelVersion>
              <groupId>com.example</groupId>
              <artifactId>app</artifactId>
              <version>1.0</version>

              <dependencies>
                <dependency>
                  <groupId>com.example</groupId>
                  <artifactId>dep</artifactId>
                  <version>1.0</version>
                </dependency>
              </dependencies>
            </project>
        "#,
    )
    .expect("write pom.xml");

    let src_dir = root.join("src/main/java");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    std::fs::write(
        src_dir.join("module-info.java"),
        "module com.example.app { requires com.example.dep; }",
    )
    .expect("write module-info.java");
    std::fs::write(
        src_dir.join("Main.java"),
        "package com.example.app; class Main {}",
    )
    .expect("write dummy java");

    let maven_repo = tmp_root.join("maven-repo");
    let jar_path = maven_repo.join("com/example/dep/1.0/dep-1.0.jar");
    std::fs::create_dir_all(jar_path.parent().expect("jar parent")).expect("mkdir jar parent");
    write_jar_with_manifest(
        &jar_path,
        "Manifest-Version: 1.0\r\nAutomatic-Module-Name: com.example.dep\r\n\r\n",
    );

    let options = LoadOptions {
        maven_repo: Some(maven_repo),
        ..LoadOptions::default()
    };

    let model = load_workspace_model_with_options(&root, &options).expect("load workspace model");

    assert_eq!(model.build_system, BuildSystem::Maven);
    assert_eq!(model.modules.len(), 1);
    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|e| e.path == jar_path && e.kind == ClasspathEntryKind::Jar),
        "Maven dependency jar with Automatic-Module-Name should be on module-path when JPMS is enabled (jar_path={})",
        jar_path.display()
    );
    assert!(
        !module.classpath.iter().any(|e| e.path == jar_path),
        "Maven dependency jar should not remain on classpath when JPMS is enabled (jar_path={})",
        jar_path.display()
    );
}

#[test]
fn gradle_workspace_model_populates_module_path_for_jpms_overrides() {
    let tmp = tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");

    // Minimal marker so `detect_build_system` chooses Gradle.
    std::fs::write(root.join("build.gradle"), "").expect("write build.gradle");

    let src_dir = root.join("src/main/java");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    std::fs::write(
        src_dir.join("module-info.java"),
        "module mod.a { requires mod.b; }",
    )
    .expect("write module-info.java");
    std::fs::write(src_dir.join("Main.java"), "class Main {}").expect("write dummy java");

    let dep_dir = root.join("deps/mod-b");
    std::fs::create_dir_all(&dep_dir).expect("mkdir dep_dir");
    std::fs::write(dep_dir.join("module-info.class"), make_module_info_class())
        .expect("write module-info.class");

    let mut options = LoadOptions::default();
    options.classpath_overrides.push(dep_dir.clone());
    let model = load_workspace_model_with_options(&root, &options).expect("load workspace model");

    assert_eq!(model.build_system, BuildSystem::Gradle);
    assert_eq!(model.modules.len(), 1);
    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|e| e.path == dep_dir && e.kind == ClasspathEntryKind::Directory),
        "override directory should be placed on module-path when JPMS is enabled (dep_dir={})",
        dep_dir.display()
    );
    assert!(
        !module.classpath.iter().any(|e| e.path == dep_dir),
        "override directory should not remain on the classpath when JPMS is enabled (dep_dir={})",
        dep_dir.display()
    );

    let main_out = root.join("build/classes/java/main");
    let test_out = root.join("build/classes/java/test");
    assert!(
        module
            .classpath
            .iter()
            .any(|e| e.path == main_out && e.kind == ClasspathEntryKind::Directory),
        "Gradle output dirs should remain on classpath (main_out={})",
        main_out.display()
    );
    assert!(
        module
            .classpath
            .iter()
            .any(|e| e.path == test_out && e.kind == ClasspathEntryKind::Directory),
        "Gradle output dirs should remain on classpath (test_out={})",
        test_out.display()
    );
}

#[test]
fn bazel_workspace_model_populates_module_path_for_jpms_overrides() {
    let tmp = tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");

    std::fs::write(root.join("WORKSPACE"), "").expect("write WORKSPACE");

    std::fs::write(
        root.join("module-info.java"),
        "module mod.a { requires mod.b; }",
    )
    .expect("write module-info.java");

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    std::fs::write(src_dir.join("Main.java"), "class Main {}").expect("write dummy java");

    let dep_dir = root.join("deps/mod-b");
    std::fs::create_dir_all(&dep_dir).expect("mkdir dep_dir");
    std::fs::write(dep_dir.join("module-info.class"), make_module_info_class())
        .expect("write module-info.class");

    let mut options = LoadOptions::default();
    options.classpath_overrides.push(dep_dir.clone());
    let model = load_workspace_model_with_options(&root, &options).expect("load workspace model");

    assert_eq!(model.build_system, BuildSystem::Bazel);
    assert_eq!(model.modules.len(), 1);
    let module = &model.modules[0];

    assert!(
        module
            .module_path
            .iter()
            .any(|e| e.path == dep_dir && e.kind == ClasspathEntryKind::Directory),
        "override directory should be placed on module-path when JPMS is enabled (dep_dir={})",
        dep_dir.display()
    );
    assert!(
        !module.classpath.iter().any(|e| e.path == dep_dir),
        "override directory should not remain on the classpath when JPMS is enabled (dep_dir={})",
        dep_dir.display()
    );
}

fn make_module_info_class() -> Vec<u8> {
    fn push_u2(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn push_u4(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    #[derive(Clone)]
    enum CpEntry {
        Utf8(String),
        Module { name_index: u16 },
        Package { name_index: u16 },
    }

    struct Cp {
        entries: Vec<CpEntry>,
    }

    impl Cp {
        fn new() -> Self {
            Self {
                entries: Vec::new(),
            }
        }

        fn push(&mut self, entry: CpEntry) -> u16 {
            self.entries.push(entry);
            self.entries.len() as u16
        }

        fn utf8(&mut self, s: &str) -> u16 {
            self.push(CpEntry::Utf8(s.to_string()))
        }

        fn module(&mut self, name: &str) -> u16 {
            let name_index = self.utf8(name);
            self.push(CpEntry::Module { name_index })
        }

        fn package(&mut self, name: &str) -> u16 {
            let name_index = self.utf8(name);
            self.push(CpEntry::Package { name_index })
        }

        fn write(&self, out: &mut Vec<u8>) {
            push_u2(out, (self.entries.len() as u16) + 1);
            for entry in &self.entries {
                match entry {
                    CpEntry::Utf8(s) => {
                        out.push(1);
                        push_u2(out, s.len() as u16);
                        out.extend_from_slice(s.as_bytes());
                    }
                    CpEntry::Module { name_index } => {
                        out.push(19);
                        push_u2(out, *name_index);
                    }
                    CpEntry::Package { name_index } => {
                        out.push(20);
                        push_u2(out, *name_index);
                    }
                }
            }
        }
    }

    let mut cp = Cp::new();
    let module_attr_name = cp.utf8("Module");
    let self_module = cp.module("mod.b");
    let api_pkg = cp.package("com/example/b/api");
    let target_mod = cp.module("mod.a");

    let mut module_attr = Vec::new();
    push_u2(&mut module_attr, self_module); // module_name_index
    push_u2(&mut module_attr, 0); // module_flags
    push_u2(&mut module_attr, 0); // module_version_index
    push_u2(&mut module_attr, 0); // requires_count
    push_u2(&mut module_attr, 1); // exports_count
                                  // exports
    push_u2(&mut module_attr, api_pkg); // exports_index (Package)
    push_u2(&mut module_attr, 0); // exports_flags
    push_u2(&mut module_attr, 1); // exports_to_count
    push_u2(&mut module_attr, target_mod); // exports_to_index (Module)
    push_u2(&mut module_attr, 0); // opens_count
    push_u2(&mut module_attr, 0); // uses_count
    push_u2(&mut module_attr, 0); // provides_count

    let mut out = Vec::new();
    push_u4(&mut out, 0xCAFEBABE);
    push_u2(&mut out, 0); // minor
    push_u2(&mut out, 53); // major (Java 9)
    cp.write(&mut out);

    push_u2(&mut out, 0); // access_flags
    push_u2(&mut out, 0); // this_class
    push_u2(&mut out, 0); // super_class
    push_u2(&mut out, 0); // interfaces_count
    push_u2(&mut out, 0); // fields_count
    push_u2(&mut out, 0); // methods_count

    push_u2(&mut out, 1); // attributes_count
    push_u2(&mut out, module_attr_name);
    push_u4(&mut out, module_attr.len() as u32);
    out.extend_from_slice(&module_attr);

    // Sanity check: ensure the fixture parses (helps catch accidental corruption).
    let info = nova_classfile::parse_module_info_class(&out).expect("parse module-info.class");
    assert_eq!(info.name.as_str(), "mod.b");

    out
}

fn write_jar_with_manifest(path: &Path, manifest: &str) {
    let mut jar = zip::ZipWriter::new(std::fs::File::create(path).expect("create jar"));
    let options = FileOptions::<()>::default();
    jar.start_file("META-INF/MANIFEST.MF", options)
        .expect("start manifest entry");
    jar.write_all(manifest.as_bytes())
        .expect("write manifest contents");
    jar.finish().expect("finish jar");
}

#[test]
fn make_module_info_class_fixture_is_readable_from_directory_archive() {
    let tmp = tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");
    let dep_dir = root.join("mod-b");
    std::fs::create_dir_all(&dep_dir).expect("mkdir dep_dir");
    std::fs::write(dep_dir.join("module-info.class"), make_module_info_class())
        .expect("write module-info.class");

    let archive = nova_archive::Archive::new(dep_dir.clone());
    let bytes = archive
        .read("module-info.class")
        .expect("read module-info.class")
        .expect("missing module-info.class");
    let info = nova_classfile::parse_module_info_class(&bytes).expect("parse module-info.class");
    assert_eq!(info.name.as_str(), "mod.b");
}
