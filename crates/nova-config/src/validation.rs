use std::path::Path;

use crate::diagnostics::{ConfigValidationError, ConfigWarning, ValidationDiagnostics};
use crate::{AiProviderKind, LoggingConfig, NovaConfig};

/// Context for semantic config validation.
///
/// Some validations (like checking whether configured directories exist) require a base directory.
/// `NovaConfig` paths are documented as relative to the workspace root when possible; callers that
/// don't have a workspace root can instead provide the directory containing the config file.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigValidationContext<'a> {
    /// Workspace root used to resolve relative paths in the config.
    pub workspace_root: Option<&'a Path>,
    /// Directory containing the loaded config file, used as a fallback base directory.
    pub config_dir: Option<&'a Path>,
}

impl<'a> ConfigValidationContext<'a> {
    fn base_dir(self) -> Option<&'a Path> {
        self.workspace_root.or(self.config_dir)
    }
}

impl NovaConfig {
    /// Validate semantic invariants for a configuration.
    ///
    /// Validation is best-effort: it attempts to report as many problems as possible in one pass.
    #[must_use]
    pub fn validate(&self) -> ValidationDiagnostics {
        self.validate_with_context(ConfigValidationContext::default())
    }

    /// Like [`NovaConfig::validate`] but with access to additional context such as the workspace root.
    #[must_use]
    pub fn validate_with_context(&self, ctx: ConfigValidationContext<'_>) -> ValidationDiagnostics {
        let mut out = ValidationDiagnostics::default();

        validate_ai(self, &mut out);
        validate_extensions(self, ctx, &mut out);
        validate_generated_sources(self, &mut out);
        validate_logging(self, &mut out);

        out
    }
}

fn validate_generated_sources(config: &NovaConfig, out: &mut ValidationDiagnostics) {
    if matches!(&config.generated_sources.override_roots, Some(roots) if roots.is_empty()) {
        out.warnings
            .push(ConfigWarning::GeneratedSourcesOverrideRootsEmpty);
    }
}

fn validate_logging(config: &NovaConfig, out: &mut ValidationDiagnostics) {
    let normalized = LoggingConfig::normalize_level_directives(&config.logging.level);
    if !config.logging.level.trim().is_empty()
        && tracing_subscriber::EnvFilter::try_new(normalized.clone()).is_err()
    {
        out.warnings.push(ConfigWarning::LoggingLevelInvalid {
            value: config.logging.level.clone(),
            normalized,
        });
    }
}

fn validate_extensions(config: &NovaConfig, ctx: ConfigValidationContext<'_>, out: &mut ValidationDiagnostics) {
    if !config.extensions.enabled {
        return;
    }

    if matches!(config.extensions.wasm_memory_limit_bytes, Some(0)) {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "extensions.wasm_memory_limit_bytes".to_string(),
            message: "must be >= 1".to_string(),
        });
    }

    if matches!(config.extensions.wasm_timeout_ms, Some(0)) {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "extensions.wasm_timeout_ms".to_string(),
            message: "must be >= 1".to_string(),
        });
    }

    let base_dir = ctx.base_dir();

    for (idx, path) in config.extensions.wasm_paths.iter().enumerate() {
        let resolved = if path.is_absolute() {
            path.clone()
        } else if let Some(base_dir) = base_dir {
            base_dir.join(path)
        } else {
            continue;
        };
        let toml_path = format!("extensions.wasm_paths[{idx}]");
        if !resolved.exists() {
            out.warnings.push(ConfigWarning::ExtensionsWasmPathMissing {
                toml_path,
                resolved,
            });
            continue;
        }
        if !resolved.is_dir() {
            out.warnings
                .push(ConfigWarning::ExtensionsWasmPathNotDirectory { toml_path, resolved });
        }
    }
}

