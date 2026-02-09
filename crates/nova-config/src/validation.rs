use std::path::Path;

use crate::diagnostics::{ConfigValidationError, ConfigWarning, ValidationDiagnostics};
use crate::{AiEmbeddingsBackend, AiProviderKind, LoggingConfig, NovaConfig};

const MAX_IN_PROCESS_LLAMA_CONTEXT_SIZE_TOKENS: usize = 8_192;

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

        validate_ai(self, ctx, &mut out);
        validate_build_integration(self, &mut out);
        validate_extensions(self, ctx, &mut out);
        validate_generated_sources(self, &mut out);
        validate_jdk(self, ctx, &mut out);
        validate_logging(self, &mut out);

        out
    }
}

fn validate_build_integration(config: &NovaConfig, out: &mut ValidationDiagnostics) {
    let cfg = &config.build;
    let maven_on = matches!(cfg.maven_mode(), crate::BuildIntegrationMode::On);
    let gradle_on = matches!(cfg.gradle_mode(), crate::BuildIntegrationMode::On);

    let mut needs_global_timeout_warning = false;

    if maven_on {
        if matches!(cfg.maven.timeout_ms, Some(0)) {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: "build.maven.timeout_ms".to_string(),
                message: "must be >= 1 (0 is treated as 1)".to_string(),
            });
        } else if cfg.maven.timeout_ms.is_none() && cfg.timeout_ms == 0 {
            needs_global_timeout_warning = true;
        }
    }

    if gradle_on {
        if matches!(cfg.gradle.timeout_ms, Some(0)) {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: "build.gradle.timeout_ms".to_string(),
                message: "must be >= 1 (0 is treated as 1)".to_string(),
            });
        } else if cfg.gradle.timeout_ms.is_none() && cfg.timeout_ms == 0 {
            needs_global_timeout_warning = true;
        }
    }

    if needs_global_timeout_warning {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "build.timeout_ms".to_string(),
            message: "must be >= 1 when build.enabled is true (0 is treated as 1)".to_string(),
        });
    }

    if matches!(cfg.base_mode(), crate::BuildIntegrationMode::On) && !maven_on && !gradle_on {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: if matches!(cfg.enabled, Some(true)) {
                "build.enabled".to_string()
            } else {
                "build.mode".to_string()
            },
            message: "build.enabled=true but all build tools are disabled; enable build.maven.enabled and/or build.gradle.enabled"
                .to_string(),
        });
    }
}

fn validate_generated_sources(config: &NovaConfig, out: &mut ValidationDiagnostics) {
    for (idx, path) in config.generated_sources.additional_roots.iter().enumerate() {
        if path.as_os_str().is_empty() {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: format!("generated_sources.additional_roots[{idx}]"),
                message: "must be non-empty".to_string(),
            });
        }
    }

    if matches!(&config.generated_sources.override_roots, Some(roots) if roots.is_empty()) {
        out.warnings
            .push(ConfigWarning::GeneratedSourcesOverrideRootsEmpty);
    }

    if let Some(roots) = config.generated_sources.override_roots.as_ref() {
        for (idx, path) in roots.iter().enumerate() {
            if path.as_os_str().is_empty() {
                out.warnings.push(ConfigWarning::InvalidValue {
                    toml_path: format!("generated_sources.override_roots[{idx}]"),
                    message: "must be non-empty".to_string(),
                });
            }
        }
    }
}

