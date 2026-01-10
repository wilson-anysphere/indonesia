use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GeneratedSourcesConfig {
    /// Whether generated sources should be indexed and participate in resolution.
    #[serde(default = "default_generated_sources_enabled")]
    pub enabled: bool,
    /// Additional generated roots (relative to project root unless absolute).
    #[serde(default)]
    pub additional_roots: Vec<PathBuf>,
    /// If set, replaces default discovery entirely.
    #[serde(default)]
    pub override_roots: Option<Vec<PathBuf>>,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NovaConfig {
    #[serde(default)]
    pub generated_sources: GeneratedSourcesConfig,

    /// Offline / local LLM configuration (Ollama, vLLM, etc).
    #[serde(default)]
    pub ai: AiConfig,
}

impl Default for NovaConfig {
    fn default() -> Self {
        Self {
            generated_sources: GeneratedSourcesConfig::default(),
            ai: AiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    #[serde(default)]
    pub provider: AiProviderConfig,
    #[serde(default)]
    pub privacy: AiPrivacyConfig,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: AiProviderConfig::default(),
            privacy: AiPrivacyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProviderKind {
    /// Ollama HTTP API (e.g. http://localhost:11434)
    Ollama,
    /// OpenAI-compatible endpoints (e.g. vLLM, llama.cpp server, etc.)
    OpenAiCompatible,
}

impl Default for AiProviderKind {
    fn default() -> Self {
        Self::Ollama
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiProviderConfig {
    /// Which backend implementation to use.
    #[serde(default)]
    pub kind: AiProviderKind,

    /// Base URL for the provider (e.g. http://localhost:11434, http://localhost:8000).
    pub url: Url,

    /// Default model name.
    pub model: String,

    /// Default max tokens for responses.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// Per-request timeout.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Maximum number of concurrent requests Nova will make to the backend.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
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
            url: Url::parse("http://localhost:11434").expect("valid default url"),
            model: "llama3".to_string(),
            max_tokens: default_max_tokens(),
            timeout_ms: default_timeout_ms(),
            concurrency: default_concurrency(),
        }
    }
}

impl AiProviderConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiPrivacyConfig {
    /// If true, Nova will not use any cloud providers. This is the recommended
    /// setting for privacy-sensitive environments.
    #[serde(default = "default_local_only")]
    pub local_only: bool,

    /// If unset, defaults to:
    /// - `false` when `local_only = true`
    /// - `true` when `local_only = false` (cloud mode)
    ///
    /// This matches Nova's privacy philosophy: anonymize when sending code to a
    /// third-party, but avoid needless transformations when everything stays
    /// local.
    #[serde(default)]
    pub anonymize: Option<bool>,

    /// Glob patterns for file paths that must never be sent to the LLM.
    #[serde(default)]
    pub excluded_paths: Vec<String>,

    /// Regex patterns to redact from any text that will be sent to the LLM.
    #[serde(default)]
    pub redact_patterns: Vec<String>,
}

fn default_local_only() -> bool {
    true
}

impl Default for AiPrivacyConfig {
    fn default() -> Self {
        Self {
            local_only: default_local_only(),
            anonymize: None,
            excluded_paths: Vec::new(),
            redact_patterns: Vec::new(),
        }
    }
}

impl AiPrivacyConfig {
    /// Resolve the effective anonymization flag based on privacy defaults.
    pub fn effective_anonymize(&self) -> bool {
        match self.anonymize {
            Some(value) => value,
            None => !self.local_only,
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
