use nova_config::{
    AiEmbeddingsBackend, AiProviderKind, ConfigValidationError, ConfigWarning, NovaConfig,
};
use tempfile::{tempdir, NamedTempFile};

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
fn reports_unknown_keys_in_build_section() {
    let text = r#"
[build]
enabled = true
timeuot_ms = 1000
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(diagnostics.unknown_keys, vec!["build.timeuot_ms"]);
}

#[test]
fn reports_unknown_keys_in_build_tool_sections() {
    let text = r#"
[build]
enabled = true

[build.maven]
enabeld = true
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(diagnostics.unknown_keys, vec!["build.maven.enabeld"]);
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
fn reports_deprecated_jdk_target_release_alias() {
    let text = r#"
[jdk]
target_release = 17
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::DeprecatedKey {
            path: "jdk.target_release".to_string(),
            message: "jdk.target_release is deprecated; use jdk.release instead".to_string(),
        }]
    );
}

#[test]
fn reports_deprecated_ai_privacy_anonymize_alias() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
anonymize = false
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::DeprecatedKey {
            path: "ai.privacy.anonymize".to_string(),
            message:
                "ai.privacy.anonymize is deprecated; use ai.privacy.anonymize_identifiers instead"
                    .to_string(),
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
fn validates_jdk_home_is_non_empty() {
    let text = r#"
[jdk]
home = ""
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "jdk.home".to_string(),
            message: "must be non-empty".to_string(),
        }]
    );
}

#[test]
fn validates_ai_provider_url_scheme_is_http() {
    let text = r#"
[ai]
enabled = true

[ai.provider]
kind = "ollama"
url = "ftp://localhost:11434"
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.url".to_string(),
            message: "unsupported URL scheme ftp; expected http or https".to_string(),
        }]
    );
}

#[test]
fn validates_ai_provider_model_is_non_empty() {
    let text = r#"
[ai]
enabled = true

[ai.provider]
kind = "ollama"
model = ""
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.model".to_string(),
            message: "must be non-empty".to_string(),
        }]
    );
}

#[test]
fn validates_azure_api_version_is_non_empty() {
    let text = r#"
[ai]
enabled = true
api_key = "secret"

[ai.privacy]
local_only = false

[ai.provider]
kind = "azure_open_ai"
azure_deployment = "my-deployment"
azure_api_version = ""
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.azure_api_version".to_string(),
            message: "must be non-empty when set".to_string(),
        }]
    );
}

#[test]
fn validates_in_process_llama_model_path_exists() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.provider]
kind = "in_process_llama"

[ai.provider.in_process_llama]
model_path = "missing.gguf"
"#,
    )
    .expect("write config");

    let (_config, diagnostics) =
        NovaConfig::load_from_path_with_diagnostics(&config_path).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.in_process_llama.model_path".to_string(),
            message: format!(
                "path does not exist: {}",
                dir.path().join("missing.gguf").display()
            ),
        }]
    );
}

#[test]
fn validates_in_process_llama_model_path_is_non_empty() {
    let text = r#"
[ai]
enabled = true

[ai.provider]
kind = "in_process_llama"

[ai.provider.in_process_llama]
model_path = ""
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.in_process_llama.model_path".to_string(),
            message: "must be non-empty".to_string(),
        }]
    );
}

#[test]
fn validates_in_process_llama_context_size_is_bounded() {
    let text = r#"
[ai]
enabled = true

[ai.provider]
kind = "in_process_llama"

[ai.provider.in_process_llama]
model_path = "model.gguf"
context_size = 9000
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.in_process_llama.context_size".to_string(),
            message: "must be <= 8192".to_string(),
        }]
    );
}

#[test]
fn validates_in_process_llama_temperature_is_non_negative() {
    let text = r#"
[ai]
enabled = true

[ai.provider]
kind = "in_process_llama"

[ai.provider.in_process_llama]
model_path = "model.gguf"
temperature = -0.1
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.in_process_llama.temperature".to_string(),
            message: "must be >= 0".to_string(),
        }]
    );
}

#[test]
fn validates_in_process_llama_top_p_is_in_range() {
    let text = r#"
[ai]
enabled = true

[ai.provider]
kind = "in_process_llama"

[ai.provider.in_process_llama]
model_path = "model.gguf"
top_p = 1.1
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.in_process_llama.top_p".to_string(),
            message: "must be within [0, 1]".to_string(),
        }]
    );
}