fn validate_jdk(
    config: &NovaConfig,
    ctx: ConfigValidationContext<'_>,
    out: &mut ValidationDiagnostics,
) {
    if matches!(config.jdk.release, Some(0)) {
        out.errors.push(ConfigValidationError::InvalidValue {
            toml_path: "jdk.release".to_string(),
            message: "must be >= 1".to_string(),
        });
    }

    if let Some(home) = config.jdk.home.as_ref() {
        if home.as_os_str().is_empty() {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "jdk.home".to_string(),
                message: "must be non-empty".to_string(),
            });
        } else {
            let base_dir = ctx.base_dir();
            let resolved = if home.is_absolute() {
                Some(home.clone())
            } else {
                base_dir.map(|base_dir| base_dir.join(home))
            };

            if let Some(resolved) = resolved {
                if !resolved.exists() {
                    out.errors.push(ConfigValidationError::InvalidValue {
                        toml_path: "jdk.home".to_string(),
                        message: format!("path does not exist: {}", resolved.display()),
                    });
                } else if !resolved.is_dir() {
                    out.errors.push(ConfigValidationError::InvalidValue {
                        toml_path: "jdk.home".to_string(),
                        message: format!("path is not a directory: {}", resolved.display()),
                    });
                }
            }
        }
    }

    let base_dir = ctx.base_dir();
    let mut seen_releases = std::collections::HashMap::<u16, usize>::new();

    for (idx, toolchain) in config.jdk.toolchains.iter().enumerate() {
        if toolchain.release == 0 {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: format!("jdk.toolchains[{idx}].release"),
                message: "must be >= 1".to_string(),
            });
            continue;
        }

        if let Some(prev_idx) = seen_releases.insert(toolchain.release, idx) {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: format!("jdk.toolchains[{idx}].release"),
                message: format!(
                    "duplicate toolchain release {} (overwriting entry at index {prev_idx})",
                    toolchain.release
                ),
            });
        }

        let home = &toolchain.home;
        if home.as_os_str().is_empty() {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: format!("jdk.toolchains[{idx}].home"),
                message: "must be non-empty".to_string(),
            });
            continue;
        }

        let resolved = if home.is_absolute() {
            home.clone()
        } else if let Some(base_dir) = base_dir {
            base_dir.join(home)
        } else {
            continue;
        };

        if !resolved.exists() {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: format!("jdk.toolchains[{idx}].home"),
                message: format!("path does not exist: {}", resolved.display()),
            });
        } else if !resolved.is_dir() {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: format!("jdk.toolchains[{idx}].home"),
                message: format!("path is not a directory: {}", resolved.display()),
            });
        }
    }
}

fn validate_logging(config: &NovaConfig, out: &mut ValidationDiagnostics) {
    if config.logging.buffer_lines == 0 {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "logging.buffer_lines".to_string(),
            message: "must be >= 1 (0 is treated as 1)".to_string(),
        });
    }

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

fn validate_extensions(
    config: &NovaConfig,
    ctx: ConfigValidationContext<'_>,
    out: &mut ValidationDiagnostics,
) {
    if !config.extensions.enabled {
        return;
    }

    if let Some(allow) = &config.extensions.allow {
        if allow.is_empty() {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: "extensions.allow".to_string(),
                message: "empty allow list disables all extensions; remove it or set extensions.enabled=false"
                    .to_string(),
            });
        }

        for (idx, pattern) in allow.iter().enumerate() {
            if pattern.trim().is_empty() {
                out.warnings.push(ConfigWarning::InvalidValue {
                    toml_path: format!("extensions.allow[{idx}]"),
                    message: "must be non-empty".to_string(),
                });
            }
        }
    }

    for (idx, pattern) in config.extensions.deny.iter().enumerate() {
        if pattern.trim().is_empty() {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: format!("extensions.deny[{idx}]"),
                message: "must be non-empty".to_string(),
            });
        }
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
        if path.as_os_str().is_empty() {
            out.warnings.push(ConfigWarning::InvalidValue {
                toml_path: format!("extensions.wasm_paths[{idx}]"),
                message: "must be non-empty".to_string(),
            });
            continue;
        }

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
                .push(ConfigWarning::ExtensionsWasmPathNotDirectory {
                    toml_path,
                    resolved,
                });
        }
    }
}

