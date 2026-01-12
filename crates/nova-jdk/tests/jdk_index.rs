use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use nova_classfile::ClassFile;
use nova_core::{JdkConfig, Name, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_jdk::{
    internal_name_to_source_entry_path, IndexingStats, JdkIndex, JdkIndexBacking, JdkIndexError,
    JdkInstallation,
};
use nova_modules::ModuleName;
use nova_test_utils::{env_lock, EnvVarGuard};
use nova_types::TypeProvider;
use tempfile::tempdir;

#[test]
fn builtin_index_resolves_minimal_fixture_types() {
    let index = JdkIndex::new();

    assert_eq!(
        index.resolve_type(&QualifiedName::from_dotted("java.util.function.Function")),
        Some(TypeName::new("java.util.function.Function"))
    );
    assert_eq!(
        index.resolve_type(&QualifiedName::from_dotted("java.util.Collections")),
        Some(TypeName::new("java.util.Collections"))
    );
    assert_eq!(
        index.resolve_type(&QualifiedName::from_dotted("java.lang.Iterable")),
        Some(TypeName::new("java.lang.Iterable"))
    );
    assert_eq!(
        index.resolve_type(&QualifiedName::from_dotted("java.lang.Runnable")),
        Some(TypeName::new("java.lang.Runnable"))
    );
    assert_eq!(
        index.resolve_type(&QualifiedName::from_dotted("java.lang.Number")),
        Some(TypeName::new("java.lang.Number"))
    );
    assert_eq!(
        index.resolve_type(&QualifiedName::from_dotted("java.util.function.Supplier")),
        Some(TypeName::new("java.util.function.Supplier"))
    );
    assert_eq!(
        index.resolve_type(&QualifiedName::from_dotted("java.util.function.Consumer")),
        Some(TypeName::new("java.util.function.Consumer"))
    );

    assert_eq!(
        index.resolve_static_member(&TypeName::new("java.lang.Math"), &Name::from("max")),
        Some(StaticMemberId::new("java.lang.Math::max"))
    );
    assert_eq!(
        index.resolve_static_member(&TypeName::new("java.lang.Math"), &Name::from("min")),
        Some(StaticMemberId::new("java.lang.Math::min"))
    );
    assert_eq!(
        index.resolve_static_member(&TypeName::new("java.lang.Math"), &Name::from("PI")),
        Some(StaticMemberId::new("java.lang.Math::PI"))
    );
    assert_eq!(
        index.resolve_static_member(&TypeName::new("java.lang.Math"), &Name::from("E")),
        Some(StaticMemberId::new("java.lang.Math::E"))
    );

    assert_eq!(
        index.resolve_static_member(
            &TypeName::new("java.util.Collections"),
            &Name::from("emptyList")
        ),
        Some(StaticMemberId::new("java.util.Collections::emptyList"))
    );
    assert_eq!(
        index.resolve_static_member(
            &TypeName::new("java.util.Collections"),
            &Name::from("singletonList")
        ),
        Some(StaticMemberId::new("java.util.Collections::singletonList"))
    );
}

fn fake_jdk_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/fake-jdk")
}

fn fake_jdk8_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/fake-jdk8")
}

fn find_jdk_symbol_index_cache_file(cache_root: &Path) -> PathBuf {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, out);
                continue;
            }

            if path.file_name().and_then(|name| name.to_str()) == Some("jdk-symbol-index.idx") {
                out.push(path);
            }
        }
    }

    let mut matches = Vec::new();
    visit(cache_root, &mut matches);
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one jdk-symbol-index.idx file under {}",
        cache_root.display()
    );
    matches.pop().expect("checked matches length")
}

fn minimal_class_bytes(internal_name: &str) -> Vec<u8> {
    minimal_class_bytes_with_marker(internal_name, None)
}

fn minimal_class_bytes_with_marker(internal_name: &str, marker: Option<&str>) -> Vec<u8> {
    fn push_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn push_utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1); // CONSTANT_Utf8
        push_u16(out, s.len() as u16);
        out.extend_from_slice(s.as_bytes());
    }
    fn push_class(out: &mut Vec<u8>, name_index: u16) {
        out.push(7); // CONSTANT_Class
        push_u16(out, name_index);
    }

    const MAJOR_JAVA_8: u16 = 52;
    let super_internal = "java/lang/Object";

    // Constant pool:
    // 1: Utf8 this
    // 2: Class #1
    // 3: Utf8 super
    // 4: Class #3
    // 5: (optional) Utf8 marker
    let cp_count: u16 = if marker.is_some() { 6 } else { 5 };

    let mut bytes = Vec::new();
    push_u32(&mut bytes, 0xCAFEBABE);
    push_u16(&mut bytes, 0); // minor
    push_u16(&mut bytes, MAJOR_JAVA_8);
    push_u16(&mut bytes, cp_count);

    push_utf8(&mut bytes, internal_name);
    push_class(&mut bytes, 1);
    push_utf8(&mut bytes, super_internal);
    push_class(&mut bytes, 3);
    if let Some(marker) = marker {
        push_utf8(&mut bytes, marker);
    }

    // access_flags (public + super)
    push_u16(&mut bytes, 0x0021);
    // this_class
    push_u16(&mut bytes, 2);
    // super_class
    push_u16(&mut bytes, 4);
    // interfaces_count, fields_count, methods_count, attributes_count
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);

    bytes
}

