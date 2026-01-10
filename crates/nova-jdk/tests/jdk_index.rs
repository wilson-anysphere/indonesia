use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;

use nova_core::{Name, ProjectConfig, StaticMemberId, TypeIndex, TypeName};
use nova_jdk::{JdkIndex, JdkInstallation};
use nova_types::TypeProvider;
use tempfile::tempdir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn fake_jdk_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/fake-jdk")
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
    assert!(index.lookup_type("String")?.is_some(), "universe-scope lookup");

    let java_lang = index.java_lang_symbols()?;
    assert!(java_lang.iter().any(|t| t.binary_name == "java.lang.String"));

    let pkgs = index.packages()?;
    assert!(pkgs.contains(&"java.lang".to_owned()));

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
    let cfg = ProjectConfig {
        jdk_home: Some(fake),
    };

    let install = JdkInstallation::discover(Some(&cfg))?;
    assert_eq!(install.root(), cfg.jdk_home.as_deref().unwrap());

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