fn validate_ai(
    config: &NovaConfig,
    ctx: ConfigValidationContext<'_>,
    out: &mut ValidationDiagnostics,
) {
    if config.ai.audit_log.enabled && !config.ai.enabled {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "ai.audit_log.enabled".to_string(),
            message: "ignored unless ai.enabled=true".to_string(),
        });
    }

    if !config.ai.enabled {
        return;
    }

    validate_ai_privacy_patterns(config, out);

    let scheme = config.ai.provider.url.scheme();
    if scheme != "http" && scheme != "https" {
        out.errors.push(ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.url".to_string(),
            message: format!("unsupported URL scheme {scheme}; expected http or https"),
        });
    }

    if config.ai.provider.model.trim().is_empty() {
        out.errors.push(ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.model".to_string(),
            message: "must be non-empty".to_string(),
        });
    }

    if matches!(config.ai.provider.kind, AiProviderKind::AzureOpenAi)
        && matches!(
            config.ai.provider.azure_api_version.as_deref(),
            Some(value) if value.trim().is_empty()
        )
    {
        out.errors.push(ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.azure_api_version".to_string(),
            message: "must be non-empty when set".to_string(),
        });
    }

    validate_ai_code_edit_policy(config, out);

    // Multi-token completions are privacy-sensitive because the prompt includes identifier-heavy
    // symbol lists (available methods + importable symbols). Nova's cloud multi-token completion
    // provider refuses to call the model when identifier anonymization is enabled, so surface a
    // configuration warning when the feature is enabled in this mode.
    if config.ai.features.multi_token_completion
        && config.ai.privacy.effective_anonymize_identifiers()
    {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "ai.privacy.anonymize_identifiers".to_string(),
            message: "multi-token completions are disabled while identifier anonymization is enabled; set ai.privacy.anonymize_identifiers=false".to_string(),
        });
    }

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

    if let Some(temp) = config.ai.provider.temperature {
        if temp.is_nan() || temp < 0.0 {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.provider.temperature".to_string(),
                message: "must be >= 0".to_string(),
            });
        }
    }

    if config.ai.provider.retry_initial_backoff_ms == 0 {
        out.errors.push(ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.retry_initial_backoff_ms".to_string(),
            message: "must be >= 1".to_string(),
        });
    }

    if config.ai.provider.retry_max_backoff_ms == 0 {
        out.errors.push(ConfigValidationError::InvalidValue {
            toml_path: "ai.provider.retry_max_backoff_ms".to_string(),
            message: "must be >= 1".to_string(),
        });
    }

    if matches!(config.ai.provider.concurrency, Some(0)) {
        out.errors.push(ConfigValidationError::AiConcurrencyZero);
    }

    if config.ai.cache_enabled {
        if config.ai.cache_max_entries == 0 {
            out.errors
                .push(ConfigValidationError::AiCacheMaxEntriesZero);
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
                    out.errors
                        .push(ConfigValidationError::AiLocalOnlyUrlNotLocal {
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
        && config
            .ai
            .provider
            .azure_deployment
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
    {
        out.errors
            .push(ConfigValidationError::AiMissingAzureDeployment);
    }

    if matches!(config.ai.provider.kind, AiProviderKind::InProcessLlama)
        && config.ai.provider.in_process_llama.is_none()
    {
        out.errors
            .push(ConfigValidationError::AiMissingInProcessConfig);
    }

    if matches!(config.ai.provider.kind, AiProviderKind::InProcessLlama) {
        if let Some(cfg) = config.ai.provider.in_process_llama.as_ref() {
            let base_dir = ctx.base_dir();
            if cfg.context_size == 0 {
                out.errors.push(ConfigValidationError::InvalidValue {
                    toml_path: "ai.provider.in_process_llama.context_size".to_string(),
                    message: "must be >= 1".to_string(),
                });
            } else if cfg.context_size > MAX_IN_PROCESS_LLAMA_CONTEXT_SIZE_TOKENS {
                out.errors.push(ConfigValidationError::InvalidValue {
                    toml_path: "ai.provider.in_process_llama.context_size".to_string(),
                    message: format!("must be <= {MAX_IN_PROCESS_LLAMA_CONTEXT_SIZE_TOKENS}"),
                });
            }

            if cfg.temperature.is_nan() || cfg.temperature < 0.0 {
                out.errors.push(ConfigValidationError::InvalidValue {
                    toml_path: "ai.provider.in_process_llama.temperature".to_string(),
                    message: "must be >= 0".to_string(),
                });
            }

            if !(0.0..=1.0).contains(&cfg.top_p) {
                out.errors.push(ConfigValidationError::InvalidValue {
                    toml_path: "ai.provider.in_process_llama.top_p".to_string(),
                    message: "must be within [0, 1]".to_string(),
                });
            }

            if cfg.model_path.as_os_str().is_empty() {
                out.errors.push(ConfigValidationError::InvalidValue {
                    toml_path: "ai.provider.in_process_llama.model_path".to_string(),
                    message: "must be non-empty".to_string(),
                });
            } else {
                let resolved = if cfg.model_path.is_absolute() {
                    Some(cfg.model_path.clone())
                } else {
                    base_dir.map(|base| base.join(&cfg.model_path))
                };

                if let Some(resolved) = resolved {
                    if !resolved.exists() {
                        out.errors.push(ConfigValidationError::InvalidValue {
                            toml_path: "ai.provider.in_process_llama.model_path".to_string(),
                            message: format!("path does not exist: {}", resolved.display()),
                        });
                    } else if !resolved.is_file() {
                        out.errors.push(ConfigValidationError::InvalidValue {
                            toml_path: "ai.provider.in_process_llama.model_path".to_string(),
                            message: format!("path is not a file: {}", resolved.display()),
                        });
                    }
                }
            }
        }
    }

    if config.ai.features.completion_ranking && config.ai.timeouts.completion_ranking_ms == 0 {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "ai.timeouts.completion_ranking_ms".to_string(),
            message: "must be >= 1 when ai.features.completion_ranking is enabled".to_string(),
        });
    }

    if config.ai.features.multi_token_completion
        && config.ai.timeouts.multi_token_completion_ms == 0
    {
        out.warnings.push(ConfigWarning::InvalidValue {
            toml_path: "ai.timeouts.multi_token_completion_ms".to_string(),
            message: "must be >= 1 when ai.features.multi_token_completion is enabled".to_string(),
        });
    }

    if config.ai.embeddings.enabled {
        if config.ai.embeddings.model_dir.as_os_str().is_empty() {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.model_dir".to_string(),
                message: "must be non-empty when ai.embeddings.enabled is true".to_string(),
            });
        }

        if matches!(config.ai.embeddings.backend, AiEmbeddingsBackend::Provider)
            && !matches!(
                config.ai.provider.kind,
                AiProviderKind::Ollama
                    | AiProviderKind::OpenAiCompatible
                    | AiProviderKind::OpenAi
                    | AiProviderKind::AzureOpenAi
            )
        {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.provider.kind".to_string(),
                message: format!(
                    "embeddings are not supported for provider kind {}; supported kinds: ollama, open_ai_compatible, open_ai, azure_open_ai",
                    ai_provider_kind_name(&config.ai.provider.kind)
                ),
            });
        }

        if matches!(
            config.ai.embeddings.model.as_deref(),
            Some(value) if value.trim().is_empty()
        ) {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.model".to_string(),
                message: "must be non-empty when set".to_string(),
            });
        }

        if matches!(config.ai.embeddings.timeout_ms, Some(0)) {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.timeout_ms".to_string(),
                message: "must be >= 1 when set".to_string(),
            });
        }

        if matches!(config.ai.embeddings.backend, AiEmbeddingsBackend::Local)
            && config.ai.embeddings.local_model.trim().is_empty()
        {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.local_model".to_string(),
                message: "must be non-empty when ai.embeddings.backend = \"local\"".to_string(),
            });
        }

        if config.ai.embeddings.batch_size == 0 {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.batch_size".to_string(),
                message: "must be >= 1 when ai.embeddings.enabled is true".to_string(),
            });
        }

        if config.ai.embeddings.max_memory_bytes.0 == 0 {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: "ai.embeddings.max_memory_bytes".to_string(),
                message: "must be >= 1 when ai.embeddings.enabled is true".to_string(),
            });
        }
    }

    // In-process llama config is validated earlier (alongside model path checks) so we can report
    // all relevant issues together.
}