fn write_jar(
    jar_path: &std::path::Path,
    entries: &[(&str, Vec<u8>)],
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let file = std::fs::File::create(jar_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::default();

    for &(name, ref bytes) in entries {
        zip.start_file(name, options)?;
        zip.write_all(bytes)?;
    }

    zip.finish()?;
    Ok(())
}

fn read_zip_entry_bytes(
    zip_path: &Path,
    entry_path: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::io::Read;

    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut entry = archive.by_name(entry_path)?;
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes)?;
    Ok(bytes)
}


#[test]
fn loads_java_lang_string_from_test_jmod() -> Result<(), Box<dyn std::error::Error>> {
    let root = fake_jdk_root();
    let index = JdkIndex::from_jdk_root(&root)?;

    assert_eq!(index.info().backing, JdkIndexBacking::Jmods);
    assert_eq!(
        index.info().root,
        std::fs::canonicalize(&root).unwrap_or(root.clone())
    );
    assert!(index.src_zip().is_none());

    let java_base = ModuleName::new("java.base");
    let graph = index
        .module_graph()
        .expect("JMOD-backed JdkIndex should expose a module graph");
    assert!(
        graph.get(&java_base).is_some(),
        "module graph should include java.base"
    );

    let java_base_info = index
        .module_info(&java_base)
        .expect("java.base module descriptor should be indexed");
    assert_eq!(java_base_info.name.as_str(), "java.base");

    let string = index
        .lookup_type("java.lang.String")?
        .expect("java.lang.String should be present in testdata");

    assert_eq!(string.internal_name, "java/lang/String");
    assert_eq!(string.binary_name, "java.lang.String");
    assert!(
        string
            .methods
            .iter()
            .any(|m| m.name == "<init>" && m.descriptor == "()V"),
        "fixture should have a public no-arg constructor"
    );

    assert!(index.lookup_type("java/lang/String")?.is_some());
    assert!(
        index.lookup_type("String")?.is_some(),
        "universe-scope lookup"
    );

    let java_lang = index.java_lang_symbols()?;
    assert!(java_lang
        .iter()
        .any(|t| t.binary_name == "java.lang.String"));

    let string_module = index
        .module_of_type("java.lang.String")
        .expect("module lookup should succeed for java.lang.String");
    assert_eq!(string_module.as_str(), "java.base");

    let pkgs = index.packages()?;
    assert!(pkgs.contains(&"java.lang".to_owned()));
    assert!(index
        .packages_with_prefix("java.l")?
        .contains(&"java.lang".to_string()));
    assert!(index
        .packages_with_prefix("java/l")?
        .contains(&"java.lang".to_string()));

    assert!(index
        .class_names_with_prefix("java.lang.S")?
        .contains(&"java.lang.String".to_string()));
    assert!(index
        .class_names_with_prefix("java/lang/S")?
        .contains(&"java.lang.String".to_string()));

    let string_def = TypeProvider::lookup_type(&index, "java.lang.String")
        .expect("TypeProvider should expose java.lang.String when symbols are loaded");
    assert_eq!(string_def.binary_name, "java.lang.String");
    assert!(
        string_def
            .methods
            .iter()
            .any(|m| m.name == "<init>" && m.descriptor == "()V"),
        "TypeProvider stub should include class members"
    );

    let list = index
        .lookup_type("java.util.List")?
        .expect("java.util.List should be present in testdata");
    assert_eq!(
        list.signature.as_deref(),
        Some("<E:Ljava/lang/Object;>Ljava/lang/Object;")
    );
    let get = list
        .methods
        .iter()
        .find(|m| m.name == "get")
        .expect("fixture should have List.get(int)");
    assert_eq!(get.signature.as_deref(), Some("(I)TE;"));

    let list_def = TypeProvider::lookup_type(&index, "java.util.List")
        .expect("TypeProvider should expose java.util.List when symbols are loaded");
    assert_eq!(
        list_def.signature.as_deref(),
        Some("<E:Ljava/lang/Object;>Ljava/lang/Object;")
    );
    let get_def = list_def
        .methods
        .iter()
        .find(|m| m.name == "get")
        .expect("TypeProvider stub should include List.get(int)");
    assert_eq!(get_def.signature.as_deref(), Some("(I)TE;"));

    Ok(())
}

