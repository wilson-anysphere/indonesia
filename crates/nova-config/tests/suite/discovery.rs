use std::ffi::OsString;
use std::sync::Mutex;

use nova_config::{
    discover_config_path, load_for_workspace, load_for_workspace_with_diagnostics, ConfigWarning,
    NovaConfig, NOVA_CONFIG_ENV_VAR,
};
use tempfile::tempdir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

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
fn discovers_nova_toml_in_workspace_root() {
    let _lock = ENV_LOCK.lock().expect("ENV_LOCK mutex poisoned");
    let _env = EnvVarGuard::unset(NOVA_CONFIG_ENV_VAR);

    let dir = tempdir().unwrap();
    let config_path = dir.path().join("nova.toml");
    std::fs::write(&config_path, "[generated_sources]\nenabled = false\n").unwrap();

    let discovered = discover_config_path(dir.path())
        .expect("nova.toml should be discovered when present in workspace root");
    assert_eq!(
        discovered,
        config_path.canonicalize().unwrap_or(config_path),
        "expected config discovery to return the workspace-root nova.toml path"
    );
}

#[test]
fn env_override_wins_over_workspace_file() {
    let _lock = ENV_LOCK.lock().expect("ENV_LOCK mutex poisoned");

    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("nova.toml"),
        "[generated_sources]\nenabled = true\n",
    )
    .unwrap();

    let override_path = dir.path().join("override.toml");
    std::fs::write(
        &override_path,
        "[generated_sources]\nenabled = false\n[logging]\nlevel = \"debug\"\n",
    )
    .unwrap();

    let _env = EnvVarGuard::set_os(NOVA_CONFIG_ENV_VAR, &OsString::from("override.toml"));

    let (config, path) = load_for_workspace(dir.path()).unwrap();
    assert!(
        !config.generated_sources.enabled,
        "expected override config to be loaded"
    );
    assert_eq!(
        path.expect("load_for_workspace should return the resolved config path"),
        override_path.canonicalize().unwrap_or(override_path)
    );
}

#[test]
fn env_override_accepts_absolute_path() {
    let _lock = ENV_LOCK.lock().expect("ENV_LOCK mutex poisoned");

    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("nova.toml"),
        "[generated_sources]\nenabled = true\n",
    )
    .unwrap();

    let override_path = dir.path().join("override.toml");
    std::fs::write(&override_path, "[generated_sources]\nenabled = false\n").unwrap();

    let _env = EnvVarGuard::set(NOVA_CONFIG_ENV_VAR, &override_path);

    let (config, path) = load_for_workspace(dir.path()).unwrap();
    assert!(!config.generated_sources.enabled);
    assert_eq!(
        path.expect("load_for_workspace should return the resolved config path"),
        override_path.canonicalize().unwrap_or(override_path)
    );
}

#[test]
fn missing_config_returns_defaults() {
    let _lock = ENV_LOCK.lock().expect("ENV_LOCK mutex poisoned");
    let _env = EnvVarGuard::unset(NOVA_CONFIG_ENV_VAR);

    let dir = tempdir().unwrap();
    let (config, path) = load_for_workspace(dir.path()).unwrap();
    assert_eq!(path, None);
    assert_eq!(config, NovaConfig::default());
}

#[test]
fn reload_for_workspace_reports_changes() {
    let _lock = ENV_LOCK.lock().expect("ENV_LOCK mutex poisoned");
    let _env = EnvVarGuard::unset(NOVA_CONFIG_ENV_VAR);

    let dir = tempdir().unwrap();
    let config_path = dir.path().join("nova.toml");
    std::fs::write(&config_path, "[generated_sources]\nenabled = false\n").unwrap();

    let (config, path) = load_for_workspace(dir.path()).unwrap();
    assert_eq!(
        path.as_deref(),
        Some(
            config_path
                .canonicalize()
                .unwrap_or(config_path.clone())
                .as_path()
        )
    );
    assert!(!config.generated_sources.enabled);

    let (reloaded, reloaded_path, changed) =
        nova_config::reload_for_workspace(dir.path(), &config, path.as_deref()).unwrap();
    assert!(!changed, "expected reload to report unchanged config");
    assert_eq!(reloaded, config);
    assert_eq!(reloaded_path, path);

    // Mutate the config file and ensure reload detects it.
    std::fs::write(&config_path, "[generated_sources]\nenabled = true\n").unwrap();
    let (reloaded, _reloaded_path, changed) =
        nova_config::reload_for_workspace(dir.path(), &config, path.as_deref()).unwrap();
    assert!(changed, "expected reload to report config changes");
    assert!(reloaded.generated_sources.enabled);
}

#[test]
fn load_for_workspace_with_diagnostics_resolves_relative_paths_from_workspace_root() {
    let _lock = ENV_LOCK.lock().expect("ENV_LOCK mutex poisoned");
    let _env = EnvVarGuard::unset(NOVA_CONFIG_ENV_VAR);

    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".nova")).unwrap();

    let config_path = dir.path().join(".nova/config.toml");
    std::fs::write(
        &config_path,
        r#"
[extensions]
enabled = true
wasm_paths = ["extensions"]
"#,
    )
    .unwrap();

    let (_config, path, diagnostics) = load_for_workspace_with_diagnostics(dir.path()).unwrap();

    assert_eq!(
        path.expect("expected discovered config path"),
        config_path.canonicalize().unwrap_or(config_path.clone())
    );
    assert!(diagnostics.unknown_keys.is_empty());
    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::ExtensionsWasmPathMissing {
            toml_path: "extensions.wasm_paths[0]".to_string(),
            resolved: dir.path().join("extensions"),
        }]
    );
}