fn validate_ai_privacy_patterns(config: &NovaConfig, out: &mut ValidationDiagnostics) {
    for (idx, pattern) in config.ai.privacy.excluded_paths.iter().enumerate() {
        if pattern.trim().is_empty() {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: format!("ai.privacy.excluded_paths[{idx}]"),
                message: "must be non-empty".to_string(),
            });
            continue;
        }

        if let Err(err) = globset::Glob::new(pattern) {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: format!("ai.privacy.excluded_paths[{idx}]"),
                message: format!("invalid glob pattern: {err}"),
            });
        }
    }

    for (idx, pattern) in config.ai.privacy.redact_patterns.iter().enumerate() {
        if pattern.trim().is_empty() {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: format!("ai.privacy.redact_patterns[{idx}]"),
                message: "must be non-empty".to_string(),
            });
            continue;
        }

        if let Err(err) = regex::Regex::new(pattern) {
            out.errors.push(ConfigValidationError::InvalidValue {
                toml_path: format!("ai.privacy.redact_patterns[{idx}]"),
                message: format!("invalid regex: {err}"),
            });
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
                toml_path: "ai.privacy.anonymize_identifiers".to_string(),
                message: "cloud code edits are disabled while identifier anonymization is enabled; set ai.privacy.anonymize_identifiers=false".to_string(),
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

fn ai_provider_kind_name(kind: &AiProviderKind) -> &'static str {
    match kind {
        AiProviderKind::Ollama => "ollama",
        AiProviderKind::OpenAiCompatible => "open_ai_compatible",
        AiProviderKind::InProcessLlama => "in_process_llama",
        AiProviderKind::OpenAi => "open_ai",
        AiProviderKind::Anthropic => "anthropic",
        AiProviderKind::Gemini => "gemini",
        AiProviderKind::AzureOpenAi => "azure_open_ai",
        AiProviderKind::Http => "http",
    }
}