#[test]
fn validates_ai_privacy_excluded_paths_are_valid_globs() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
excluded_paths = ["["]
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(diagnostics.errors.len(), 1);
    match &diagnostics.errors[0] {
        ConfigValidationError::InvalidValue { toml_path, message } => {
            assert_eq!(toml_path, "ai.privacy.excluded_paths[0]");
            assert!(
                message.starts_with("invalid glob pattern:"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected InvalidValue error, got {other:?}"),
    }
}

#[test]
fn validates_ai_privacy_redact_patterns_are_valid_regexes() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
redact_patterns = ["("]
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(diagnostics.errors.len(), 1);
    match &diagnostics.errors[0] {
        ConfigValidationError::InvalidValue { toml_path, message } => {
            assert_eq!(toml_path, "ai.privacy.redact_patterns[0]");
            assert!(
                message.starts_with("invalid regex:"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected InvalidValue error, got {other:?}"),
    }
}

#[test]
fn validates_ai_privacy_redact_patterns_are_non_empty() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
redact_patterns = [""]
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.privacy.redact_patterns[0]".to_string(),
            message: "must be non-empty".to_string(),
        }]
    );
}

#[test]
fn validates_ai_embeddings_model_dir_is_non_empty() {
    let text = r#"
[ai]
enabled = true

[ai.embeddings]
enabled = true
model_dir = ""
batch_size = 1
max_memory_bytes = 1
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.embeddings.model_dir".to_string(),
            message: "must be non-empty when ai.embeddings.enabled is true".to_string(),
        }]
    );
}

#[test]
fn validates_ai_embeddings_model_dir_is_not_a_file() {
    let tmp_file = NamedTempFile::new().expect("create temp file");
    let path = tmp_file
        .path()
        .canonicalize()
        .unwrap_or_else(|_| tmp_file.path().to_path_buf());

    let text = format!(
        r#"
[ai]
enabled = true

[ai.embeddings]
enabled = true
model_dir = '{path}'
batch_size = 1
max_memory_bytes = 1
"#,
        path = path.display()
    );

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(&text).expect("config should parse");

    assert_eq!(diagnostics.errors.len(), 1);
    match &diagnostics.errors[0] {
        ConfigValidationError::InvalidValue { toml_path, message } => {
            assert_eq!(toml_path, "ai.embeddings.model_dir");
            assert!(
                message.contains("expected a directory"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected InvalidValue error, got {other:?}"),
    }
}

#[test]
fn ai_embeddings_backend_defaults_to_hash_for_backwards_compatible_configs() {
    let text = r#"
[ai]
enabled = true

[ai.embeddings]
enabled = true
"#;

    let (config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(config.ai.embeddings.backend, AiEmbeddingsBackend::Hash);
}

#[test]
fn validates_ai_embeddings_provider_backend_requires_supported_provider() {
    let text = r#"
[ai]
enabled = true

[ai.provider]
kind = "http"

[ai.embeddings]
enabled = true
backend = "provider"
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.kind".to_string(),
            message: "embeddings are not supported for provider kind http; supported kinds: ollama, open_ai_compatible, open_ai, azure_open_ai".to_string(),
        }]
    );
}

#[test]
fn validates_ai_embeddings_model_override_is_non_empty() {
    let text = r#"
[ai]
enabled = true

[ai.embeddings]
enabled = true
model = ""
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.embeddings.model".to_string(),
            message: "must be non-empty when set".to_string(),
        }]
    );
}

#[test]
fn validates_ai_embeddings_timeout_override_is_positive() {
    let text = r#"
[ai]
enabled = true

[ai.embeddings]
enabled = true
timeout_ms = 0
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "ai.embeddings.timeout_ms".to_string(),
            message: "must be >= 1 when set".to_string(),
        }]
    );
}

#[test]
fn warns_when_extensions_allow_is_empty() {
    let text = r#"
[extensions]
enabled = true
allow = []
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "extensions.allow".to_string(),
            message:
                "empty allow list disables all extensions; remove it or set extensions.enabled=false"
                    .to_string(),
        }]
    );
}

#[test]
fn warns_when_extensions_allow_contains_empty_pattern() {
    let text = r#"
[extensions]
enabled = true
allow = [""]
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "extensions.allow[0]".to_string(),
            message: "must be non-empty".to_string(),
        }]
    );
}

#[test]
fn warns_when_extensions_deny_contains_empty_pattern() {
    let text = r#"
[extensions]
enabled = true
deny = [""]
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "extensions.deny[0]".to_string(),
            message: "must be non-empty".to_string(),
        }]
    );
}

#[test]
fn warns_when_extensions_wasm_paths_contains_empty_path() {
    let text = r#"
[extensions]
enabled = true
wasm_paths = [""]
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "extensions.wasm_paths[0]".to_string(),
            message: "must be non-empty".to_string(),
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
fn warns_when_generated_sources_roots_contain_empty_paths() {
    let text = r#"
[generated_sources]
additional_roots = [""]
override_roots = [""]
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![
            ConfigWarning::InvalidValue {
                toml_path: "generated_sources.additional_roots[0]".to_string(),
                message: "must be non-empty".to_string(),
            },
            ConfigWarning::InvalidValue {
                toml_path: "generated_sources.override_roots[0]".to_string(),
                message: "must be non-empty".to_string(),
            },
        ]
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
fn warns_when_logging_buffer_lines_is_zero() {
    let text = r#"
[logging]
buffer_lines = 0
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "logging.buffer_lines".to_string(),
            message: "must be >= 1 (0 is treated as 1)".to_string(),
        }]
    );
}