#[test]
fn builtin_iter_binary_class_names_is_sorted_and_contains_expected() {
    let index = JdkIndex::new();

    let names: Vec<&str> = index
        .iter_binary_class_names()
        .expect("builtin JdkIndex should enumerate names without errors")
        .collect();

    assert!(
        names.windows(2).all(|w| w[0] <= w[1]),
        "expected built-in binary class names to be sorted"
    );
    assert!(
        names.contains(&"java.lang.String"),
        "built-in index should include java.lang.String"
    );

    let slice = index
        .binary_class_names()
        .expect("builtin index should expose binary_class_names slice");
    let slice_names: Vec<&str> = slice.iter().map(|s| s.as_str()).collect();
    assert_eq!(slice_names, names);
}

#[test]
fn symbol_iter_binary_class_names_is_sorted_and_contains_expected(
) -> Result<(), Box<dyn std::error::Error>> {
    let index = JdkIndex::from_jdk_root(fake_jdk_root())?;

    assert!(
        index.binary_class_names().is_none(),
        "symbol-backed JdkIndex should not report builtin binary_class_names slice"
    );

    let names: Vec<&str> = index.iter_binary_class_names()?.collect();
    assert!(
        names.windows(2).all(|w| w[0] <= w[1]),
        "expected symbol-backed binary class names to be sorted"
    );
    assert!(
        names.contains(&"java.lang.String"),
        "expected symbol-backed index to include java.lang.String"
    );

    Ok(())
}

#[test]
fn reads_java_lang_string_class_bytes_from_test_jmod() -> Result<(), Box<dyn std::error::Error>> {
    let index = JdkIndex::from_jdk_root(fake_jdk_root())?;

    let bytes = index
        .read_class_bytes("java/lang/String")?
        .expect("java/lang/String should be present in testdata");
    assert!(
        bytes.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]),
        "class files should start with CAFEBABE"
    );

    let class_file = ClassFile::parse(&bytes)?;
    assert_eq!(class_file.this_class, "java/lang/String");

    assert!(
        index.read_class_bytes("module-info")?.is_none(),
        "module-info.class is not a type"
    );

    Ok(())
}

#[test]
fn loads_java_lang_string_from_test_rt_jar() -> Result<(), Box<dyn std::error::Error>> {
    let index = JdkIndex::from_jdk_root(fake_jdk8_root())?;

    assert!(
        index.module_graph().is_none(),
        "legacy jar-backed JdkIndex should not expose a module graph"
    );

    let string = index
        .lookup_type("java.lang.String")?
        .expect("java.lang.String should be present in testdata");
    assert_eq!(string.internal_name, "java/lang/String");

    let list = index
        .lookup_type("java.util.List")?
        .expect("java.util.List should be present in testdata");
    assert_eq!(list.internal_name, "java/util/List");

    let java_lang = index.java_lang_symbols()?;
    assert!(java_lang
        .iter()
        .any(|t| t.binary_name == "java.lang.String"));

    let pkgs = index.packages()?;
    assert!(pkgs.contains(&"java.lang".to_owned()));
    assert!(pkgs.contains(&"java.util".to_owned()));

    assert!(
        index.module_of_type("java.lang.String").is_none(),
        "legacy jar-backed JdkIndex should not report JPMS modules"
    );

    Ok(())
}

#[test]
fn reads_java_lang_string_class_bytes_from_test_rt_jar() -> Result<(), Box<dyn std::error::Error>> {
    let index = JdkIndex::from_jdk_root(fake_jdk8_root())?;

    let bytes = index
        .read_class_bytes("java/lang/String")?
        .expect("java/lang/String should be present in testdata");
    assert!(
        bytes.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]),
        "class files should start with CAFEBABE"
    );

    Ok(())
}

