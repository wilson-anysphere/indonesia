use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;

use nova_core::ProjectConfig;
use nova_jdk::{JdkIndex, JdkInstallation};

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