#[test]
fn warns_when_audit_log_enabled_without_ai_enabled() {
    let text = r#"
[ai.audit_log]
enabled = true
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "ai.audit_log.enabled".to_string(),
            message: "ignored unless ai.enabled=true".to_string(),
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
retry_initial_backoff_ms = 0
retry_max_backoff_ms = 0
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
            ConfigValidationError::InvalidValue {
                toml_path: "ai.provider.retry_initial_backoff_ms".to_string(),
                message: "must be >= 1".to_string(),
            },
            ConfigValidationError::InvalidValue {
                toml_path: "ai.provider.retry_max_backoff_ms".to_string(),
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
                message: "must be >= 1 when ai.features.multi_token_completion is enabled"
                    .to_string(),
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
            toml_path: "ai.privacy.anonymize_identifiers".to_string(),
            message: "cloud code edits are disabled while identifier anonymization is enabled; set ai.privacy.anonymize_identifiers=false".to_string(),
        }]
    );
}

#[test]
fn warns_when_multi_token_completion_enabled_with_anonymize_identifiers_enabled() {
    let text = r#"
[ai]
enabled = true

[ai.privacy]
local_only = false

[ai.features]
multi_token_completion = true
"#;

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "ai.privacy.anonymize_identifiers".to_string(),
            message: "multi-token completions are disabled while identifier anonymization is enabled; set ai.privacy.anonymize_identifiers=false".to_string(),
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
anonymize_identifiers = false
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

#[test]
fn validates_jdk_home_exists() {
    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("missing-jdk");
    let text = format!(
        r#"
[jdk]
home = "{}"
"#,
        missing.display()
    );

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(&text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "jdk.home".to_string(),
            message: format!("path does not exist: {}", missing.display()),
        }]
    );
}

#[test]
fn validates_jdk_home_is_directory() {
    let dir = tempdir().expect("tempdir");
    let file_path = dir.path().join("not-a-dir");
    std::fs::write(&file_path, "not a dir").expect("write file");
    let text = format!(
        r#"
[jdk]
home = "{}"
"#,
        file_path.display()
    );

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(&text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "jdk.home".to_string(),
            message: format!("path is not a directory: {}", file_path.display()),
        }]
    );
}

#[test]
fn warns_when_jdk_toolchain_release_is_duplicated() {
    let dir = tempdir().expect("tempdir");
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir_all(&first).expect("create toolchain dir");
    std::fs::create_dir_all(&second).expect("create toolchain dir");

    let text = format!(
        r#"
[jdk]

[[jdk.toolchains]]
release = 8
home = "{}"

[[jdk.toolchains]]
release = 8
home = "{}"
"#,
        first.display(),
        second.display()
    );

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(&text).expect("config should parse");

    assert!(diagnostics.errors.is_empty());
    assert_eq!(
        diagnostics.warnings,
        vec![ConfigWarning::InvalidValue {
            toml_path: "jdk.toolchains[1].release".to_string(),
            message: "duplicate toolchain release 8 (overwriting entry at index 0)".to_string(),
        }]
    );
}

#[test]
fn validates_jdk_toolchain_release_is_positive() {
    let dir = tempdir().expect("tempdir");
    let toolchain_dir = dir.path().join("jdk0");
    std::fs::create_dir_all(&toolchain_dir).expect("create toolchain dir");

    let text = format!(
        r#"
[jdk]

[[jdk.toolchains]]
release = 0
home = "{}"
"#,
        toolchain_dir.display()
    );

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(&text).expect("config should parse");

    assert!(diagnostics.warnings.is_empty());
    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "jdk.toolchains[0].release".to_string(),
            message: "must be >= 1".to_string(),
        }]
    );
}

#[test]
fn validates_jdk_toolchain_home_exists() {
    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("missing-jdk-toolchain");
    let text = format!(
        r#"
[jdk]

[[jdk.toolchains]]
release = 17
home = "{}"
"#,
        missing.display()
    );

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(&text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "jdk.toolchains[0].home".to_string(),
            message: format!("path does not exist: {}", missing.display()),
        }]
    );
}

#[test]
fn validates_jdk_toolchain_home_is_directory() {
    let dir = tempdir().expect("tempdir");
    let file_path = dir.path().join("not-a-dir");
    std::fs::write(&file_path, "not a dir").expect("write file");

    let text = format!(
        r#"
[jdk]

[[jdk.toolchains]]
release = 17
home = "{}"
"#,
        file_path.display()
    );

    let (_config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(&text).expect("config should parse");

    assert_eq!(
        diagnostics.errors,
        vec![ConfigValidationError::InvalidValue {
            toml_path: "jdk.toolchains[0].home".to_string(),
            message: format!("path is not a directory: {}", file_path.display()),
        }]
    );
}
