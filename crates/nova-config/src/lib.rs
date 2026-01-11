use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use thiserror::Error;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use url::Url;

/// Tracing target used for AI audit events (prompts / model output).
pub const AI_AUDIT_TARGET: &str = "nova.ai.audit";

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

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct JdkConfig {
    /// Optional override for the JDK installation to use.
    ///
    /// When set, JDK discovery will use this path instead of searching `JAVA_HOME`
    /// or `java` on `PATH`.
    #[serde(default, alias = "jdk_home")]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NovaConfig {
    #[serde(default)]
    pub generated_sources: GeneratedSourcesConfig,

    /// Workspace-level JDK override configuration.
    #[serde(default)]
    pub jdk: JdkConfig,

    /// Global logging settings for Nova crates.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Offline / local LLM configuration (Ollama, vLLM, etc).
    #[serde(default)]
    pub ai: AiConfig,
}

impl Default for NovaConfig {
    fn default() -> Self {
        Self {
            generated_sources: GeneratedSourcesConfig::default(),
            jdk: JdkConfig::default(),
            logging: LoggingConfig::default(),
            ai: AiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Logging level for all Nova crates.
    #[serde(default = "LoggingConfig::default_level")]
    pub level: String,

    /// Emit logs in JSON format.
    #[serde(default)]
    pub json: bool,

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

    fn default_buffer_lines() -> usize {
        2_000
    }

    pub fn level_filter(&self) -> tracing_subscriber::filter::LevelFilter {
        match self.level.to_ascii_lowercase().as_str() {
            "trace" => tracing_subscriber::filter::LevelFilter::TRACE,
            "debug" => tracing_subscriber::filter::LevelFilter::DEBUG,
            "warn" | "warning" => tracing_subscriber::filter::LevelFilter::WARN,
            "error" => tracing_subscriber::filter::LevelFilter::ERROR,
            _ => tracing_subscriber::filter::LevelFilter::INFO,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: Self::default_level(),
            json: false,
            include_backtrace: false,
            buffer_lines: Self::default_buffer_lines(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for AiFeaturesConfig {
    fn default() -> Self {
        Self {
            completion_ranking: false,
            semantic_search: false,
            multi_token_completion: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiTimeoutsConfig {
    /// Timeout for completion ranking requests.
    #[serde(default = "default_completion_ranking_timeout_ms")]
    pub completion_ranking_ms: u64,

    /// Timeout for multi-token completion requests.
    #[serde(default = "default_multi_token_completion_timeout_ms")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    #[serde(default)]
    pub provider: AiProviderConfig,
    #[serde(default)]
    pub privacy: AiPrivacyConfig,

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
    pub cache_max_entries: usize,

    /// Cache TTL in seconds.
    #[serde(default = "default_ai_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: AiProviderConfig::default(),
            privacy: AiPrivacyConfig::default(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub path: Option<PathBuf>,
}

impl Default for AuditLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
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

    pub fn jdk_config(&self) -> nova_core::JdkConfig {
        nova_core::JdkConfig {
            home: self.jdk.home.clone(),
        }
    }
}

/// Load Nova configuration for a workspace root.
///
/// Discovery order (first match wins):
/// 1) `NOVA_CONFIG_PATH` env var (if set)
/// 2) Walk up from `root` (or `root.parent()` when `root` is a file) looking for:
///    - `.nova/config.toml`
///    - `nova.toml`
/// 3) fallback `NovaConfig::default()`
pub fn load_for_workspace(root: impl AsRef<Path>) -> Result<NovaConfig, ConfigError> {
    if let Some(path) = std::env::var_os("NOVA_CONFIG_PATH") {
        return NovaConfig::load_from_path(PathBuf::from(path));
    }

    let root = root.as_ref();
    let start = if root.is_file() {
        root.parent().unwrap_or(root)
    } else {
        root
    };

    for dir in start.ancestors() {
        let candidates = [dir.join(".nova").join("config.toml"), dir.join("nova.toml")];
        for candidate in candidates {
            match NovaConfig::load_from_path(&candidate) {
                Ok(config) => return Ok(config),
                Err(ConfigError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    Ok(NovaConfig::default())
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
        .get_or_init(|| Arc::new(LogBuffer::new(2_000)))
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
        let filter =
            tracing_subscriber::EnvFilter::default().add_directive(logging.level_filter().into());

        let audit_file = ai
            .filter(|ai| ai.enabled && ai.audit_log.enabled)
            .and_then(|ai| {
                let path = ai
                    .audit_log
                    .path
                    .clone()
                    .unwrap_or_else(|| std::env::temp_dir().join("nova-ai-audit.log"));

                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok()
            })
            .map(|file| Arc::new(Mutex::new(file)));
        let audit_enabled = audit_file.is_some();

        let base_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = if logging.json {
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(LogBufferMakeWriter {
                    buffer: buffer.clone(),
                })
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
                .with_writer(LogBufferMakeWriter {
                    buffer: buffer.clone(),
                })
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
        let _ = tracing::subscriber::set_global_default(subscriber);
    });

    buffer
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
}
