use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use nova_classfile::ClassFile;
use nova_core::{JdkConfig, Name, StaticMemberId, TypeIndex, TypeName};
use nova_jdk::{internal_name_to_source_entry_path, IndexingStats, JdkIndex, JdkInstallation};
use nova_modules::ModuleName;
use nova_types::TypeProvider;
use tempfile::tempdir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn fake_jdk_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/fake-jdk")
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

struct EnvVarGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }

    fn set_os(key: &'static str, value: &OsString) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }

    fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn loads_java_lang_string_from_test_jmod() -> Result<(), Box<dyn std::error::Error>> {
    let index = JdkIndex::from_jdk_root(fake_jdk_root())?;

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
    let _guard = ENV_LOCK.lock().unwrap();

    let fake = fake_jdk_root();
    let _java_home = EnvVarGuard::set("JAVA_HOME", &fake);

    let install = JdkInstallation::discover(None)?;
    assert_eq!(install.root(), fake.as_path());

    // Point JAVA_HOME at a bogus directory but still expect the config override
    // to win.
    let bogus = fake.join("bogus");
    let _java_home = EnvVarGuard::set("JAVA_HOME", &bogus);
    let cfg = JdkConfig { home: Some(fake) };

    let install = JdkInstallation::discover(Some(&cfg))?;
    assert_eq!(install.root(), cfg.home.as_deref().unwrap());

    Ok(())
}

#[test]
fn discovery_supports_workspace_config_override() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = ENV_LOCK.lock().unwrap();

    let temp = tempdir()?;
    let workspace_root = temp.path();

    let fake = fake_jdk_root();
    std::fs::write(
        workspace_root.join("nova.toml"),
        format!("[jdk]\njdk_home = '{}'\n", fake.display()),
    )?;

    let _nova_config_path = EnvVarGuard::unset("NOVA_CONFIG_PATH");
    let _java_home = EnvVarGuard::set("JAVA_HOME", &workspace_root.join("bogus-java-home"));

    let (nova_config, _config_path) = nova_config::load_for_workspace(workspace_root)?;
    // `nova-config` is the source of truth for on-disk settings; `nova-core`
    // exposes a lightweight config used by JDK discovery.
    let jdk_config: JdkConfig = nova_config.jdk_config();

    let install = JdkInstallation::discover(Some(&jdk_config))?;
    assert_eq!(install.root(), fake.as_path());

    Ok(())
}

#[test]
fn discovery_coerces_java_home_jre_subdir() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = ENV_LOCK.lock().unwrap();

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

    let _java_home = EnvVarGuard::set("JAVA_HOME", &jre_dir);
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
    assert_eq!(install.src_zip(), Some(lib_src));

    let root_src = root.join("src.zip");
    std::fs::write(&root_src, "")?;
    assert_eq!(install.src_zip(), Some(root_src));

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

    let _guard = ENV_LOCK.lock().unwrap();

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
    let _path_guard = EnvVarGuard::set_os("PATH", &new_path);

    let install = JdkInstallation::discover(None)?;
    assert_eq!(install.root(), root);

    Ok(())
}

#[cfg(not(windows))]
#[test]
fn discovery_falls_back_to_java_on_path_via_symlink_resolution(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let _guard = ENV_LOCK.lock().unwrap();

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
    let _path_guard = EnvVarGuard::set_os("PATH", &new_path);

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
