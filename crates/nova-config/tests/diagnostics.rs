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
fn validates_extensions_wasm_paths_absolute_without_context() {
    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("missing-abs");
    let missing_str = missing.display().to_string();

    let text = format!(
        r#"
[extensions]
enabled = true
wasm_paths = ["{missing_str}"]
"#
    );

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(&text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::ExtensionsWasmPathMissing {
            toml_path: "extensions.wasm_paths[0]".to_string(),
            resolved: missing,
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

#[test]
fn validates_ai_provider_limits_are_positive() {
    let text = r#"
[ai]
enabled = true

[ai.provider]
max_tokens = 0
timeout_ms = 0
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![
            ConfigValidationError::InvalidValue {
                toml_path: "ai.provider.timeout_ms".to_string(),
                message: "must be >= 1".to_string(),
            },
            ConfigValidationError::InvalidValue {
                toml_path: "ai.provider.max_tokens".to_string(),
                message: "must be >= 1".to_string(),
            },
        ]
    );
}

#[test]
fn validates_ai_feature_timeouts_are_positive() {
    let text = r#"
[ai]
enabled = true

[ai.features]
completion_ranking = true
multi_token_completion = true

[ai.timeouts]
completion_ranking_ms = 0
multi_token_completion_ms = 0
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![
            ConfigWarning::InvalidValue {
                toml_path: "ai.timeouts.completion_ranking_ms".to_string(),
                message: "must be >= 1 when ai.features.completion_ranking is enabled".to_string(),
            },
            ConfigWarning::InvalidValue {
                toml_path: "ai.timeouts.multi_token_completion_ms".to_string(),
                message: "must be >= 1 when ai.features.multi_token_completion is enabled".to_string(),
            },
        ]
    );
}

#[test]
fn validates_ai_embeddings_limits_are_positive() {
    let text = r#"
[ai]
enabled = true

[ai.embeddings]
enabled = true
batch_size = 0
max_memory_bytes = 0
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![
            ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.batch_size".to_string(),
                message: "must be >= 1 when ai.embeddings.enabled is true".to_string(),
            },
            ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.max_memory_bytes".to_string(),
                message: "must be >= 1 when ai.embeddings.enabled is true".to_string(),
            },
        ]
    );
}

#[test]
fn validates_extensions_wasm_limits_are_positive() {
    let text = r#"
[extensions]
enabled = true
wasm_memory_limit_bytes = 0
wasm_timeout_ms = 0
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![
            ConfigWarning::InvalidValue {
                toml_path: "extensions.wasm_memory_limit_bytes".to_string(),
                message: "must be >= 1".to_string(),
            },
            ConfigWarning::InvalidValue {
                toml_path: "extensions.wasm_timeout_ms".to_string(),
                message: "must be >= 1".to_string(),
            },
        ]
    );
}

#[test]
fn warns_when_cloud_code_edits_enabled_without_disabling_anonymize() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
local_only = false
allow_cloud_code_edits = true
allow_code_edits_without_anonymization = true
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "ai.privacy.anonymize".to_string(),
            message: "cloud code edits are disabled while anonymization is enabled; set ai.privacy.anonymize=false".to_string(),
        }]
    );
}

#[test]
fn warns_when_cloud_code_edit_flags_are_set_in_local_only_mode() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
local_only = true
allow_cloud_code_edits = true
allow_code_edits_without_anonymization = true
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![
            ConfigWarning::InvalidValue {
                toml_path: "ai.privacy.allow_cloud_code_edits".to_string(),
                message: "ignored while ai.privacy.local_only=true".to_string(),
            },
            ConfigWarning::InvalidValue {
                toml_path: "ai.privacy.allow_code_edits_without_anonymization".to_string(),
                message: "ignored while ai.privacy.local_only=true".to_string(),
            },
        ]
    );
}

#[test]
fn warns_when_allow_code_edits_without_anonymization_is_set_without_cloud_opt_in() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
local_only = false
anonymize = false
allow_cloud_code_edits = false
allow_code_edits_without_anonymization = true
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "ai.privacy.allow_code_edits_without_anonymization".to_string(),
            message: "has no effect unless ai.privacy.allow_cloud_code_edits=true".to_string(),
        }]
    );
}
