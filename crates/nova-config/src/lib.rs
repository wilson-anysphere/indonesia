use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use thiserror::Error;
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use url::Url;

mod diagnostics;
mod schema;
mod validation;

pub use diagnostics::{
    ConfigDiagnostics, ConfigValidationError, ConfigWarning, ValidationDiagnostics,
};
pub use schema::json_schema;
pub use validation::ConfigValidationContext;

/// Tracing target used for AI audit events (prompts / model output).
pub const AI_AUDIT_TARGET: &str = "nova.ai.audit";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GeneratedSourcesConfig {
    /// Whether generated sources should be indexed and participate in resolution.
    #[serde(default = "default_generated_sources_enabled")]
    pub enabled: bool,
    /// Additional generated roots (relative to project root unless absolute).
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub additional_roots: Vec<PathBuf>,
    /// If set, replaces default discovery entirely.
    #[serde(default)]
    #[schemars(with = "Option<Vec<String>>")]
    pub override_roots: Option<Vec<PathBuf>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[schemars(deny_unknown_fields)]
pub struct JdkConfig {
    /// Optional override for the JDK installation to use.
    ///
    /// When set, JDK discovery will use this path instead of searching `JAVA_HOME`
    /// or `java` on `PATH`.
    #[serde(default, alias = "jdk_home")]
    #[schemars(with = "Option<String>")]
    pub home: Option<PathBuf>,
}

fn default_generated_sources_enabled() -> bool {
    true
}

impl Default for GeneratedSourcesConfig {
    fn default() -> Self {
        Self {
            enabled: default_generated_sources_enabled(),
            additional_roots: Vec::new(),
            override_roots: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExtensionsConfig {
    /// Whether extensions are enabled.
    #[serde(default = "default_extensions_enabled")]
    pub enabled: bool,
    /// Directories searched for extension bundles.
    ///
    /// Each directory is scanned for extension bundle folders containing a `nova-ext.toml` manifest.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub wasm_paths: Vec<PathBuf>,
    /// If set, only extensions with an id in this list will be loaded.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    /// Extensions with an id in this list will never be loaded.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Optional upper bound for WASM extension linear memory (in bytes).
    ///
    /// When set, the runtime uses the *minimum* of this value and the per-plugin default.
    /// If unset, the per-plugin default applies.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub wasm_memory_limit_bytes: Option<u64>,
    /// Optional upper bound for WASM extension execution timeouts (in milliseconds).
    ///
    /// When set, the runtime uses the *minimum* of this value and the per-plugin default.
    /// If unset, the per-plugin default applies.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub wasm_timeout_ms: Option<u64>,
}

fn default_extensions_enabled() -> bool {
    true
}

impl Default for ExtensionsConfig {
    fn default() -> Self {
        Self {
            enabled: default_extensions_enabled(),
            wasm_paths: Vec::new(),
            allow: None,
            deny: Vec::new(),
            wasm_memory_limit_bytes: None,
            wasm_timeout_ms: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Top-level Nova configuration loaded from TOML.
///
/// Extensions can be configured via the `[extensions]` table:
/// ```toml
/// [extensions]
/// enabled = true
/// wasm_paths = ["./extensions"]
/// allow = ["example.my_extension"]
/// deny = ["example.bad_extension"]
/// wasm_memory_limit_bytes = 268435456
/// wasm_timeout_ms = 5000
/// ```
pub struct NovaConfig {
    /// Generated sources indexing and discovery configuration.
    #[serde(default)]
    pub generated_sources: GeneratedSourcesConfig,

    /// Workspace-level JDK override configuration.
    #[serde(default)]
    pub jdk: JdkConfig,

    /// Workspace extensions (WASM bundles) configuration.
    #[serde(default)]
    pub extensions: ExtensionsConfig,

    /// Global logging settings for Nova crates.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Offline / local LLM configuration (Ollama, vLLM, etc).
    #[serde(default)]
    pub ai: AiConfig,
}

#[allow(clippy::derivable_impls)]
impl Default for NovaConfig {
    fn default() -> Self {
        Self {
            generated_sources: GeneratedSourcesConfig::default(),
            jdk: JdkConfig::default(),
            extensions: ExtensionsConfig::default(),
            logging: LoggingConfig::default(),
            ai: AiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LoggingConfig {
    /// Logging level for all Nova crates.
    #[serde(default = "LoggingConfig::default_level")]
    pub level: String,

    /// Emit logs in JSON format.
    #[serde(default)]
    pub json: bool,

    /// Mirror logs to stderr (in addition to the in-memory buffer).
    ///
    /// Defaults to enabled so running Nova binaries outside an editor still
    /// produces real-time logs.
    #[serde(default = "LoggingConfig::default_stderr")]
    pub stderr: bool,

    /// Append logs to the given file path (in addition to the in-memory buffer).
    ///
    /// If the file cannot be opened, file logging is disabled while other sinks
    /// remain active.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub file: Option<PathBuf>,

    /// Capture and include backtraces in panic reports.
    #[serde(default)]
    pub include_backtrace: bool,

    /// Number of log lines kept in memory for bug reports.
    #[serde(default = "LoggingConfig::default_buffer_lines")]
    pub buffer_lines: usize,
}

impl LoggingConfig {
    fn default_level() -> String {
        "info".to_owned()
    }

    fn default_stderr() -> bool {
        true
    }

    fn default_buffer_lines() -> usize {
        2_000
    }

    pub(crate) fn normalize_level_directives(input: &str) -> String {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Self::default_level();
        }

        match trimmed.to_ascii_lowercase().as_str() {
            // Simple levels should be forgiving about casing and synonyms.
            "trace" => "trace".to_owned(),
            "debug" => "debug".to_owned(),
            "info" => "info".to_owned(),
            "warn" | "warning" => "warn".to_owned(),
            "error" => "error".to_owned(),
            // Anything else is treated as an `EnvFilter` directive string.
            _ => trimmed.to_owned(),
        }
    }

    fn config_env_filter(&self) -> tracing_subscriber::EnvFilter {
        let directives = Self::normalize_level_directives(&self.level);
        tracing_subscriber::EnvFilter::try_new(directives).unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::default()
                .add_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
        })
    }

    /// Create the effective `EnvFilter` for Nova tracing.
    ///
    /// `LoggingConfig.level` may be either a simple level (`info`, `debug`, ...)
    /// or a full `tracing_subscriber::EnvFilter` directive string.
    ///
    /// If `RUST_LOG` is set, it is merged into the resulting filter.
    pub fn env_filter(&self) -> tracing_subscriber::EnvFilter {
        let env_directives = std::env::var("RUST_LOG")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());

        let config_directives = Self::normalize_level_directives(&self.level);

        match env_directives {
            Some(env_directives) => {
                let combined = format!("{config_directives},{env_directives}");
                tracing_subscriber::EnvFilter::try_new(combined)
                    .or_else(|_| tracing_subscriber::EnvFilter::try_new(env_directives))
                    .unwrap_or_else(|_| self.config_env_filter())
            }
            None => self.config_env_filter(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: Self::default_level(),
            json: false,
            stderr: Self::default_stderr(),
            file: None,
            include_backtrace: false,
            buffer_lines: Self::default_buffer_lines(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiFeaturesConfig {
    /// Enables AI-assisted completion re-ranking.
    #[serde(default)]
    pub completion_ranking: bool,

    /// Enables semantic search-based features.
    #[serde(default)]
    pub semantic_search: bool,

    /// Enables multi-token completion suggestions.
    #[serde(default)]
    pub multi_token_completion: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for AiFeaturesConfig {
    fn default() -> Self {
        Self {
            completion_ranking: false,
            semantic_search: false,
            multi_token_completion: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiTimeoutsConfig {
    /// Timeout for completion ranking requests.
    #[serde(default = "default_completion_ranking_timeout_ms")]
    #[schemars(range(min = 1))]
    pub completion_ranking_ms: u64,

    /// Timeout for multi-token completion requests.
    #[serde(default = "default_multi_token_completion_timeout_ms")]
    #[schemars(range(min = 1))]
    pub multi_token_completion_ms: u64,
}

fn default_completion_ranking_timeout_ms() -> u64 {
    20
}

fn default_multi_token_completion_timeout_ms() -> u64 {
    250
}

impl Default for AiTimeoutsConfig {
    fn default() -> Self {
        Self {
            completion_ranking_ms: default_completion_ranking_timeout_ms(),
            multi_token_completion_ms: default_multi_token_completion_timeout_ms(),
        }
    }
}

impl AiTimeoutsConfig {
    pub fn completion_ranking(&self) -> Duration {
        Duration::from_millis(self.completion_ranking_ms)
    }

    pub fn multi_token_completion(&self) -> Duration {
        Duration::from_millis(self.multi_token_completion_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiConfig {
    #[serde(default)]
    pub provider: AiProviderConfig,
    #[serde(default)]
    pub privacy: AiPrivacyConfig,

    /// Local embedding model configuration used for offline semantic search.
    #[serde(default)]
    pub embeddings: AiEmbeddingsConfig,

    /// Enables AI-assisted features. When enabled, **audit** logging may be
    /// enabled separately to capture prompts and model output (sanitized).
    #[serde(default)]
    pub enabled: bool,

    /// API key for the configured provider. This should never be included in
    /// bug report bundles.
    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default)]
    pub audit_log: AuditLogConfig,

    /// Local augmentation features and their individual toggles.
    #[serde(default)]
    pub features: AiFeaturesConfig,

    /// Timeouts for latency-sensitive AI operations.
    #[serde(default)]
    pub timeouts: AiTimeoutsConfig,
    /// Enable in-memory response caching for LLM calls made via `nova-ai`.
    ///
    /// Defaults to `false` (conservative): caching may retain model output in
    /// memory, so consumers must explicitly opt in.
    #[serde(default)]
    pub cache_enabled: bool,

    /// Maximum number of cached responses to keep in memory.
    #[serde(default = "default_ai_cache_max_entries")]
    #[schemars(range(min = 1))]
    pub cache_max_entries: usize,

    /// Cache TTL in seconds.
    #[serde(default = "default_ai_cache_ttl_secs")]
    #[schemars(range(min = 1))]
    pub cache_ttl_secs: u64,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: AiProviderConfig::default(),
            privacy: AiPrivacyConfig::default(),
            embeddings: AiEmbeddingsConfig::default(),
            enabled: false,
            api_key: None,
            audit_log: AuditLogConfig::default(),
            features: AiFeaturesConfig::default(),
            timeouts: AiTimeoutsConfig::default(),
            cache_enabled: false,
            cache_max_entries: default_ai_cache_max_entries(),
            cache_ttl_secs: default_ai_cache_ttl_secs(),
        }
    }
}

fn default_ai_cache_max_entries() -> usize {
    256
}

fn default_ai_cache_ttl_secs() -> u64 {
    300
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiEmbeddingsConfig {
    /// Enable local embeddings for semantic search and context building.
    #[serde(default)]
    pub enabled: bool,

    /// Directory containing embedding model files / cache.
    #[serde(default = "default_embeddings_model_dir")]
    #[schemars(with = "String")]
    pub model_dir: PathBuf,

    /// Maximum batch size for embedding requests.
    #[serde(default = "default_embeddings_batch_size")]
    #[schemars(range(min = 1))]
    pub batch_size: usize,

    /// Soft memory budget (in bytes) for embedding models / caches.
    #[serde(default = "default_embeddings_max_memory_bytes")]
    #[schemars(range(min = 1))]
    pub max_memory_bytes: usize,
}

fn default_embeddings_model_dir() -> PathBuf {
    PathBuf::from(".nova/models/embeddings")
}

fn default_embeddings_batch_size() -> usize {
    32
}

fn default_embeddings_max_memory_bytes() -> usize {
    512 * 1024 * 1024
}

impl Default for AiEmbeddingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_dir: default_embeddings_model_dir(),
            batch_size: default_embeddings_batch_size(),
            max_memory_bytes: default_embeddings_max_memory_bytes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AuditLogConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub path: Option<PathBuf>,
}

#[allow(clippy::derivable_impls)]
impl Default for AuditLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AiProviderKind {
    /// Ollama HTTP API (e.g. http://localhost:11434)
    Ollama,
    /// OpenAI-compatible endpoints (e.g. vLLM, llama.cpp server, etc.)
    OpenAiCompatible,
    /// In-process local inference using a GGUF model file (llama.cpp).
    InProcessLlama,
    /// OpenAI's public API (https://api.openai.com). Requires `ai.api_key`.
    OpenAi,
    /// Anthropic Messages API (https://api.anthropic.com). Requires `ai.api_key`.
    Anthropic,
    /// Google Gemini Generative Language API (https://generativelanguage.googleapis.com).
    /// Requires `ai.api_key`.
    Gemini,
    /// Azure OpenAI. Requires `ai.api_key` and `ai.provider.azure_deployment`.
    AzureOpenAi,
    /// A simple JSON-over-HTTP API (useful for proxies and tests).
    ///
    /// Request body:
    /// `{ "model": "...", "prompt": "...", "max_tokens": 123, "temperature": 0.2 }`
    ///
    /// Response body:
    /// `{ "completion": "..." }`
    Http,
}

#[allow(clippy::derivable_impls)]
impl Default for AiProviderKind {
    fn default() -> Self {
        Self::Ollama
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiProviderConfig {
    /// Which backend implementation to use.
    #[serde(default)]
    pub kind: AiProviderKind,

    /// Base URL for the provider (e.g. http://localhost:11434, http://localhost:8000).
    #[serde(default = "default_provider_url")]
    #[schemars(schema_with = "crate::schema::url_schema")]
    pub url: Url,

    /// Default model name.
    #[serde(default = "default_model_name")]
    pub model: String,

    /// Azure OpenAI deployment name.
    ///
    /// Required when `kind = "azure_open_ai"`.
    #[serde(default)]
    pub azure_deployment: Option<String>,

    /// Azure OpenAI API version (e.g. `2024-02-01`).
    ///
    /// If unset, Nova defaults to `2024-02-01`.
    #[serde(default)]
    pub azure_api_version: Option<String>,

    /// Default max tokens for responses.
    #[serde(default = "default_max_tokens")]
    #[schemars(range(min = 1))]
    pub max_tokens: u32,

    /// Per-request timeout.
    #[serde(default = "default_timeout_ms")]
    #[schemars(range(min = 1))]
    pub timeout_ms: u64,

    /// Maximum number of concurrent requests Nova will make to the backend.
    ///
    /// If unset, defaults to:
    /// - `1` for [`AiProviderKind::InProcessLlama`]
    /// - `4` for HTTP providers
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub concurrency: Option<usize>,

    /// Configuration for in-process inference when `kind = "in_process_llama"`.
    #[serde(default)]
    pub in_process_llama: Option<InProcessLlamaConfig>,
}

fn default_provider_url() -> Url {
    Url::parse("http://localhost:11434").expect("valid default url")
}

fn default_model_name() -> String {
    "llama3".to_string()
}

fn default_max_tokens() -> u32 {
    1024
}

fn default_timeout_ms() -> u64 {
    60_000
}

fn default_concurrency() -> usize {
    4
}

impl Default for AiProviderConfig {
    fn default() -> Self {
        Self {
            kind: AiProviderKind::default(),
            url: default_provider_url(),
            model: default_model_name(),
            azure_deployment: None,
            azure_api_version: None,
            max_tokens: default_max_tokens(),
            timeout_ms: default_timeout_ms(),
            concurrency: None,
            in_process_llama: None,
        }
    }
}

impl AiProviderConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    pub fn effective_concurrency(&self) -> usize {
        self.concurrency.unwrap_or_else(|| match self.kind {
            AiProviderKind::InProcessLlama => 1,
            AiProviderKind::Ollama
            | AiProviderKind::OpenAiCompatible
            | AiProviderKind::OpenAi
            | AiProviderKind::Anthropic
            | AiProviderKind::Gemini
            | AiProviderKind::AzureOpenAi
            | AiProviderKind::Http => default_concurrency(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct InProcessLlamaConfig {
    /// Path to a GGUF model file on disk.
    #[schemars(with = "String")]
    pub model_path: PathBuf,

    /// Context window size (`n_ctx`) used for inference.
    ///
    /// Larger values increase memory usage roughly linearly.
    #[serde(default = "default_in_process_llama_context_size")]
    #[schemars(range(min = 1))]
    pub context_size: usize,

    /// Number of CPU threads to use (`n_threads`).
    ///
    /// If unset or set to `0`, the backend will use the available parallelism.
    #[serde(default)]
    pub threads: Option<usize>,

    /// Sampling temperature.
    #[serde(default = "default_in_process_llama_temperature")]
    pub temperature: f32,

    /// Nucleus sampling probability.
    #[serde(default = "default_in_process_llama_top_p")]
    pub top_p: f32,

    /// Number of layers to offload to GPU (if supported by the build).
    ///
    /// A value of `0` disables GPU offload.
    #[serde(default)]
    pub gpu_layers: u32,
}

fn default_in_process_llama_context_size() -> usize {
    4096
}

fn default_in_process_llama_temperature() -> f32 {
    0.2
}

fn default_in_process_llama_top_p() -> f32 {
    0.95
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiPrivacyConfig {
    /// If true, Nova will not use any cloud providers. This is the recommended
    /// setting for privacy-sensitive environments.
    #[serde(default = "default_local_only")]
    pub local_only: bool,

    /// If unset, defaults to:
    /// - `false` when `local_only = true`
    /// - `true` when `local_only = false` (cloud mode)
    ///
    /// This matches Nova's privacy philosophy: anonymize identifiers when
    /// sending code to a third-party, but avoid needless transformations when
    /// everything stays local.
    ///
    /// Backwards compatibility: `ai.privacy.anonymize` is accepted as an alias
    /// for this field.
    #[serde(default, alias = "anonymize")]
    pub anonymize_identifiers: Option<bool>,

    /// Redact suspicious string literals (API keys, tokens, passwords) before
    /// sending code to an AI provider.
    ///
    /// If unset, defaults to:
    /// - `false` when `local_only = true` (unless identifier anonymization is explicitly enabled)
    /// - `true` when `local_only = false` (cloud mode)
    #[serde(default)]
    pub redact_sensitive_strings: Option<bool>,

    /// Redact suspiciously long numeric literals (IDs / hashes) before sending
    /// code to an AI provider.
    ///
    /// If unset, defaults to:
    /// - `false` when `local_only = true` (unless identifier anonymization is explicitly enabled)
    /// - `true` when `local_only = false` (cloud mode)
    #[serde(default)]
    pub redact_numeric_literals: Option<bool>,

    /// Strip or redact comment bodies before sending code to an AI provider.
    ///
    /// If unset, defaults to:
    /// - `false` when `local_only = true` (unless identifier anonymization is explicitly enabled)
    /// - `true` when `local_only = false` (cloud mode)
    #[serde(default)]
    pub strip_or_redact_comments: Option<bool>,

    /// Glob patterns for file paths that must never be sent to the LLM.
    #[serde(default)]
    pub excluded_paths: Vec<String>,

    /// Regex patterns to redact from any text that will be sent to the LLM.
    #[serde(default)]
    pub redact_patterns: Vec<String>,

    /// Allow AI-assisted code edits (patches / file modifications) when
    /// `local_only = false` (cloud mode).
    ///
    /// This is intentionally opt-in because code-edit prompts typically include
    /// larger portions of source code and the resulting edits can directly
    /// modify project files.
    #[serde(default)]
    pub allow_cloud_code_edits: bool,

    /// Allow AI-assisted code edits when anonymization is disabled.
    ///
    /// In cloud mode (`local_only = false`), disabling anonymization means raw
    /// source identifiers will be sent to the provider. This flag is an
    /// additional opt-in to avoid accidental leakage.
    #[serde(default)]
    pub allow_code_edits_without_anonymization: bool,
}

fn default_local_only() -> bool {
    true
}

impl Default for AiPrivacyConfig {
    fn default() -> Self {
        Self {
            local_only: default_local_only(),
            anonymize_identifiers: None,
            redact_sensitive_strings: None,
            redact_numeric_literals: None,
            strip_or_redact_comments: None,
            excluded_paths: Vec::new(),
            redact_patterns: Vec::new(),
            allow_cloud_code_edits: false,
            allow_code_edits_without_anonymization: false,
        }
    }
}

impl AiPrivacyConfig {
    /// Resolve the effective anonymization flag based on privacy defaults.
    pub fn effective_anonymize(&self) -> bool {
        self.effective_anonymize_identifiers()
    }

    pub fn effective_anonymize_identifiers(&self) -> bool {
        match self.anonymize_identifiers {
            Some(value) => value,
            None => !self.local_only,
        }
    }

    pub fn effective_redact_sensitive_strings(&self) -> bool {
        match self.redact_sensitive_strings {
            Some(value) => value,
            None => !self.local_only || self.anonymize_identifiers.unwrap_or(false),
        }
    }

    pub fn effective_redact_numeric_literals(&self) -> bool {
        match self.redact_numeric_literals {
            Some(value) => value,
            None => !self.local_only || self.anonymize_identifiers.unwrap_or(false),
        }
    }

    pub fn effective_strip_or_redact_comments(&self) -> bool {
        match self.strip_or_redact_comments {
            Some(value) => value,
            None => !self.local_only || self.anonymize_identifiers.unwrap_or(false),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse toml config: {0}")]
    Toml(#[from] toml::de::Error),
}

impl NovaConfig {
    /// Load a config file from TOML.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Ok(toml::from_str(&text)?)
    }

    /// Load a config file from TOML and return diagnostics (unknown keys, deprecated keys, and
    /// semantic validation failures).
    pub fn load_from_path_with_diagnostics(
        path: impl AsRef<Path>,
    ) -> Result<(Self, ConfigDiagnostics), ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;

        let ctx = ConfigValidationContext {
            workspace_root: None,
            config_dir: path.parent(),
        };
        Self::load_from_str_with_diagnostics_inner(&text, ctx)
    }

    /// Load a config from a TOML string and return diagnostics.
    pub fn load_from_str_with_diagnostics(
        text: &str,
    ) -> Result<(Self, ConfigDiagnostics), ConfigError> {
        Self::load_from_str_with_diagnostics_inner(text, ConfigValidationContext::default())
    }

    fn load_from_str_with_diagnostics_inner(
        text: &str,
        ctx: ConfigValidationContext<'_>,
    ) -> Result<(Self, ConfigDiagnostics), ConfigError> {
        let (config, unknown_keys) =
            diagnostics::deserialize_toml_with_unknown_keys::<NovaConfig>(text)?;

        let mut diagnostics = ConfigDiagnostics {
            unknown_keys,
            ..ConfigDiagnostics::default()
        };

        if let Ok(value) = toml::from_str::<toml::Value>(text) {
            diagnostics.warnings.extend(deprecation_warnings(&value));
        }

        diagnostics.extend_validation(config.validate_with_context(ctx));

        Ok((config, diagnostics))
    }

    pub fn jdk_config(&self) -> nova_core::JdkConfig {
        nova_core::JdkConfig {
            home: self.jdk.home.clone(),
        }
    }
}

pub const NOVA_CONFIG_ENV_VAR: &str = "NOVA_CONFIG_PATH";

/// Discover the Nova configuration file for a workspace root.
///
/// Search order:
/// 1) `NOVA_CONFIG_PATH` (absolute or relative to `workspace_root`)
/// 2) `nova.toml` in `workspace_root`
/// 3) `.nova.toml` in `workspace_root`
/// 4) `nova.config.toml` in `workspace_root`
/// 5) `.nova/config.toml` in `workspace_root` (legacy fallback)
pub fn discover_config_path(workspace_root: &Path) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(NOVA_CONFIG_ENV_VAR) {
        let candidate = PathBuf::from(value);
        let path = if candidate.is_absolute() {
            candidate
        } else {
            workspace_root.join(candidate)
        };
        return Some(path.canonicalize().unwrap_or(path));
    }

    let candidates = [
        "nova.toml",
        ".nova.toml",
        "nova.config.toml",
        // Legacy workspace-local config (kept for backwards compatibility).
        ".nova/config.toml",
    ];
    candidates
        .into_iter()
        .map(|name| workspace_root.join(name))
        .find(|path| path.is_file())
        .map(|path| path.canonicalize().unwrap_or(path))
}

/// Load the Nova configuration for a workspace root.
///
/// If no config is present, returns [`NovaConfig::default`] and `None`.
pub fn load_for_workspace(
    workspace_root: &Path,
) -> Result<(NovaConfig, Option<PathBuf>), ConfigError> {
    let Some(path) = discover_config_path(workspace_root) else {
        return Ok((NovaConfig::default(), None));
    };

    let config = NovaConfig::load_from_path(&path)?;
    Ok((config, Some(path)))
}

/// Load the Nova configuration for a workspace root with diagnostics.
///
/// If no config is present, returns [`NovaConfig::default`], `None`, and empty diagnostics.
pub fn load_for_workspace_with_diagnostics(
    workspace_root: &Path,
) -> Result<(NovaConfig, Option<PathBuf>, ConfigDiagnostics), ConfigError> {
    let Some(path) = discover_config_path(workspace_root) else {
        return Ok((NovaConfig::default(), None, ConfigDiagnostics::default()));
    };

    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;

    let ctx = ConfigValidationContext {
        workspace_root: Some(workspace_root),
        config_dir: path.parent(),
    };

    let (config, diagnostics) = NovaConfig::load_from_str_with_diagnostics_inner(&text, ctx)?;
    Ok((config, Some(path), diagnostics))
}

/// Reload the Nova configuration for a workspace root and report whether it changed.
pub fn reload_for_workspace(
    workspace_root: &Path,
    previous: &NovaConfig,
    previous_path: Option<&Path>,
) -> Result<(NovaConfig, Option<PathBuf>, bool), ConfigError> {
    let (config, path) = load_for_workspace(workspace_root)?;
    let changed = path.as_deref() != previous_path || &config != previous;
    Ok((config, path, changed))
}

/// Reload the Nova configuration for a workspace root with diagnostics and report whether it
/// changed.
pub fn reload_for_workspace_with_diagnostics(
    workspace_root: &Path,
    previous: &NovaConfig,
    previous_path: Option<&Path>,
) -> Result<(NovaConfig, Option<PathBuf>, bool, ConfigDiagnostics), ConfigError> {
    let (config, path, diagnostics) = load_for_workspace_with_diagnostics(workspace_root)?;
    let changed = path.as_deref() != previous_path || &config != previous;
    Ok((config, path, changed, diagnostics))
}

fn deprecation_warnings(value: &toml::Value) -> Vec<ConfigWarning> {
    let mut out = Vec::new();

    if let Some(jdk) = value.get("jdk").and_then(|v| v.as_table()) {
        if jdk.contains_key("jdk_home") {
            out.push(ConfigWarning::DeprecatedKey {
                path: "jdk.jdk_home".to_string(),
                message: "jdk.jdk_home is deprecated; use jdk.home instead".to_string(),
            });
        }
    }

    out
}

/// Ring buffer of formatted log lines for bug reports.
#[derive(Debug)]
pub struct LogBuffer {
    capacity: usize,
    inner: Mutex<VecDeque<String>>,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(VecDeque::with_capacity(capacity.min(8_192))),
        }
    }

    pub fn push_line(&self, line: String) {
        let mut inner = self.inner.lock().expect("LogBuffer mutex poisoned");
        if inner.len() == self.capacity {
            inner.pop_front();
        }
        inner.push_back(line);
    }

    pub fn last_lines(&self, n: usize) -> Vec<String> {
        let inner = self.inner.lock().expect("LogBuffer mutex poisoned");
        inner.iter().rev().take(n).cloned().rev().collect()
    }
}

struct LogBufferMakeWriter {
    buffer: Arc<LogBuffer>,
}

impl<'a> MakeWriter<'a> for LogBufferMakeWriter {
    type Writer = LogBufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogBufferWriter {
            buffer: self.buffer.clone(),
            bytes: Vec::new(),
        }
    }
}

struct LogBufferWriter {
    buffer: Arc<LogBuffer>,
    bytes: Vec<u8>,
}

impl Write for LogBufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogBufferWriter {
    fn drop(&mut self) {
        if self.bytes.is_empty() {
            return;
        }

        let text = String::from_utf8_lossy(&self.bytes);
        for line in text.split_terminator('\n') {
            let line = line.trim_end_matches('\r');
            if !line.is_empty() {
                self.buffer.push_line(line.to_owned());
            }
        }
    }
}

struct MutexFileMakeWriter {
    file: Arc<Mutex<std::fs::File>>,
}

impl<'a> MakeWriter<'a> for MutexFileMakeWriter {
    type Writer = MutexFileWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        MutexFileWriter {
            guard: self.file.lock().expect("audit log file mutex poisoned"),
        }
    }
}

struct MutexFileWriter<'a> {
    guard: std::sync::MutexGuard<'a, std::fs::File>,
}

impl Write for MutexFileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.guard.flush()
    }
}

static TRACING_INIT: Once = Once::new();
static GLOBAL_LOG_BUFFER: OnceLock<Arc<LogBuffer>> = OnceLock::new();

pub fn global_log_buffer() -> Arc<LogBuffer> {
    GLOBAL_LOG_BUFFER
        .get_or_init(|| Arc::new(LogBuffer::new(LoggingConfig::default_buffer_lines())))
        .clone()
}

/// Initializes structured `tracing` logging.
///
/// This function is safe to call multiple times; only the first call installs a
/// global subscriber. Subsequent calls return the global in-memory log buffer.
pub fn init_tracing(config: &LoggingConfig) -> Arc<LogBuffer> {
    init_tracing_inner(config, None)
}

/// Like [`init_tracing`] but also configures the optional AI audit log channel.
pub fn init_tracing_with_config(config: &NovaConfig) -> Arc<LogBuffer> {
    init_tracing_inner(&config.logging, Some(&config.ai))
}

fn init_tracing_inner(logging: &LoggingConfig, ai: Option<&AiConfig>) -> Arc<LogBuffer> {
    let buffer = GLOBAL_LOG_BUFFER
        .get_or_init(|| Arc::new(LogBuffer::new(logging.buffer_lines)))
        .clone();

    TRACING_INIT.call_once(|| {
        let filter = logging.env_filter();

        let base_file = logging
            .file
            .as_ref()
            .and_then(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok()
            })
            .map(|file| Arc::new(Mutex::new(file)));

        let audit_path = ai
            .filter(|ai| ai.enabled && ai.audit_log.enabled)
            .map(|ai| {
                ai.audit_log
                    .path
                    .clone()
                    .unwrap_or_else(|| std::env::temp_dir().join("nova-ai-audit.log"))
            });
        let audit_enabled = audit_path.is_some();
        let audit_file = audit_path
            .as_ref()
            .and_then(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok()
            })
            .map(|file| Arc::new(Mutex::new(file)));
        let audit_open_failed = audit_enabled && audit_file.is_none();

        let mut make_writer = BoxMakeWriter::new(LogBufferMakeWriter {
            buffer: buffer.clone(),
        });
        if logging.stderr {
            // `cargo test` output capture only works for the stdlib's `print!/eprint!`
            // macros. Using `TestWriter` in debug builds keeps unit tests quiet
            // while still providing real-time logs for `cargo run` workflows.
            if cfg!(debug_assertions) {
                make_writer = BoxMakeWriter::new(
                    make_writer.and(tracing_subscriber::fmt::writer::TestWriter::with_stderr),
                );
            } else {
                make_writer = BoxMakeWriter::new(make_writer.and(std::io::stderr));
            }
        }
        if let Some(file) = base_file {
            make_writer = BoxMakeWriter::new(make_writer.and(MutexFileMakeWriter { file }));
        }

        let base_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = if logging.json {
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(make_writer)
                .with_ansi(false);

            if audit_enabled {
                layer
                    .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                        meta.target() != AI_AUDIT_TARGET
                    }))
                    .boxed()
            } else {
                layer.boxed()
            }
        } else {
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(make_writer)
                .with_ansi(false);

            if audit_enabled {
                layer
                    .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                        meta.target() != AI_AUDIT_TARGET
                    }))
                    .boxed()
            } else {
                layer.boxed()
            }
        };

        let audit_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> =
            if let Some(file) = audit_file {
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(MutexFileMakeWriter { file })
                    .with_ansi(false)
                    .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                        meta.target() == AI_AUDIT_TARGET
                    }))
                    .boxed()
            } else {
                tracing_subscriber::layer::Identity::new().boxed()
            };

        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(base_layer)
            .with(audit_layer);
        if tracing::subscriber::set_global_default(subscriber).is_ok() && audit_open_failed {
            if let Some(path) = audit_path {
                tracing::warn!(
                    target: "nova.config",
                    path = %path.display(),
                    "failed to open AI audit log file; audit events will be dropped"
                );
            } else {
                tracing::warn!(
                    target: "nova.config",
                    "failed to open AI audit log file; audit events will be dropped"
                );
            }
        }
    });

    buffer
}

#[cfg(test)]
mod toml_tests {
    use super::*;

    #[test]
    fn logging_level_parses_simple_levels() {
        let mut logging = LoggingConfig::default();
        logging.level = "DEBUG".to_owned();

        let filter = logging.config_env_filter();
        let buffer = Arc::new(LogBuffer::new(64));
        let subscriber = tracing_subscriber::registry().with(filter).with(
            tracing_subscriber::fmt::layer()
                .with_writer(LogBufferMakeWriter {
                    buffer: buffer.clone(),
                })
                .with_ansi(false),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!("not visible");
            tracing::debug!("visible");
        });

        let text = buffer.last_lines(64).join("\n");
        assert!(!text.contains("not visible"), "{text}");
        assert!(text.contains("visible"), "{text}");
    }

    #[test]
    fn logging_level_parses_env_filter_directives() {
        let mut logging = LoggingConfig::default();
        logging.level = "warn,nova_config=trace".to_owned();

        let filter = logging.config_env_filter();
        let buffer = Arc::new(LogBuffer::new(64));
        let subscriber = tracing_subscriber::registry().with(filter).with(
            tracing_subscriber::fmt::layer()
                .with_writer(LogBufferMakeWriter {
                    buffer: buffer.clone(),
                })
                .with_ansi(false),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "other_target", "not visible");
            tracing::warn!(target: "other_target", "visible warn");
            tracing::trace!(target: "nova_config", "visible trace");
        });

        let text = buffer.last_lines(64).join("\n");
        assert!(!text.contains("not visible"), "{text}");
        assert!(text.contains("visible warn"), "{text}");
        assert!(text.contains("visible trace"), "{text}");
    }

    #[test]
    fn audit_target_is_excluded_from_base_log_buffer_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audit_path = dir.path().join("audit.log");

        let mut config = NovaConfig::default();
        config.ai.enabled = true;
        config.ai.audit_log.enabled = true;
        config.ai.audit_log.path = Some(audit_path);

        let buffer = init_tracing_with_config(&config);

        tracing::info!("visible");
        tracing::info!(target: AI_AUDIT_TARGET, "should not be in base");

        let text = buffer.last_lines(256).join("\n");
        assert!(text.contains("visible"), "{text}");
        assert!(!text.contains("should not be in base"), "{text}");
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub enable_indexing: bool,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self {
            enable_indexing: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_config_features_and_timeouts_roundtrip_toml() {
        let mut config = AiConfig::default();
        config.enabled = true;
        config.features.completion_ranking = true;
        config.features.semantic_search = true;
        config.features.multi_token_completion = true;
        config.timeouts.completion_ranking_ms = 123;
        config.timeouts.multi_token_completion_ms = 456;

        let text = toml::to_string(&config).expect("serialize AiConfig");
        let decoded: AiConfig = toml::from_str(&text).expect("deserialize AiConfig");

        assert!(decoded.enabled);
        assert!(decoded.features.completion_ranking);
        assert!(decoded.features.semantic_search);
        assert!(decoded.features.multi_token_completion);
        assert_eq!(decoded.timeouts.completion_ranking_ms, 123);
        assert_eq!(decoded.timeouts.multi_token_completion_ms, 456);
    }

    #[test]
    fn toml_without_extensions_table_uses_defaults() {
        let config: NovaConfig = toml::from_str("").expect("config should parse");

        assert!(config.extensions.enabled);
        assert!(config.extensions.wasm_paths.is_empty());
        assert!(config.extensions.allow.is_none());
        assert!(config.extensions.deny.is_empty());
        assert!(config.extensions.wasm_memory_limit_bytes.is_none());
        assert!(config.extensions.wasm_timeout_ms.is_none());
    }

    #[test]
    fn toml_extensions_table_parses() {
        let text = r#"
[extensions]
enabled = false
wasm_paths = ["./extensions", "extensions/custom.wasm"]
allow = ["example.one", "example.two"]
deny = ["example.bad"]
wasm_memory_limit_bytes = 134217728
wasm_timeout_ms = 2500
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");

        assert!(!config.extensions.enabled);
        assert_eq!(
            config.extensions.wasm_paths,
            vec![
                PathBuf::from("./extensions"),
                PathBuf::from("extensions/custom.wasm")
            ]
        );
        assert_eq!(
            config.extensions.allow,
            Some(vec!["example.one".to_owned(), "example.two".to_owned()])
        );
        assert_eq!(config.extensions.deny, vec!["example.bad".to_owned()]);
        assert_eq!(config.extensions.wasm_memory_limit_bytes, Some(134_217_728));
        assert_eq!(config.extensions.wasm_timeout_ms, Some(2_500));
    }

    #[test]
    fn toml_extensions_table_partial_uses_defaults() {
        let text = r#"
[extensions]
wasm_paths = ["extensions"]
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");

        assert!(config.extensions.enabled);
        assert_eq!(
            config.extensions.wasm_paths,
            vec![PathBuf::from("extensions")]
        );
        assert!(config.extensions.allow.is_none());
        assert!(config.extensions.deny.is_empty());
        assert!(config.extensions.wasm_memory_limit_bytes.is_none());
        assert!(config.extensions.wasm_timeout_ms.is_none());
    }
}