#[cfg(not(windows))]
#[test]
fn indexes_legacy_boot_classpath_jars_and_dirs() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempdir()?;
    let root = temp.path();

    // Fake a JDK8-like layout:
    //   $ROOT/bin/java
    //   $ROOT/jre/lib/rt.jar
    //   $ROOT/jre/lib/jsse.jar
    //   $ROOT/jre/classes/**/*.class
    let jre_dir = root.join("jre");
    let lib_dir = jre_dir.join("lib");
    std::fs::create_dir_all(&lib_dir)?;

    let rt_jar = lib_dir.join("rt.jar");
    let jsse_jar = lib_dir.join("jsse.jar");

    let dup_rt_bytes = minimal_class_bytes_with_marker("dup/Duplicate", Some("rt"));
    let dup_jsse_bytes = minimal_class_bytes_with_marker("dup/Duplicate", Some("jsse"));

    write_jar(
        &rt_jar,
        &[
            (
                "java/lang/String.class",
                minimal_class_bytes("java/lang/String"),
            ),
            ("dup/Duplicate.class", dup_rt_bytes.clone()),
        ],
    )?;

    write_jar(
        &jsse_jar,
        &[
            (
                "javax/net/ssl/SSLContext.class",
                minimal_class_bytes("javax/net/ssl/SSLContext"),
            ),
            ("dup/Duplicate.class", dup_jsse_bytes),
        ],
    )?;

    let classes_dir = jre_dir.join("classes");
    let from_dir_internal = "com/example/FromDir";
    let from_dir_path = classes_dir.join("com").join("example");
    std::fs::create_dir_all(&from_dir_path)?;
    std::fs::write(
        from_dir_path.join("FromDir.class"),
        minimal_class_bytes(from_dir_internal),
    )?;

    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let java_path = bin_dir.join("java");
    let boot_cp = format!(
        "{}:{}:{}",
        rt_jar.display(),
        jsse_jar.display(),
        classes_dir.display()
    );
    let script = format!(
        "#!/usr/bin/env sh\nif [ \"$1\" = \"-XshowSettings:properties\" ] && [ \"$2\" = \"-version\" ]; then\n  echo \"    java.home = {}\" 1>&2\n  echo \"    sun.boot.class.path = {}\" 1>&2\nfi\nexit 0\n",
        jre_dir.display(),
        boot_cp
    );
    std::fs::write(&java_path, script)?;
    let mut perms = std::fs::metadata(&java_path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&java_path, perms)?;

    let index = JdkIndex::from_jdk_root(root)?;

    assert!(
        index.lookup_type("javax.net.ssl.SSLContext")?.is_some(),
        "should index classes from additional boot jar entries"
    );
    assert!(
        index.lookup_type("com.example.FromDir")?.is_some(),
        "should index classes from directory entries on the boot classpath"
    );

    let bytes = index
        .read_class_bytes("dup/Duplicate")?
        .expect("dup/Duplicate should be present");
    assert_eq!(
        bytes, dup_rt_bytes,
        "when the same internal name exists in multiple containers, the first boot classpath entry should win"
    );

    Ok(())
}

#[test]
fn resolves_static_member_from_jar_stub() -> Result<(), Box<dyn std::error::Error>> {
    let index = JdkIndex::from_jdk_root(fake_jdk8_root())?;

    let owner = TypeName::from("java.lang.Custom");
    let member = Name::from("FOO");
    assert_eq!(
        index.resolve_static_member(&owner, &member),
        Some(StaticMemberId::new("java.lang.Custom::FOO"))
    );

    Ok(())
}

#[test]
fn resolves_static_member_from_jmod_stub() -> Result<(), Box<dyn std::error::Error>> {
    let index = JdkIndex::from_jdk_root(fake_jdk_root())?;

    let owner = TypeName::from("java.lang.Custom");
    let member = Name::from("FOO");
    assert_eq!(
        index.resolve_static_member(&owner, &member),
        Some(StaticMemberId::new("java.lang.Custom::FOO"))
    );

    Ok(())
}

#[test]
fn discovery_prefers_config_override() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = env_lock();

    let fake = fake_jdk_root();
    let _java_home = EnvVarGuard::set("JAVA_HOME", fake.as_os_str());

    let install = JdkInstallation::discover(None)?;
    assert_eq!(install.root(), fake.as_path());

    // Point JAVA_HOME at a bogus directory but still expect the config override
    // to win.
    let bogus = fake.join("bogus");
    let _java_home = EnvVarGuard::set("JAVA_HOME", bogus.as_os_str());
    let cfg = JdkConfig {
        home: Some(fake),
        ..Default::default()
    };

    let install = JdkInstallation::discover(Some(&cfg))?;
    assert_eq!(install.root(), cfg.home.as_deref().unwrap());

    Ok(())
}

#[test]
fn discovery_supports_workspace_config_override() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = env_lock();

    let temp = tempdir()?;
    let workspace_root = temp.path();

    let fake = fake_jdk_root();
    std::fs::write(
        workspace_root.join("nova.toml"),
        format!("[jdk]\njdk_home = '{}'\n", fake.display()),
    )?;

    let _nova_config_path = EnvVarGuard::unset("NOVA_CONFIG_PATH");
    let _java_home = EnvVarGuard::set(
        "JAVA_HOME",
        workspace_root.join("bogus-java-home").as_os_str(),
    );

    let (nova_config, _config_path) = nova_config::load_for_workspace(workspace_root)?;
    // `nova-config` is the source of truth for on-disk settings; `nova-core`
    // exposes a lightweight config used by JDK discovery.
    let jdk_config: JdkConfig = nova_config.jdk_config();

    let install = JdkInstallation::discover(Some(&jdk_config))?;
    assert_eq!(install.root(), fake.as_path());

    Ok(())
}

