use nova_ai::PrivacyMode;
use nova_config::{AiConfig, AiProviderKind};
use std::env;

pub(super) fn load_ai_config_from_env() -> Result<Option<(AiConfig, PrivacyMode)>, String> {
    let provider = match env::var("NOVA_AI_PROVIDER") {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };

    let model = env::var("NOVA_AI_MODEL").unwrap_or_else(|_| "default".to_string());
    let api_key = env::var("NOVA_AI_API_KEY").ok();

    let audit_logging = matches!(
        env::var("NOVA_AI_AUDIT_LOGGING").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );

    let cache_enabled = matches!(
        env::var("NOVA_AI_CACHE_ENABLED").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let cache_max_entries = env::var("NOVA_AI_CACHE_MAX_ENTRIES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(256);
    let cache_ttl = env::var("NOVA_AI_CACHE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| std::time::Duration::from_secs(300));

    let timeout = env::var("NOVA_AI_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| std::time::Duration::from_secs(30));
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
    let force_local_only = matches!(
        env::var("NOVA_AI_LOCAL_ONLY").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let anonymize_identifiers = !matches!(
        env::var("NOVA_AI_ANONYMIZE_IDENTIFIERS").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    );
    let include_file_paths = matches!(
        env::var("NOVA_AI_INCLUDE_FILE_PATHS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let allow_cloud_code_edits = matches!(
        env::var("NOVA_AI_ALLOW_CLOUD_CODE_EDITS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let allow_code_edits_without_anonymization = matches!(
        env::var("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let optional_bool = |key: &str| match env::var(key).as_deref() {
        Ok("1") | Ok("true") | Ok("TRUE") => Some(true),
        Ok("0") | Ok("false") | Ok("FALSE") => Some(false),
        _ => None,
    };
    let redact_sensitive_strings = optional_bool("NOVA_AI_REDACT_SENSITIVE_STRINGS");
    let redact_numeric_literals = optional_bool("NOVA_AI_REDACT_NUMERIC_LITERALS");
    let strip_or_redact_comments = optional_bool("NOVA_AI_STRIP_OR_REDACT_COMMENTS");

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
        other => return Err(format!("unknown NOVA_AI_PROVIDER: {other}")),
    };
    if force_local_only {
        cfg.privacy.local_only = true;
    }

    cfg.provider.url = match provider.as_str() {
        "http" => {
            let endpoint = env::var("NOVA_AI_ENDPOINT")
                .map_err(|_| "NOVA_AI_ENDPOINT is required for http provider".to_string())?;
            url::Url::parse(&endpoint).map_err(|e| e.to_string())?
        }
        "ollama" => url::Url::parse(
            &env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| "http://localhost:11434".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "openai_compatible" => {
            let endpoint = env::var("NOVA_AI_ENDPOINT").map_err(|_| {
                "NOVA_AI_ENDPOINT is required for openai_compatible provider".to_string()
            })?;
            url::Url::parse(&endpoint).map_err(|e| e.to_string())?
        }
        "openai" => url::Url::parse(
            &env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| "https://api.openai.com/".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "anthropic" => url::Url::parse(
            &env::var("NOVA_AI_ENDPOINT")
                .unwrap_or_else(|_| "https://api.anthropic.com/".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "gemini" => url::Url::parse(
            &env::var("NOVA_AI_ENDPOINT")
                .unwrap_or_else(|_| "https://generativelanguage.googleapis.com/".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "azure" => {
            let endpoint = env::var("NOVA_AI_ENDPOINT")
                .map_err(|_| "NOVA_AI_ENDPOINT is required for azure provider".to_string())?;
            url::Url::parse(&endpoint).map_err(|e| e.to_string())?
        }
        _ => cfg.provider.url.clone(),
    };

    if provider == "azure" {
        cfg.provider.azure_deployment =
            Some(env::var("NOVA_AI_AZURE_DEPLOYMENT").map_err(|_| {
                "NOVA_AI_AZURE_DEPLOYMENT is required for azure provider".to_string()
            })?);
        cfg.provider.azure_api_version = Some(
            env::var("NOVA_AI_AZURE_API_VERSION").unwrap_or_else(|_| "2024-02-01".to_string()),
        );
    }

    let mut privacy = PrivacyMode::from_ai_privacy_config(&cfg.privacy);
    privacy.include_file_paths = include_file_paths;

    Ok(Some((cfg, privacy)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{EnvVarGuard, ENV_LOCK};

    #[test]
    fn load_ai_config_from_env_exposes_privacy_opt_ins() {
        let _lock = ENV_LOCK.lock().unwrap();

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

        let (cfg, privacy) = load_ai_config_from_env()
            .expect("load_ai_config_from_env")
            .expect("config should be present");
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

            let (cfg, privacy) = load_ai_config_from_env()
                .expect("load_ai_config_from_env")
                .expect("config should be present");
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
            let (_cfg, privacy) = load_ai_config_from_env()
                .expect("load_ai_config_from_env")
                .expect("config should be present");
            assert!(privacy.include_file_paths);
        }

        // `NOVA_AI_LOCAL_ONLY` forces local-only mode regardless of provider.
        {
            let _force_local_only = EnvVarGuard::set("NOVA_AI_LOCAL_ONLY", "1");
            let (cfg, _privacy) = load_ai_config_from_env()
                .expect("load_ai_config_from_env")
                .expect("config should be present");
            assert_eq!(cfg.privacy.local_only, true);
        }
    }
}
