use nova_ai::PrivacyMode;
use nova_config::{AiConfig, AiProviderKind};
use std::env::{self, VarError};

#[derive(Debug, Clone)]
pub(super) enum AiEnvWarning {
    NonUnicode {
        key: &'static str,
    },
    MissingRequired {
        key: &'static str,
    },
    InvalidValue {
        key: &'static str,
        value: String,
        expected: &'static str,
        parse_error: Option<String>,
    },
    InvalidUrl {
        key: &'static str,
        value: String,
        error: String,
    },
    UnknownProvider {
        value: String,
    },
}

#[derive(Debug)]
pub(super) struct LoadedAiEnvConfig {
    pub(super) config: Option<(AiConfig, PrivacyMode)>,
    pub(super) warnings: Vec<AiEnvWarning>,
}

fn env_var_lossy(key: &'static str, warnings: &mut Vec<AiEnvWarning>) -> Option<String> {
    match env::var(key) {
        Ok(value) => Some(value),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(_)) => {
            warnings.push(AiEnvWarning::NonUnicode { key });
            None
        }
    }
}

fn required_env(key: &'static str, warnings: &mut Vec<AiEnvWarning>) -> Option<String> {
    match env::var(key) {
        Ok(value) => Some(value),
        Err(VarError::NotPresent) => {
            warnings.push(AiEnvWarning::MissingRequired { key });
            None
        }
        Err(VarError::NotUnicode(_)) => {
            warnings.push(AiEnvWarning::NonUnicode { key });
            None
        }
    }
}

fn parse_bool_env(key: &'static str, warnings: &mut Vec<AiEnvWarning>) -> Option<bool> {
    let raw = env_var_lossy(key, warnings)?;
    match raw.as_str() {
        "1" | "true" | "TRUE" => Some(true),
        "0" | "false" | "FALSE" => Some(false),
        _ => {
            warnings.push(AiEnvWarning::InvalidValue {
                key,
                value: raw,
                expected: "0|1|false|true|FALSE|TRUE",
                parse_error: None,
            });
            None
        }
    }
}

fn parse_usize_env(key: &'static str, default: usize, warnings: &mut Vec<AiEnvWarning>) -> usize {
    let Some(raw) = env_var_lossy(key, warnings) else {
        return default;
    };
    match raw.parse::<usize>() {
        Ok(value) => value,
        Err(err) => {
            warnings.push(AiEnvWarning::InvalidValue {
                key,
                value: raw,
                expected: "positive integer",
                parse_error: Some(err.to_string()),
            });
            default
        }
    }
}

fn parse_u64_env(key: &'static str, default: u64, warnings: &mut Vec<AiEnvWarning>) -> u64 {
    let Some(raw) = env_var_lossy(key, warnings) else {
        return default;
    };
    match raw.parse::<u64>() {
        Ok(value) => value,
        Err(err) => {
            warnings.push(AiEnvWarning::InvalidValue {
                key,
                value: raw,
                expected: "positive integer",
                parse_error: Some(err.to_string()),
            });
            default
        }
    }
}

fn parse_url(
    key: &'static str,
    value: String,
    warnings: &mut Vec<AiEnvWarning>,
) -> Option<url::Url> {
    match url::Url::parse(&value) {
        Ok(url) => Some(url),
        Err(err) => {
            warnings.push(AiEnvWarning::InvalidUrl {
                key,
                value,
                error: err.to_string(),
            });
            None
        }
    }
}

pub(super) fn log_ai_env_warnings(warnings: &[AiEnvWarning]) {
    for warning in warnings {
        match warning {
            AiEnvWarning::NonUnicode { key } => {
                tracing::warn!(
                    target = "nova.lsp",
                    key,
                    "AI env var is not valid unicode; ignoring"
                );
            }
            AiEnvWarning::MissingRequired { key } => {
                tracing::warn!(
                    target = "nova.lsp",
                    key,
                    "missing required AI env var; AI env config is disabled"
                );
            }
            AiEnvWarning::InvalidValue {
                key,
                value,
                expected,
                parse_error,
            } => match parse_error {
                Some(error) => {
                    tracing::warn!(
                        target = "nova.lsp",
                        key,
                        value = %value,
                        expected,
                        error = %error,
                        "invalid AI env var value; using default"
                    );
                }
                None => {
                    tracing::warn!(
                        target = "nova.lsp",
                        key,
                        value = %value,
                        expected,
                        "invalid AI env var value; using default"
                    );
                }
            },
            AiEnvWarning::InvalidUrl { key, value, error } => {
                tracing::warn!(
                    target = "nova.lsp",
                    key,
                    value = %value,
                    error = %error,
                    "invalid AI endpoint URL; AI env config is disabled"
                );
            }
            AiEnvWarning::UnknownProvider { value } => {
                tracing::warn!(
                    target = "nova.lsp",
                    value = %value,
                    "unknown NOVA_AI_PROVIDER; AI env config is disabled"
                );
            }
        }
    }
}

