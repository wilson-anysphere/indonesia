use nova_config::{AiProviderKind, ConfigValidationError, ConfigWarning, NovaConfig};
use tempfile::tempdir;

#[test]
fn reports_unknown_keys_with_full_paths() {
    let text = r#"
typo = 1

[extensions]
enabeld = true
wasm_paths = []

[ai.provider]
kindd = "ollama"
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.unknown_keys,
        vec!["ai.provider.kindd", "extensions.enabeld", "typo"]
    );
}

#[test]
fn reports_deprecated_keys() {
    let text = r#"
[jdk]
jdk_home = "/tmp/jdk"
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::DeprecatedKey {
            path: "jdk.jdk_home".to_string(),
            message: "jdk.jdk_home is deprecated; use jdk.home instead".to_string(),
        }]
    );
}

#[test]
fn validates_ai_provider_requirements() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
local_only = false

[ai.provider]
kind = "open_ai"
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::AiMissingApiKey {
            provider: AiProviderKind::OpenAi,
        }]
    );
}

#[test]
fn validates_extensions_wasm_paths_exist() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[extensions]
enabled = true
wasm_paths = ["./missing"]
"#,
    )
    .expect("write config");

    let (_config, diagnostics) =
        NovaConfig::load_from_path_with_diagnostics(&config_path).expect("config should parse");

    assert_eq!(diagnostics.unknown_keys, Vec::<String>::new());
    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::ExtensionsWasmPathMissing {
            toml_path: "extensions.wasm_paths[0]".to_string(),
            resolved: dir.path().join("./missing"),
        }]
    );
}

#[test]
fn validates_generated_sources_override_roots_is_not_empty() {
    let text = r#"
[generated_sources]
override_roots = []
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::GeneratedSourcesOverrideRootsEmpty]
    );
}

#[test]
fn validates_logging_level_directives() {
    let text = r#"
[logging]
level = "warn,nova=foo"
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::LoggingLevelInvalid {
            value: "warn,nova=foo".to_string(),
            normalized: "warn,nova=foo".to_string(),
        }]
    );
}