fn validate_ai(config: &NovaConfig, out: &mut ValidationDiagnostics) {
    if !config.ai.enabled {
        return;
    }

    validate_ai_code_edit_policy(config, out);

    if config.ai.provider.timeout_ms == 0 {
        out.errors.push(ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.timeout_ms".to_string(),
            message: "must be >= 1".to_string(),
        });
    }

    if config.ai.provider.max_tokens == 0 {
        out.errors.push(ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.max_tokens".to_string(),
            message: "must be >= 1".to_string(),
        });
    }

    if matches!(config.ai.provider.concurrency, Some(0)) {
        out.errors.push(ConfigValidationError::AiConcurrencyZero);
    }

    if config.ai.cache_enabled {
        if config.ai.cache_max_entries == 0 {
            out.errors.push(ConfigValidationError::AiCacheMaxEntriesZero);
        }
        if config.ai.cache_ttl_secs == 0 {
            out.errors.push(ConfigValidationError::AiCacheTtlZero);
        }
    }

    if config.ai.privacy.local_only {
        match config.ai.provider.kind {
            AiProviderKind::InProcessLlama => {}
            AiProviderKind::Ollama | AiProviderKind::OpenAiCompatible | AiProviderKind::Http => {
                if !url_is_loopback(&config.ai.provider.url) {
                    out.errors.push(ConfigValidationError::AiLocalOnlyUrlNotLocal {
                        provider: config.ai.provider.kind.clone(),
                        url: config.ai.provider.url.to_string(),
                    });
                }
            }
            AiProviderKind::OpenAi
            | AiProviderKind::Anthropic
            | AiProviderKind::Gemini
            | AiProviderKind::AzureOpenAi => {
                out.errors
                    .push(ConfigValidationError::AiLocalOnlyForbidsCloudProvider {
                        provider: config.ai.provider.kind.clone(),
                    });
            }
        }
    }

    match config.ai.provider.kind {
        AiProviderKind::OpenAi
        | AiProviderKind::Anthropic
        | AiProviderKind::Gemini
        | AiProviderKind::AzureOpenAi => {
            if config.ai.api_key.as_deref().unwrap_or("").trim().is_empty() {
                out.errors.push(ConfigValidationError::AiMissingApiKey {
                    provider: config.ai.provider.kind.clone(),
                });
            }
        }
        _ => {}
    }

    if matches!(config.ai.provider.kind, AiProviderKind::AzureOpenAi)
        && config.ai.provider.azure_deployment.as_deref().unwrap_or("").trim().is_empty()
    {
        out.errors.push(ConfigValidationError::AiMissingAzureDeployment);
    }

    if matches!(config.ai.provider.kind, AiProviderKind::InProcessLlama)
        && config.ai.provider.in_process_llama.is_none()
    {
        out.errors.push(ConfigValidationError::AiMissingInProcessConfig);
    }

    if config.ai.features.completion_ranking && config.ai.timeouts.completion_ranking_ms == 0 {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "ai.timeouts.completion_ranking_ms".to_string(),
            message: "must be >= 1 when ai.features.completion_ranking is enabled".to_string(),
        });
    }

    if config.ai.features.multi_token_completion && config.ai.timeouts.multi_token_completion_ms == 0 {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "ai.timeouts.multi_token_completion_ms".to_string(),
            message: "must be >= 1 when ai.features.multi_token_completion is enabled".to_string(),
        });
    }

    if config.ai.embeddings.enabled {
        if config.ai.embeddings.batch_size == 0 {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.batch_size".to_string(),
                message: "must be >= 1 when ai.embeddings.enabled is true".to_string(),
            });
        }

        if config.ai.embeddings.max_memory_bytes == 0 {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.max_memory_bytes".to_string(),
                message: "must be >= 1 when ai.embeddings.enabled is true".to_string(),
            });
        }
    }

    if matches!(config.ai.provider.kind, AiProviderKind::InProcessLlama) {
        if let Some(cfg) = config.ai.provider.in_process_llama.as_ref() {
            if cfg.context_size == 0 {
                out.errors.push(ConfigValidationError::InvalidValue {
                    toml_path: "ai.provider.in_process_llama.context_size".to_string(),
                    message: "must be >= 1".to_string(),
                });
            }
        }
    }
}

fn validate_ai_code_edit_policy(config: &NovaConfig, out: &mut ValidationDiagnostics) {
    let privacy = &config.ai.privacy;

    if privacy.local_only {
        if privacy.allow_cloud_code_edits {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: "ai.privacy.allow_cloud_code_edits".to_string(),
                message: "ignored while ai.privacy.local_only=true".to_string(),
            });
        }

        if privacy.allow_code_edits_without_anonymization {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: "ai.privacy.allow_code_edits_without_anonymization".to_string(),
                message: "ignored while ai.privacy.local_only=true".to_string(),
            });
        }

        return;
    }

    // Cloud mode: validate the explicit opt-ins for code-editing workflows.
    if privacy.allow_code_edits_without_anonymization && !privacy.allow_cloud_code_edits {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "ai.privacy.allow_code_edits_without_anonymization".to_string(),
            message: "has no effect unless ai.privacy.allow_cloud_code_edits=true".to_string(),
        });
    }

    if privacy.allow_cloud_code_edits {
        if privacy.effective_anonymize() {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: "ai.privacy.anonymize".to_string(),
                message: "cloud code edits are disabled while anonymization is enabled; set ai.privacy.anonymize=false".to_string(),
            });
        }

        if !privacy.allow_code_edits_without_anonymization {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: "ai.privacy.allow_code_edits_without_anonymization".to_string(),
                message:
                    "cloud code edits require ai.privacy.allow_code_edits_without_anonymization=true"
                        .to_string(),
            });
        }
    }
}

fn url_is_loopback(url: &url::Url) -> bool {
    use url::Host;

    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}