pub(super) fn load_ai_config_from_env() -> LoadedAiEnvConfig {
    let mut warnings = Vec::new();

    let provider = match env_var_lossy("NOVA_AI_PROVIDER", &mut warnings) {
        Some(p) => p,
        None => {
            return LoadedAiEnvConfig {
                config: None,
                warnings,
            };
        }
    };

    let model =
        env_var_lossy("NOVA_AI_MODEL", &mut warnings).unwrap_or_else(|| "default".to_string());
    let api_key = env_var_lossy("NOVA_AI_API_KEY", &mut warnings);

    let audit_logging = parse_bool_env("NOVA_AI_AUDIT_LOGGING", &mut warnings).unwrap_or(false);

    let cache_enabled = parse_bool_env("NOVA_AI_CACHE_ENABLED", &mut warnings).unwrap_or(false);
    let cache_max_entries = parse_usize_env("NOVA_AI_CACHE_MAX_ENTRIES", 256, &mut warnings);
    let cache_ttl =
        std::time::Duration::from_secs(parse_u64_env("NOVA_AI_CACHE_TTL_SECS", 300, &mut warnings));

    let timeout =
        std::time::Duration::from_secs(parse_u64_env("NOVA_AI_TIMEOUT_SECS", 30, &mut warnings));
    // Privacy defaults: safer by default (no paths, anonymize identifiers).
    //
    // Supported env vars (legacy env-var based AI wiring):
    // - `NOVA_AI_ANONYMIZE_IDENTIFIERS=0|false|FALSE` disables identifier anonymization
    //   (default: enabled, even in local-only mode).
    // - `NOVA_AI_INCLUDE_FILE_PATHS=1|true|TRUE` allows including paths in prompts
    //   (default: disabled).
    //
    // Code-editing (patch/workspace-edit) opt-ins:
    // - `NOVA_AI_LOCAL_ONLY=1|true|TRUE` forces `ai.privacy.local_only=true` regardless of
    //   provider kind (default: unset).
    // - `NOVA_AI_ALLOW_CLOUD_CODE_EDITS=1|true|TRUE` maps to
    //   `ai.privacy.allow_cloud_code_edits` (default: false).
    // - `NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION=1|true|TRUE` maps to
    //   `ai.privacy.allow_code_edits_without_anonymization` (default: false).
    //
    // Optional redaction overrides (mirror `ai.privacy.*` config knobs):
    // - `NOVA_AI_REDACT_SENSITIVE_STRINGS=0|1|false|true|FALSE|TRUE`
    // - `NOVA_AI_REDACT_NUMERIC_LITERALS=0|1|false|true|FALSE|TRUE`
    // - `NOVA_AI_STRIP_OR_REDACT_COMMENTS=0|1|false|true|FALSE|TRUE`
    let force_local_only = parse_bool_env("NOVA_AI_LOCAL_ONLY", &mut warnings).unwrap_or(false);
    let anonymize_identifiers =
        parse_bool_env("NOVA_AI_ANONYMIZE_IDENTIFIERS", &mut warnings).unwrap_or(true);
    let include_file_paths =
        parse_bool_env("NOVA_AI_INCLUDE_FILE_PATHS", &mut warnings).unwrap_or(false);
    let allow_cloud_code_edits =
        parse_bool_env("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", &mut warnings).unwrap_or(false);
    let allow_code_edits_without_anonymization = parse_bool_env(
        "NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION",
        &mut warnings,
    )
    .unwrap_or(false);
    let redact_sensitive_strings =
        parse_bool_env("NOVA_AI_REDACT_SENSITIVE_STRINGS", &mut warnings);
    let redact_numeric_literals = parse_bool_env("NOVA_AI_REDACT_NUMERIC_LITERALS", &mut warnings);
    let strip_or_redact_comments =
        parse_bool_env("NOVA_AI_STRIP_OR_REDACT_COMMENTS", &mut warnings);

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.api_key = api_key;
    cfg.audit_log.enabled = audit_logging;
    cfg.cache_enabled = cache_enabled;
    cfg.cache_max_entries = cache_max_entries;
    cfg.cache_ttl_secs = cache_ttl.as_secs().max(1);
    cfg.provider.model = model;
    cfg.provider.timeout_ms = timeout.as_millis().min(u64::MAX as u128) as u64;
    cfg.privacy.anonymize_identifiers = Some(anonymize_identifiers);
    cfg.privacy.redact_sensitive_strings = redact_sensitive_strings;
    cfg.privacy.redact_numeric_literals = redact_numeric_literals;
    cfg.privacy.strip_or_redact_comments = strip_or_redact_comments;
    cfg.privacy.allow_cloud_code_edits = allow_cloud_code_edits;
    cfg.privacy.allow_code_edits_without_anonymization = allow_code_edits_without_anonymization;

    cfg.provider.kind = match provider.as_str() {
        "ollama" => {
            cfg.privacy.local_only = true;
            AiProviderKind::Ollama
        }
        "openai_compatible" => {
            cfg.privacy.local_only = true;
            AiProviderKind::OpenAiCompatible
        }
        "http" => {
            // Treat the legacy env-var based HTTP provider as local-only by default so code-editing
            // actions (Generate tests/method bodies) are available without additional opt-ins.
            //
            // Cloud-mode privacy policy (anonymization + explicit code-edit opt-ins) is still
            // enforced when using `nova.toml` configuration.
            cfg.privacy.local_only = true;
            AiProviderKind::Http
        }
        "openai" => {
            cfg.privacy.local_only = false;
            AiProviderKind::OpenAi
        }
        "anthropic" => {
            cfg.privacy.local_only = false;
            AiProviderKind::Anthropic
        }
        "gemini" => {
            cfg.privacy.local_only = false;
            AiProviderKind::Gemini
        }
        "azure" => {
            cfg.privacy.local_only = false;
            AiProviderKind::AzureOpenAi
        }
        other => {
            warnings.push(AiEnvWarning::UnknownProvider {
                value: other.to_string(),
            });
            return LoadedAiEnvConfig {
                config: None,
                warnings,
            };
        }
    };
    if force_local_only {
        cfg.privacy.local_only = true;
    }

    cfg.provider.url = match provider.as_str() {
        "http" => {
            let Some(endpoint) = required_env("NOVA_AI_ENDPOINT", &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            let Some(url) = parse_url("NOVA_AI_ENDPOINT", endpoint, &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            url
        }
        "ollama" => {
            let endpoint = env_var_lossy("NOVA_AI_ENDPOINT", &mut warnings)
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let Some(url) = parse_url("NOVA_AI_ENDPOINT", endpoint, &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            url
        }
        "openai_compatible" => {
            let Some(endpoint) = required_env("NOVA_AI_ENDPOINT", &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            let Some(url) = parse_url("NOVA_AI_ENDPOINT", endpoint, &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            url
        }
        "openai" => {
            let endpoint = env_var_lossy("NOVA_AI_ENDPOINT", &mut warnings)
                .unwrap_or_else(|| "https://api.openai.com/".to_string());
            let Some(url) = parse_url("NOVA_AI_ENDPOINT", endpoint, &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            url
        }
        "anthropic" => {
            let endpoint = env_var_lossy("NOVA_AI_ENDPOINT", &mut warnings)
                .unwrap_or_else(|| "https://api.anthropic.com/".to_string());
            let Some(url) = parse_url("NOVA_AI_ENDPOINT", endpoint, &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            url
        }
        "gemini" => {
            let endpoint = env_var_lossy("NOVA_AI_ENDPOINT", &mut warnings)
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/".to_string());
            let Some(url) = parse_url("NOVA_AI_ENDPOINT", endpoint, &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            url
        }
        "azure" => {
            let Some(endpoint) = required_env("NOVA_AI_ENDPOINT", &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            let Some(url) = parse_url("NOVA_AI_ENDPOINT", endpoint, &mut warnings) else {
                return LoadedAiEnvConfig {
                    config: None,
                    warnings,
                };
            };
            url
        }
        _ => cfg.provider.url.clone(),
    };

    if provider == "azure" {
        let Some(deployment) = required_env("NOVA_AI_AZURE_DEPLOYMENT", &mut warnings) else {
            return LoadedAiEnvConfig {
                config: None,
                warnings,
            };
        };
        cfg.provider.azure_deployment = Some(deployment);
        cfg.provider.azure_api_version = Some(
            env_var_lossy("NOVA_AI_AZURE_API_VERSION", &mut warnings)
                .unwrap_or_else(|| "2024-02-01".to_string()),
        );
    }

    let mut privacy = PrivacyMode::from_ai_privacy_config(&cfg.privacy);
    privacy.include_file_paths = include_file_paths;

    LoadedAiEnvConfig {
        config: Some((cfg, privacy)),
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{EnvVarGuard, ENV_LOCK};

    #[test]
    fn load_ai_config_from_env_exposes_privacy_opt_ins() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

        let _provider = EnvVarGuard::set("NOVA_AI_PROVIDER", "http");
        let _endpoint = EnvVarGuard::set("NOVA_AI_ENDPOINT", "http://localhost:1234/complete");
        let _model = EnvVarGuard::set("NOVA_AI_MODEL", "default");

        // Baseline: no explicit code-edit opt-ins.
        let _local_only = EnvVarGuard::remove("NOVA_AI_LOCAL_ONLY");
        let _allow_cloud_code_edits = EnvVarGuard::remove("NOVA_AI_ALLOW_CLOUD_CODE_EDITS");
        let _allow_code_edits_without_anonymization =
            EnvVarGuard::remove("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION");
        let _anonymize = EnvVarGuard::remove("NOVA_AI_ANONYMIZE_IDENTIFIERS");
        let _include_file_paths = EnvVarGuard::remove("NOVA_AI_INCLUDE_FILE_PATHS");

        let _redact_sensitive_strings = EnvVarGuard::remove("NOVA_AI_REDACT_SENSITIVE_STRINGS");
        let _redact_numeric_literals = EnvVarGuard::remove("NOVA_AI_REDACT_NUMERIC_LITERALS");
        let _strip_or_redact_comments = EnvVarGuard::remove("NOVA_AI_STRIP_OR_REDACT_COMMENTS");

        let loaded = load_ai_config_from_env();
        assert!(
            loaded.warnings.is_empty(),
            "warnings: {:?}",
            loaded.warnings
        );
        let (cfg, privacy) = loaded.config.expect("config should be present");
        assert_eq!(cfg.privacy.local_only, true);
        assert_eq!(cfg.privacy.anonymize_identifiers, Some(true));
        assert!(!cfg.privacy.allow_cloud_code_edits);
        assert!(!cfg.privacy.allow_code_edits_without_anonymization);
        assert_eq!(cfg.privacy.redact_sensitive_strings, None);
        assert_eq!(cfg.privacy.redact_numeric_literals, None);
        assert_eq!(cfg.privacy.strip_or_redact_comments, None);
        assert!(!privacy.include_file_paths);

        // Explicit opt-in for patch-based code edits (cloud-mode gating).
        {
            let _anonymize = EnvVarGuard::set("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0");
            let _allow_cloud_code_edits = EnvVarGuard::set("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", "1");
            let _allow_code_edits_without_anonymization =
                EnvVarGuard::set("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION", "true");
            let _redact_sensitive_strings =
                EnvVarGuard::set("NOVA_AI_REDACT_SENSITIVE_STRINGS", "0");
            let _redact_numeric_literals =
                EnvVarGuard::set("NOVA_AI_REDACT_NUMERIC_LITERALS", "false");
            let _strip_or_redact_comments =
                EnvVarGuard::set("NOVA_AI_STRIP_OR_REDACT_COMMENTS", "1");

            let loaded = load_ai_config_from_env();
            assert!(
                loaded.warnings.is_empty(),
                "warnings: {:?}",
                loaded.warnings
            );
            let (cfg, privacy) = loaded.config.expect("config should be present");
            assert_eq!(cfg.privacy.local_only, true);
            assert_eq!(cfg.privacy.anonymize_identifiers, Some(false));
            assert!(cfg.privacy.allow_cloud_code_edits);
            assert!(cfg.privacy.allow_code_edits_without_anonymization);
            assert_eq!(cfg.privacy.redact_sensitive_strings, Some(false));
            assert_eq!(cfg.privacy.redact_numeric_literals, Some(false));
            assert_eq!(cfg.privacy.strip_or_redact_comments, Some(true));
            assert!(!privacy.include_file_paths);
        }

        // `NOVA_AI_INCLUDE_FILE_PATHS` explicitly opts into including paths in prompts.
        {
            let _include_file_paths = EnvVarGuard::set("NOVA_AI_INCLUDE_FILE_PATHS", "1");
            let loaded = load_ai_config_from_env();
            assert!(
                loaded.warnings.is_empty(),
                "warnings: {:?}",
                loaded.warnings
            );
            let (_cfg, privacy) = loaded.config.expect("config should be present");
            assert!(privacy.include_file_paths);
        }

        // `NOVA_AI_LOCAL_ONLY` forces local-only mode regardless of provider.
        {
            let _force_local_only = EnvVarGuard::set("NOVA_AI_LOCAL_ONLY", "1");
            let loaded = load_ai_config_from_env();
            assert!(
                loaded.warnings.is_empty(),
                "warnings: {:?}",
                loaded.warnings
            );
            let (cfg, _privacy) = loaded.config.expect("config should be present");
            assert_eq!(cfg.privacy.local_only, true);
        }
    }
}