#[test]
fn discovery_prefers_toolchain_for_configured_release() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = env_lock();

    let toolchain_dir = tempdir()?;
    let toolchain_root = toolchain_dir.path();
    let jmods_dir = toolchain_root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;

    let cfg = JdkConfig {
        home: Some(fake_jdk_root()),
        release: Some(17),
        toolchains: [(17u16, toolchain_root.to_path_buf())]
            .into_iter()
            .collect(),
    };

    let install = JdkInstallation::discover(Some(&cfg))?;
    assert_eq!(install.root(), toolchain_root);

    Ok(())
}

#[test]
fn discovery_for_release_prefers_requested_toolchain() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = env_lock();

    let toolchain_dir = tempdir()?;
    let toolchain_root = toolchain_dir.path();
    let jmods_dir = toolchain_root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;

    let cfg = JdkConfig {
        home: Some(fake_jdk_root()),
        release: None,
        toolchains: [(8u16, toolchain_root.to_path_buf())].into_iter().collect(),
    };

    let install = JdkInstallation::discover_for_release(Some(&cfg), Some(8))?;
    assert_eq!(install.root(), toolchain_root);

    Ok(())
}

#[test]
fn jdk_index_discover_for_release_prefers_requested_toolchain(
) -> Result<(), Box<dyn std::error::Error>> {
    let _lock = env_lock();

    let cfg = JdkConfig {
        home: Some(fake_jdk_root()),
        release: None,
        toolchains: [(8u16, fake_jdk8_root())].into_iter().collect(),
    };

    let index = JdkIndex::discover_for_release(Some(&cfg), Some(8))?;
    assert!(
        index.module_graph().is_none(),
        "requested legacy release should pick jar-backed toolchain"
    );
    assert_eq!(index.info().api_release, Some(8));
    assert!(index.lookup_type("java.lang.String")?.is_some());

    Ok(())
}

#[test]
fn jdk_index_discover_sets_api_release_from_config() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = env_lock();

    let cfg = JdkConfig {
        home: Some(fake_jdk_root()),
        release: Some(8),
        toolchains: [(8u16, fake_jdk8_root())].into_iter().collect(),
    };

    let index = JdkIndex::discover(Some(&cfg))?;
    assert_eq!(index.info().api_release, Some(8));
    assert!(index.lookup_type("java.lang.String")?.is_some());

    Ok(())
}

#[test]
fn jdk_index_discover_for_release_uses_ct_sym_when_release_differs(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let temp = tempdir()?;
    let root = temp.path();

    // Fake a JDK9+ layout with a single `java.base.jmod`.
    let jmods_dir = root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;

    // Provide a release file so we can detect the discovered JDK's spec version (11).
    std::fs::write(
        root.join("release"),
        "JAVA_SPEC_VERSION=\"11\"\nJAVA_VERSION=\"11.0.2\"\n",
    )?;

    // Create a minimal ct.sym containing release 8 stubs for `java.base`.
    let lib_dir = root.join("lib");
    std::fs::create_dir_all(&lib_dir)?;
    let ct_sym_path = lib_dir.join("ct.sym");

    let java_base_jmod = jmods_dir.join("java.base.jmod");
    let string_bytes = read_zip_entry_bytes(&java_base_jmod, "classes/java/lang/String.class")?;
    let module_info_bytes = read_zip_entry_bytes(&java_base_jmod, "classes/module-info.class")?;

    let file = std::fs::File::create(&ct_sym_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("META-INF/sym/8/java.base/java/lang/String.sig", opts)?;
    zip.write_all(&string_bytes)?;
    zip.start_file("META-INF/sym/8/java.base/module-info.sig", opts)?;
    zip.write_all(&module_info_bytes)?;
    zip.finish()?;

    let cfg = JdkConfig {
        home: Some(root.to_path_buf()),
        ..Default::default()
    };
    let index = JdkIndex::discover_for_release(Some(&cfg), Some(8))?;

    assert_eq!(index.info().backing, JdkIndexBacking::CtSym);
    assert_eq!(index.info().api_release, Some(8));

    let graph = index
        .module_graph()
        .expect("ct.sym index should provide a module graph when module-info.sig is present");
    assert!(
        graph.get(&ModuleName::new("java.base")).is_some(),
        "module graph should include java.base"
    );

    let module = index
        .module_of_type("java.lang.String")
        .expect("module lookup should succeed for java.lang.String");
    assert_eq!(module.as_str(), "java.base");

    assert!(index.lookup_type("java.lang.String")?.is_some());

    Ok(())
}

#[test]
fn jdk_index_discover_for_release_errors_when_ct_sym_release_missing(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let temp = tempdir()?;
    let root = temp.path();

    let jmods_dir = root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;

    std::fs::write(
        root.join("release"),
        "JAVA_SPEC_VERSION=\"11\"\nJAVA_VERSION=\"11.0.2\"\n",
    )?;

    let lib_dir = root.join("lib");
    std::fs::create_dir_all(&lib_dir)?;
    let ct_sym_path = lib_dir.join("ct.sym");

    let java_base_jmod = jmods_dir.join("java.base.jmod");
    let string_bytes = read_zip_entry_bytes(&java_base_jmod, "classes/java/lang/String.class")?;
    let module_info_bytes = read_zip_entry_bytes(&java_base_jmod, "classes/module-info.class")?;

    let file = std::fs::File::create(&ct_sym_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    // ct.sym contains only release 11 stubs; request 8 should fail.
    zip.start_file("META-INF/sym/11/java.base/java/lang/String.sig", opts)?;
    zip.write_all(&string_bytes)?;
    zip.start_file("META-INF/sym/11/java.base/module-info.sig", opts)?;
    zip.write_all(&module_info_bytes)?;
    zip.finish()?;

    let cfg = JdkConfig {
        home: Some(root.to_path_buf()),
        ..Default::default()
    };

    let err = JdkIndex::discover_for_release(Some(&cfg), Some(8)).unwrap_err();
    match err {
        JdkIndexError::CtSymReleaseNotFound { release, available } => {
            assert_eq!(release, 8);
            assert_eq!(available, vec![11]);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    Ok(())
}

#[test]
fn discovery_coerces_java_home_jre_subdir() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = env_lock();

    let temp = tempdir()?;
    let root = temp.path();
    let jmods_dir = root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;

    let jre_dir = root.join("jre");
    std::fs::create_dir_all(&jre_dir)?;

    let _java_home = EnvVarGuard::set("JAVA_HOME", jre_dir.as_os_str());
    let install = JdkInstallation::discover(None)?;
    assert_eq!(install.root(), root);

    Ok(())
}

#[test]
fn discovery_coerces_java_home_jre_subdir_for_legacy_rt_jar(
) -> Result<(), Box<dyn std::error::Error>> {
    let _lock = env_lock();

    let temp = tempdir()?;
    let root = temp.path();

    let jre_lib_dir = root.join("jre").join("lib");
    std::fs::create_dir_all(&jre_lib_dir)?;
    std::fs::copy(
        fake_jdk8_root().join("jre/lib/rt.jar"),
        jre_lib_dir.join("rt.jar"),
    )?;

    let _java_home = EnvVarGuard::set("JAVA_HOME", root.join("jre").as_os_str());
    let install = JdkInstallation::discover(None)?;
    assert_eq!(install.root(), root);

    Ok(())
}

#[test]
fn src_zip_is_discovered_in_common_jdk_layouts() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let root = temp.path();

    let jmods_dir = root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;

    let lib_dir = root.join("lib");
    std::fs::create_dir_all(&lib_dir)?;
    let lib_src = lib_dir.join("src.zip");
    std::fs::write(&lib_src, "")?;

    let install = JdkInstallation::from_root(root)?;
    assert_eq!(install.src_zip(), Some(lib_src.clone()));

    let index = JdkIndex::from_jdk_root(root)?;
    let expected_lib_src = std::fs::canonicalize(&lib_src).unwrap_or(lib_src.clone());
    assert_eq!(index.src_zip(), Some(expected_lib_src.as_path()));

    let root_src = root.join("src.zip");
    std::fs::write(&root_src, "")?;
    assert_eq!(install.src_zip(), Some(root_src.clone()));

    let index = JdkIndex::from_jdk_root(root)?;
    let expected_root_src = std::fs::canonicalize(&root_src).unwrap_or(root_src.clone());
    assert_eq!(index.src_zip(), Some(expected_root_src.as_path()));

    Ok(())
}

#[test]
fn maps_internal_names_to_source_entry_paths() {
    assert_eq!(
        internal_name_to_source_entry_path("java/lang/String"),
        "java/lang/String.java"
    );
    assert_eq!(
        internal_name_to_source_entry_path("java/util/Map$Entry"),
        "java/util/Map.java"
    );
    assert_eq!(
        internal_name_to_source_entry_path("java/util/Map$Entry$1"),
        "java/util/Map.java"
    );
}

#[cfg(not(windows))]
#[test]
fn discovery_falls_back_to_java_on_path_via_java_home_property(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let _lock = env_lock();

    let temp = tempdir()?;
    let root = temp.path();

    let jmods_dir = root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;

    let jre_dir = root.join("jre");
    std::fs::create_dir_all(&jre_dir)?;

    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let java_path = bin_dir.join("java");
    let script = format!(
        "#!/usr/bin/env sh\nif [ \"$1\" = \"-XshowSettings:properties\" ] && [ \"$2\" = \"-version\" ]; then\n  echo \"    java.home = {}\" 1>&2\nfi\nexit 0\n",
        jre_dir.display()
    );
    std::fs::write(&java_path, script)?;
    let mut perms = std::fs::metadata(&java_path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&java_path, perms)?;

    let _java_home = EnvVarGuard::unset("JAVA_HOME");

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<std::path::PathBuf> = vec![bin_dir];
    paths.extend(std::env::split_paths(&old_path));
    let new_path = std::env::join_paths(paths)?;
    let _path_guard = EnvVarGuard::set_os("PATH", new_path);

    let install = JdkInstallation::discover(None)?;
    assert_eq!(install.root(), root);

    Ok(())
}

#[cfg(not(windows))]
#[test]
fn discovery_falls_back_to_java_on_path_via_symlink_resolution(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let _lock = env_lock();

    let temp = tempdir()?;
    let root = temp.path();

    let jmods_dir = root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;

    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let java_path = bin_dir.join("java");
    // No `java.home` output so discovery must use the `bin/java -> root` heuristic.
    std::fs::write(&java_path, "#!/usr/bin/env sh\nexit 0\n")?;
    let mut perms = std::fs::metadata(&java_path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&java_path, perms)?;

    let _java_home = EnvVarGuard::unset("JAVA_HOME");

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<std::path::PathBuf> = vec![bin_dir];
    paths.extend(std::env::split_paths(&old_path));
    let new_path = std::env::join_paths(paths)?;
    let _path_guard = EnvVarGuard::set_os("PATH", new_path);

    let install = JdkInstallation::discover(None)?;
    assert_eq!(install.root(), root);

    Ok(())
}

#[test]
fn reuses_persisted_jmod_class_map_cache() -> Result<(), Box<dyn std::error::Error>> {
    let cache_dir = tempdir()?;

    let stats_first = IndexingStats::default();
    let _ = JdkIndex::from_jdk_root_with_cache_and_stats(
        fake_jdk_root(),
        Some(cache_dir.path()),
        Some(&stats_first),
    )?;

    assert_eq!(stats_first.cache_hits(), 0);
    assert_eq!(stats_first.cache_writes(), 1);
    assert!(stats_first.module_scans() > 0);

    let stats_second = IndexingStats::default();
    let index = JdkIndex::from_jdk_root_with_cache_and_stats(
        fake_jdk_root(),
        Some(cache_dir.path()),
        Some(&stats_second),
    )?;

    assert_eq!(stats_second.cache_hits(), 1);
    assert_eq!(stats_second.cache_writes(), 0);
    assert_eq!(stats_second.module_scans(), 0);

    // Ensure the loaded mapping is actually used to locate classes.
    assert!(index.lookup_type("java.lang.String")?.is_some());

    Ok(())
}

#[test]
fn reuses_persisted_jar_class_map_cache() -> Result<(), Box<dyn std::error::Error>> {
    let cache_dir = tempdir()?;

    let stats_first = IndexingStats::default();
    let _ = JdkIndex::from_jdk_root_with_cache_and_stats(
        fake_jdk8_root(),
        Some(cache_dir.path()),
        Some(&stats_first),
    )?;

    assert_eq!(stats_first.cache_hits(), 0);
    assert_eq!(stats_first.cache_writes(), 1);
    assert!(stats_first.module_scans() > 0);

    let stats_second = IndexingStats::default();
    let index = JdkIndex::from_jdk_root_with_cache_and_stats(
        fake_jdk8_root(),
        Some(cache_dir.path()),
        Some(&stats_second),
    )?;

    assert_eq!(stats_second.cache_hits(), 1);
    assert_eq!(stats_second.cache_writes(), 0);
    assert_eq!(stats_second.module_scans(), 0);

    assert!(index.lookup_type("java.lang.String")?.is_some());
    assert!(index.lookup_type("java.util.List")?.is_some());

    Ok(())
}

#[test]
fn corrupted_persisted_jmod_class_map_cache_is_treated_as_miss(
) -> Result<(), Box<dyn std::error::Error>> {
    let cache_dir = tempdir()?;

    let stats_first = IndexingStats::default();
    let _ = JdkIndex::from_jdk_root_with_cache_and_stats(
        fake_jdk_root(),
        Some(cache_dir.path()),
        Some(&stats_first),
    )?;
    assert_eq!(stats_first.cache_writes(), 1);

    let cache_file = find_jdk_symbol_index_cache_file(cache_dir.path());
    let file = std::fs::OpenOptions::new().write(true).open(&cache_file)?;
    file.set_len(0)?;
    drop(file);

    let stats_second = IndexingStats::default();
    let _ = JdkIndex::from_jdk_root_with_cache_and_stats(
        fake_jdk_root(),
        Some(cache_dir.path()),
        Some(&stats_second),
    )?;

    assert_eq!(stats_second.cache_hits(), 0);
    assert!(stats_second.module_scans() > 0);
    assert_eq!(stats_second.cache_writes(), 1);

    Ok(())
}

#[test]
fn fingerprint_mismatch_forces_cache_miss() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let jdk_root = temp.path();
    let jmods_dir = jdk_root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;

    let source_jmod = fake_jdk_root().join("jmods/java.base.jmod");
    let jmod_path = jmods_dir.join("java.base.jmod");
    std::fs::copy(&source_jmod, &jmod_path)?;

    let cache_dir = tempdir()?;

    let stats_first = IndexingStats::default();
    let _ = JdkIndex::from_jdk_root_with_cache_and_stats(
        jdk_root,
        Some(cache_dir.path()),
        Some(&stats_first),
    )?;
    assert_eq!(stats_first.cache_writes(), 1);

    // Ensure the fingerprint changes even on file systems with coarse mtime resolution.
    std::thread::sleep(Duration::from_secs(2));
    let bytes = std::fs::read(&jmod_path)?;
    std::fs::write(&jmod_path, bytes)?;

    let stats_second = IndexingStats::default();
    let _ = JdkIndex::from_jdk_root_with_cache_and_stats(
        jdk_root,
        Some(cache_dir.path()),
        Some(&stats_second),
    )?;

    assert_eq!(stats_second.cache_hits(), 0);
    assert!(stats_second.module_scans() > 0);
    assert_eq!(stats_second.cache_writes(), 1);

    Ok(())
}

#[test]
fn reuses_persisted_ct_sym_release_index_cache() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let temp = tempdir()?;
    let root = temp.path();

    // Fake a JPMS JDK root for spec release 17, but request API release 8 so
    // indexing must use ct.sym.
    let jmods_dir = root.join("jmods");
    std::fs::create_dir_all(&jmods_dir)?;
    std::fs::copy(
        fake_jdk_root().join("jmods/java.base.jmod"),
        jmods_dir.join("java.base.jmod"),
    )?;
    std::fs::write(
        root.join("release"),
        "JAVA_SPEC_VERSION=\"17\"\nJAVA_VERSION=\"17.0.2\"\n",
    )?;

    let lib_dir = root.join("lib");
    std::fs::create_dir_all(&lib_dir)?;
    let ct_sym_path = lib_dir.join("ct.sym");

    let java_base_jmod = jmods_dir.join("java.base.jmod");
    let string_bytes = read_zip_entry_bytes(&java_base_jmod, "classes/java/lang/String.class")?;
    let module_info_bytes = read_zip_entry_bytes(&java_base_jmod, "classes/module-info.class")?;

    let file = std::fs::File::create(&ct_sym_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("META-INF/sym/8/java.base/java/lang/String.sig", opts)?;
    zip.write_all(&string_bytes)?;
    zip.start_file("META-INF/sym/8/java.base/module-info.sig", opts)?;
    zip.write_all(&module_info_bytes)?;
    zip.finish()?;

    let cfg = JdkConfig {
        home: Some(root.to_path_buf()),
        release: Some(8),
        ..Default::default()
    };

    let cache_dir = tempdir()?;

    let stats_first = IndexingStats::default();
    let first = JdkIndex::discover_with_cache_and_stats(
        Some(&cfg),
        Some(cache_dir.path()),
        Some(&stats_first),
    )?;
    assert_eq!(first.info().backing, JdkIndexBacking::CtSym);
    assert!(first.lookup_type("java.lang.String")?.is_some());
    assert_eq!(stats_first.cache_hits(), 0);
    assert_eq!(stats_first.cache_writes(), 1);

    let stats_second = IndexingStats::default();
    let second = JdkIndex::discover_with_cache_and_stats(
        Some(&cfg),
        Some(cache_dir.path()),
        Some(&stats_second),
    )?;
    assert_eq!(second.info().backing, JdkIndexBacking::CtSym);
    assert!(second.lookup_type("java.lang.String")?.is_some());
    assert_eq!(stats_second.cache_hits(), 1);
    assert_eq!(stats_second.cache_writes(), 0);

    Ok(())
}
