use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use parking_lot::ReentrantMutex;
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct JdkToolchainConfig {
    /// Java feature release associated with this toolchain (e.g. 8, 17, 21).
    pub release: u16,

    /// Root directory of the JDK installation to use for this release.
    #[schemars(with = "String")]
    pub home: PathBuf,
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

    /// Default Java feature release used for `--release`-style API selection when callers don't
    /// provide one explicitly.
    #[serde(default, alias = "target_release")]
    pub release: Option<u16>,

    /// Optional per-`--release` toolchains.
    #[serde(default)]
    pub toolchains: Vec<JdkToolchainConfig>,
}

/// Controls whether Nova will invoke external build tools (Maven/Gradle) to extract build metadata
/// (compile classpaths, source roots, language level, etc).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BuildIntegrationMode {
    /// Never invoke external build tools. Nova relies on heuristic project loading (`nova-project`)
    /// and any user-specified overrides.
    Off,
    /// Use cached build metadata if available, but do not invoke build tools on cache misses.
    ///
    /// This is the default to avoid surprising slow startup costs or build tool downloads.
    Auto,
    /// Invoke build tools on workspace load (and build file changes) to obtain accurate metadata.
    On,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[schemars(deny_unknown_fields)]
pub struct BuildIntegrationToolConfig {
    /// Deprecated legacy toggle for this specific build tool.
    ///
    /// When set to `false`, this tool is treated as `mode = "off"` regardless of global mode.
    ///
    /// Prefer `mode` for full control.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Optional override for this build tool. When unset, `build.mode` is used.
    #[serde(default)]
    pub mode: Option<BuildIntegrationMode>,

    /// Optional timeout override for this build tool (in milliseconds).
    ///
    /// When unset, `build.timeout_ms` is used.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct BuildIntegrationConfig {
    /// Deprecated legacy toggle for build tool integration.
    ///
    /// When set:
    /// - `true` is treated as `mode = "on"`
    /// - `false` is treated as `mode = "off"`
    ///
    /// Prefer `mode` for full control including the cache-only default (`auto`).
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Default build integration behavior for Maven/Gradle.
    #[serde(default = "BuildIntegrationConfig::default_mode")]
    pub mode: BuildIntegrationMode,

    /// Timeout applied to build-tool metadata extraction commands (in milliseconds).
    #[serde(default = "BuildIntegrationConfig::default_timeout_ms")]
    #[schemars(range(min = 1))]
    pub timeout_ms: u64,

    /// Optional Maven-specific overrides.
    #[serde(default)]
    pub maven: BuildIntegrationToolConfig,

    /// Optional Gradle-specific overrides.
    #[serde(default)]
    pub gradle: BuildIntegrationToolConfig,
}

impl BuildIntegrationConfig {
    fn default_mode() -> BuildIntegrationMode {
        BuildIntegrationMode::Auto
    }

    fn default_timeout_ms() -> u64 {
        // Metadata extraction should be bounded during workspace load. Callers can increase this
        // if their build tool needs longer.
        120_000
    }

    pub fn maven_mode(&self) -> BuildIntegrationMode {
        if self.maven.enabled == Some(false) {
            return BuildIntegrationMode::Off;
        }
        self.maven.mode.unwrap_or(self.base_mode())
    }

    pub fn gradle_mode(&self) -> BuildIntegrationMode {
        if self.gradle.enabled == Some(false) {
            return BuildIntegrationMode::Off;
        }
        self.gradle.mode.unwrap_or(self.base_mode())
    }

    pub fn maven_timeout(&self) -> Duration {
        Duration::from_millis(self.maven.timeout_ms.unwrap_or(self.timeout_ms).max(1))
    }

    pub fn gradle_timeout(&self) -> Duration {
        Duration::from_millis(self.gradle.timeout_ms.unwrap_or(self.timeout_ms).max(1))
    }
}

impl Default for BuildIntegrationConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            mode: Self::default_mode(),
            timeout_ms: Self::default_timeout_ms(),
            maven: BuildIntegrationToolConfig::default(),
            gradle: BuildIntegrationToolConfig::default(),
        }
    }
}

impl BuildIntegrationConfig {
    fn base_mode(&self) -> BuildIntegrationMode {
        match self.enabled {
            Some(true) => BuildIntegrationMode::On,
            Some(false) => BuildIntegrationMode::Off,
            None => self.mode,
        }
    }

    /// Effective timeout for build tool invocations.
    ///
    /// This uses `max(1)` so even misconfigured values remain time-bounded.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(1))
    }

    /// Returns `true` if Maven integration is enabled in `mode = "on"`.
    pub fn maven_enabled(&self) -> bool {
        self.maven_mode() == BuildIntegrationMode::On
    }

    /// Returns `true` if Gradle integration is enabled in `mode = "on"`.
    pub fn gradle_enabled(&self) -> bool {
        self.gradle_mode() == BuildIntegrationMode::On
    }
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

impl ExtensionsConfig {
    /// Returns `true` if an extension with the given id is permitted to load
    /// according to this config.
    ///
    /// Matching follows the extension ADR semantics:
    /// - `enabled = false` disables all extensions
    /// - `allow = Some([...])` restricts to extension ids matching *any* pattern
    /// - `deny = [...]` always blocks extension ids matching *any* pattern (deny overrides allow)
    ///
    /// Pattern syntax:
    /// - `*` matches any substring (including empty)
    /// - if the pattern contains no `*`, it is treated as an exact match
    /// - matching is case-sensitive
    pub fn is_extension_allowed(&self, id: &str) -> bool {
        if !self.enabled {
            return false;
        }

        if self
            .deny
            .iter()
            .any(|pattern| matches_simple_glob(pattern, id))
        {
            return false;
        }

        match &self.allow {
            Some(patterns) => patterns
                .iter()
                .any(|pattern| matches_simple_glob(pattern, id)),
            None => true,
        }
    }

    fn normalize(&mut self) {
        fn normalize_id_list(list: &mut Vec<String>) {
            let mut out = BTreeSet::<String>::new();
            for raw in list.drain(..) {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                out.insert(trimmed.to_string());
            }
            *list = out.into_iter().collect();
        }

        self.wasm_paths.retain(|p| !p.as_os_str().is_empty());

        if let Some(allow) = &mut self.allow {
            normalize_id_list(allow);
        }
        normalize_id_list(&mut self.deny);

        // Deny overrides allow, but remove exact duplicates to keep the config deterministic.
        if let Some(allow) = &mut self.allow {
            if !self.deny.is_empty() && !allow.is_empty() {
                let deny: BTreeSet<_> = self.deny.iter().cloned().collect();
                allow.retain(|pattern| !deny.contains(pattern));
            }
        }

        if let Some(timeout_ms) = self.wasm_timeout_ms.as_mut() {
            if *timeout_ms == 0 {
                *timeout_ms = 1;
            }
        }

        if let Some(limit) = self.wasm_memory_limit_bytes.as_mut() {
            // Any valid WASM module exporting linear memory requires at least 1 page (64KiB).
            const MIN_MEMORY_BYTES: u64 = 64 * 1024;
            if *limit < MIN_MEMORY_BYTES {
                *limit = MIN_MEMORY_BYTES;
            }
        }
    }
}

fn matches_simple_glob(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }

    let pattern = pattern.as_bytes();
    let text = text.as_bytes();

    let mut p_idx = 0usize;
    let mut t_idx = 0usize;
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;

    while t_idx < text.len() {
        if p_idx < pattern.len() && pattern[p_idx] == text[t_idx] {
            p_idx += 1;
            t_idx += 1;
        } else if p_idx < pattern.len() && pattern[p_idx] == b'*' {
            star_idx = Some(p_idx);
            match_idx = t_idx;
            p_idx += 1;
        } else if let Some(star) = star_idx {
            p_idx = star + 1;
            match_idx += 1;
            t_idx = match_idx;
        } else {
            return false;
        }
    }

    while p_idx < pattern.len() && pattern[p_idx] == b'*' {
        p_idx += 1;
    }

    p_idx == pattern.len()
}

/// A byte size which supports both raw byte counts and human-friendly suffixes.
///
/// This is used for config values where TOML integer literals would be unwieldy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(transparent)]
pub struct ByteSize(pub u64);

impl Serialize for ByteSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Bytes(u64),
            Human(String),
        }

        let repr = Repr::deserialize(deserializer)?;
        match repr {
            Repr::Bytes(value) => Ok(ByteSize(value)),
            Repr::Human(value) => nova_memory::parse_byte_size(&value)
                .map(ByteSize)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// Optional configuration for Nova's in-process memory budgets (`nova-memory`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Override the total memory budget for Nova's caches (in bytes).
    #[serde(default)]
    pub total_bytes: Option<ByteSize>,

    /// Override the query cache category budget (in bytes).
    #[serde(default)]
    pub query_cache_bytes: Option<ByteSize>,

    /// Override the syntax tree category budget (in bytes).
    #[serde(default)]
    pub syntax_trees_bytes: Option<ByteSize>,

    /// Override the indexes category budget (in bytes).
    #[serde(default)]
    pub indexes_bytes: Option<ByteSize>,

    /// Override the type info category budget (in bytes).
    #[serde(default)]
    pub type_info_bytes: Option<ByteSize>,

    /// Override the "other" category budget (in bytes).
    #[serde(default)]
    pub other_bytes: Option<ByteSize>,
}

impl MemoryConfig {
    pub fn memory_budget_overrides(&self) -> nova_memory::MemoryBudgetOverrides {
        nova_memory::MemoryBudgetOverrides {
            total: self.total_bytes.map(|value| value.0),
            categories: nova_memory::MemoryBreakdownOverrides {
                query_cache: self.query_cache_bytes.map(|value| value.0),
                syntax_trees: self.syntax_trees_bytes.map(|value| value.0),
                indexes: self.indexes_bytes.map(|value| value.0),
                type_info: self.type_info_bytes.map(|value| value.0),
                other: self.other_bytes.map(|value| value.0),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Top-level Nova configuration loaded from TOML.
///
/// Build tool integration can be configured via the `[build]` table:
/// ```toml
/// [build]
/// # Default is `mode = "auto"` (use cached build metadata only; do not run build tools on cache misses).
/// mode = "on" # "off" | "auto" | "on"
/// timeout_ms = 120000
///
/// [build.maven]
/// mode = "on"
///
/// [build.gradle]
/// mode = "on"
/// ```
///
/// Note: the legacy alias `[build_integration]` is also accepted.
///
/// Legacy compatibility:
/// - `build.enabled = true|false` is treated as `build.mode = "on"|"off"`.
/// - `build.maven.enabled = false` / `build.gradle.enabled = false` forces that tool `mode = "off"`.
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

    /// Controls build tool (Maven/Gradle) invocation for workspace metadata extraction.
    #[serde(default, alias = "build_integration")]
    pub build: BuildIntegrationConfig,

    /// Workspace extensions (WASM bundles) configuration.
    #[serde(default)]
    pub extensions: ExtensionsConfig,

    /// Global logging settings for Nova crates.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Optional memory budgeting configuration (`nova-memory`).
    #[serde(default)]
    pub memory: MemoryConfig,

    /// AI configuration (provider selection, privacy controls, embeddings, etc).
    #[serde(default)]
    pub ai: AiConfig,
}

#[allow(clippy::derivable_impls)]
impl Default for NovaConfig {
    fn default() -> Self {
        Self {
            generated_sources: GeneratedSourcesConfig::default(),
            jdk: JdkConfig::default(),
            build: BuildIntegrationConfig::default(),
            extensions: ExtensionsConfig::default(),
            logging: LoggingConfig::default(),
            memory: MemoryConfig::default(),
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
    #[schemars(range(min = 1))]
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

    /// Enables the LLM-backed "Explain this error" action (`nova/ai/explainError`).
    ///
    /// Defaults to `true` so existing configs that set `ai.enabled=true` but do not mention
    /// `ai.features.explain_errors` continue to offer explain-error functionality.
    #[serde(default = "default_ai_action_feature_enabled")]
    pub explain_errors: bool,

    /// Enables LLM-backed code-editing actions (patch-based edits), such as:
    ///
    /// - "Generate method body with AI" (`nova/ai/generateMethodBody`)
    /// - "Generate tests with AI" (`nova/ai/generateTests`)
    ///
    /// Defaults to `true` so existing configs that set `ai.enabled=true` but do not mention
    /// `ai.features.code_actions` continue to offer code-editing actions.
    #[serde(default = "default_ai_action_feature_enabled")]
    pub code_actions: bool,

    /// Enables LLM-backed code review actions (for example: `nova ai review` when available).
    ///
    /// Defaults to `true` so existing configs that set `ai.enabled=true` but do not mention
    /// `ai.features.code_review` continue to allow code review actions.
    #[serde(default = "default_ai_action_feature_enabled")]
    pub code_review: bool,

    /// Maximum number of diff characters included in AI code review prompts.
    ///
    /// Large diffs can exceed LLM provider limits and increase latency/cost.
    /// When the diff exceeds this limit, Nova keeps the beginning and end of the
    /// diff and inserts a truncation marker indicating how much was omitted.
    #[serde(default = "default_code_review_max_diff_chars")]
    #[schemars(range(min = 1))]
    pub code_review_max_diff_chars: usize,
}

fn default_code_review_max_diff_chars() -> usize {
    50_000
}

#[allow(clippy::derivable_impls)]
impl Default for AiFeaturesConfig {
    fn default() -> Self {
        Self {
            completion_ranking: false,
            semantic_search: false,
            multi_token_completion: false,
            explain_errors: default_ai_action_feature_enabled(),
            code_actions: default_ai_action_feature_enabled(),
            code_review: default_ai_action_feature_enabled(),
            code_review_max_diff_chars: default_code_review_max_diff_chars(),
        }
    }
}

fn default_ai_action_feature_enabled() -> bool {
    true
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

#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiConfig {
    /// Provider/backend configuration (kind, base URL, model, timeouts, etc).
    #[serde(default)]
    pub provider: AiProviderConfig,
    /// Privacy controls and redaction behavior for AI requests.
    #[serde(default)]
    pub privacy: AiPrivacyConfig,

    /// Embeddings configuration used for semantic search and context building.
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

    /// Optional audit log configuration for AI prompts/model output.
    #[serde(default)]
    pub audit_log: AuditLogConfig,

    /// AI feature toggles (local augmentation + LLM-backed actions).
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

impl fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiConfig")
            .field("provider", &self.provider)
            .field("privacy", &self.privacy)
            .field("embeddings", &self.embeddings)
            .field("enabled", &self.enabled)
            .field("api_key_present", &self.api_key.is_some())
            .field("audit_log", &self.audit_log)
            .field("features", &self.features)
            .field("timeouts", &self.timeouts)
            .field("cache_enabled", &self.cache_enabled)
            .field("cache_max_entries", &self.cache_max_entries)
            .field("cache_ttl_secs", &self.cache_ttl_secs)
            .finish()
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AiEmbeddingsBackend {
    /// Deterministic, fully-local embeddings based on the hashing trick.
    ///
    /// This is the default to keep offline tests stable and to avoid requiring
    /// network access or model downloads.
    Hash,
    /// Provider-backed embeddings via the configured AI provider (`ai.provider.*`).
    Provider,
    /// In-process local neural embedding models.
    ///
    /// This backend is supported when `nova-ai` is built with an appropriate
    /// local-embeddings feature (e.g. `embeddings-local`).
    Local,
}

#[allow(clippy::derivable_impls)]
impl Default for AiEmbeddingsBackend {
    fn default() -> Self {
        Self::Hash
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiEmbeddingsConfig {
    /// Enable embeddings for semantic search and context building.
    #[serde(default)]
    pub enabled: bool,

    /// Which embeddings backend to use.
    #[serde(default)]
    pub backend: AiEmbeddingsBackend,

    /// Optional embedding model override when using provider-backed embeddings.
    ///
    /// When unset, Nova reuses `ai.provider.model`.
    ///
    /// Note: for Azure OpenAI (`ai.provider.kind = "azure_open_ai"`), Azure uses **deployments**
    /// instead of raw model names. In that case, `ai.embeddings.model` is treated as an embeddings
    /// deployment override (falling back to `ai.provider.azure_deployment` when unset).
    #[serde(default)]
    pub model: Option<String>,

    /// Optional timeout override (in milliseconds) when using provider-backed embeddings.
    ///
    /// When unset, Nova reuses `ai.provider.timeout_ms`.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub timeout_ms: Option<u64>,
    /// Local embedding model identifier used when `backend = "local"`.
    ///
    /// The set of supported values depends on the selected `nova-ai` local embedding
    /// implementation. When using `fastembed`, this corresponds to the built-in model IDs (e.g.
    /// `"all-MiniLM-L6-v2"` or `"bge-small-en-v1.5"`).
    #[serde(default = "default_embeddings_local_model")]
    pub local_model: String,

    /// Directory containing embedding model files / cache.
    #[serde(default = "default_embeddings_model_dir")]
    #[schemars(with = "String")]
    pub model_dir: PathBuf,

    /// Maximum batch size for embedding requests.
    #[serde(default = "default_embeddings_batch_size")]
    #[schemars(range(min = 1))]
    pub batch_size: usize,

    /// Soft memory budget (in bytes) for embedding models / caches.
    ///
    /// Accepts either:
    /// - an integer byte count (e.g. `536870912`)
    /// - a human-friendly string (e.g. `"512MiB"`, `"2G"`)
    #[serde(default = "default_embeddings_max_memory_bytes")]
    #[schemars(schema_with = "crate::schema::byte_size_schema", range(min = 1))]
    pub max_memory_bytes: ByteSize,
}

fn default_embeddings_model_dir() -> PathBuf {
    PathBuf::from(".nova/models/embeddings")
}

fn default_embeddings_local_model() -> String {
    "all-MiniLM-L6-v2".to_string()
}

fn default_embeddings_batch_size() -> usize {
    32
}

fn default_embeddings_max_memory_bytes() -> ByteSize {
    ByteSize(512 * 1024 * 1024)
}

impl Default for AiEmbeddingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: AiEmbeddingsBackend::default(),
            model: None,
            timeout_ms: None,
            local_model: default_embeddings_local_model(),
            model_dir: default_embeddings_model_dir(),
            batch_size: default_embeddings_batch_size(),
            max_memory_bytes: default_embeddings_max_memory_bytes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AuditLogConfig {
    /// Enable writing AI audit events (prompts / model output) to a dedicated log file.
    #[serde(default)]
    pub enabled: bool,

    /// Optional path for the audit log file.
    ///
    /// If unset, defaults to `$TMPDIR/nova-ai-audit.log` at runtime.
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
    /// Requests are sent by POSTing to `ai.provider.url` as configured (it is treated as a full
    /// endpoint, not a base URL).
    ///
    /// Optional auth: if `ai.api_key` is set, Nova sends `Authorization: Bearer <key>`.
    ///
    /// Non-streaming request body:
    /// `{ "model": "...", "prompt": "...", "max_tokens": 123, "temperature": 0.2 }`
    ///
    /// Non-streaming response body:
    /// `{ "completion": "..." }`
    ///
    /// Streaming (`chat_stream()`) request:
    /// - Header: `Accept: text/event-stream`
    /// - JSON body: `{ "stream": true, "model": "...", "prompt": "...", "max_tokens": 123, "temperature": 0.2 }`
    ///
    /// Streaming response (optional): providers may reply using Server-Sent Events (SSE) with
    /// `Content-Type: text/event-stream`.
    ///
    /// Each chunk is emitted as a separate SSE event (terminated by a blank line), e.g.:
    /// `data: {"completion":"..."}\n\n`
    ///
    /// The stream terminates with:
    /// `data: [DONE]\n\n`
    ///
    /// Fallback: if the response is not SSE, Nova reads a single JSON response body
    /// (`{ "completion": "..." }`) and yields it as one chunk.
    Http,
}

#[allow(clippy::derivable_impls)]
impl Default for AiProviderKind {
    fn default() -> Self {
        Self::Ollama
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiProviderConfig {
    /// Which backend implementation to use.
    #[serde(default)]
    pub kind: AiProviderKind,

    /// Base URL for the provider.
    ///
    /// Examples:
    /// - Ollama: `http://localhost:11434`
    /// - OpenAI-compatible (vLLM, llama.cpp server): `http://localhost:8000/v1`
    /// - OpenAI: `https://api.openai.com/v1`
    /// - Anthropic: `https://api.anthropic.com`
    /// - Gemini: `https://generativelanguage.googleapis.com`
    /// - Azure OpenAI: `https://{resource}.openai.azure.com`
    /// - HTTP (kind = "http"): `http://localhost:1234/complete`
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

    /// Optional default sampling temperature applied to chat requests.
    ///
    /// When unset, Nova omits the `temperature` field entirely and the provider's default is used
    /// (often `1.0`).
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub temperature: Option<f32>,

    /// Per-request timeout (in milliseconds).
    ///
    /// For non-streaming requests (`chat`), this bounds the total request duration.
    ///
    /// For streaming requests (`chat_stream`), this is treated as an *idle timeout* while reading
    /// the stream (maximum time between chunks) and as a timeout for establishing the response
    /// (send + headers). It does **not** cap the total stream duration as long as chunks keep
    /// arriving.
    #[serde(default = "default_timeout_ms")]
    #[schemars(range(min = 1))]
    pub timeout_ms: u64,

    /// Maximum number of retries for failed LLM requests.
    ///
    /// Set to `0` to disable retries entirely (useful for latency-sensitive environments).
    #[serde(default = "default_retry_max_retries")]
    #[schemars(range(min = 0))]
    pub retry_max_retries: usize,

    /// Initial exponential backoff delay between retries (in milliseconds).
    #[serde(default = "default_retry_initial_backoff_ms")]
    #[schemars(range(min = 1))]
    pub retry_initial_backoff_ms: u64,

    /// Maximum exponential backoff delay between retries (in milliseconds).
    #[serde(default = "default_retry_max_backoff_ms")]
    #[schemars(range(min = 1))]
    pub retry_max_backoff_ms: u64,

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

impl fmt::Debug for AiProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiProviderConfig")
            .field("kind", &self.kind)
            .field("url", &sanitize_url_for_debug(&self.url))
            .field("model", &self.model)
            .field("azure_deployment", &self.azure_deployment)
            .field("azure_api_version", &self.azure_api_version)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("timeout_ms", &self.timeout_ms)
            .field("retry_max_retries", &self.retry_max_retries)
            .field("retry_initial_backoff_ms", &self.retry_initial_backoff_ms)
            .field("retry_max_backoff_ms", &self.retry_max_backoff_ms)
            .field("concurrency", &self.concurrency)
            .field("in_process_llama", &self.in_process_llama)
            .finish()
    }
}

pub(crate) fn sanitize_url_for_debug(url: &Url) -> String {
    const REDACTION: &str = "<redacted>";
    let mut out = String::new();

    // Use a stable, human-readable redacted representation rather than `Url::to_string()`, which
    // percent-encodes `<redacted>` in the userinfo/query components.
    out.push_str(url.scheme());
    out.push_str("://");

    if !url.username().is_empty() || url.password().is_some() {
        out.push_str(REDACTION);
        out.push('@');
    }

    match url.host_str() {
        Some(host) => out.push_str(host),
        None => out.push_str("<unknown-host>"),
    }

    if let Some(port) = url.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }

    out.push_str(url.path());

    if url.query().is_some() {
        out.push('?');
        let mut first = true;
        for (k, _v) in url.query_pairs() {
            if !first {
                out.push('&');
            }
            first = false;
            out.push_str(&k);
            out.push('=');
            // Be conservative: query parameters often encode credentials (including unknown keys).
            // Always redact values in debug output so configs can be safely included in bug reports.
            out.push_str(REDACTION);
        }
    }

    if url.fragment().is_some() {
        out.push('#');
        // URL fragments can also contain secrets (e.g. single-page-app tokens). Redact.
        out.push_str(REDACTION);
    }

    out
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

fn default_retry_max_retries() -> usize {
    2
}

fn default_retry_initial_backoff_ms() -> u64 {
    200
}

fn default_retry_max_backoff_ms() -> u64 {
    2_000
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
            temperature: None,
            timeout_ms: default_timeout_ms(),
            retry_max_retries: default_retry_max_retries(),
            retry_initial_backoff_ms: default_retry_initial_backoff_ms(),
            retry_max_backoff_ms: default_retry_max_backoff_ms(),
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
    #[schemars(range(min = 1, max = 8192))]
    pub context_size: usize,

    /// Number of CPU threads to use (`n_threads`).
    ///
    /// If unset or set to `0`, the backend will use the available parallelism.
    #[serde(default)]
    pub threads: Option<usize>,

    /// Sampling temperature.
    #[serde(default = "default_in_process_llama_temperature")]
    #[schemars(range(min = 0.0))]
    pub temperature: f32,

    /// Nucleus sampling probability.
    #[serde(default = "default_in_process_llama_top_p")]
    #[schemars(range(min = 0.0, max = 1.0))]
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

#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AiPrivacyConfig {
    /// If true, Nova will not use any cloud providers. This is the recommended
    /// setting for privacy-sensitive environments.
    #[serde(default = "default_local_only")]
    pub local_only: bool,

    /// Allow including file system paths (file names, workspace-relative paths, or absolute paths)
    /// in prompts/context sent to the LLM.
    ///
    /// This is a **high-sensitivity** setting: file paths can leak user names, organization names,
    /// internal directory structure, and other metadata. The safe default is `false`.
    ///
    /// Note: `ai.privacy.excluded_paths` is still enforced regardless of this flag. Excluded files
    /// are omitted from prompts, and Nova avoids attaching file path metadata for excluded files.
    #[serde(default)]
    pub include_file_paths: bool,

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

impl fmt::Debug for AiPrivacyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiPrivacyConfig")
            .field("local_only", &self.local_only)
            .field("include_file_paths", &self.include_file_paths)
            .field("anonymize_identifiers", &self.anonymize_identifiers)
            .field("redact_sensitive_strings", &self.redact_sensitive_strings)
            .field("redact_numeric_literals", &self.redact_numeric_literals)
            .field("strip_or_redact_comments", &self.strip_or_redact_comments)
            .field("excluded_paths_count", &self.excluded_paths.len())
            .field("redact_patterns_count", &self.redact_patterns.len())
            .field("allow_cloud_code_edits", &self.allow_cloud_code_edits)
            .field(
                "allow_code_edits_without_anonymization",
                &self.allow_code_edits_without_anonymization,
            )
            .finish()
    }
}

fn default_local_only() -> bool {
    true
}

impl Default for AiPrivacyConfig {
    fn default() -> Self {
        Self {
            local_only: default_local_only(),
            include_file_paths: false,
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
    Toml(String),
}

fn sanitize_toml_error_message(message: &str) -> String {
    // `toml::de::Error::message()` can still include user-provided scalar values, e.g.
    // `invalid type: string "secret", expected a boolean`.
    //
    // Config parsing errors are commonly surfaced through CLI/LSP diagnostics and logs, so redact
    // quoted substrings to avoid leaking arbitrary config string values (including secrets) even
    // when no snippet is included.
    static QUOTED_STRING_RE: OnceLock<regex::Regex> = OnceLock::new();
    static SINGLE_QUOTED_STRING_RE: OnceLock<regex::Regex> = OnceLock::new();

    // Handle escaped quotes (e.g. `\"`) inside the quoted substring. `toml::de::Error::message()`
    // includes string values using a debug-like escaping style, so an embedded quote will appear as
    // `\"`. A naive `"[^"]*"` pattern would stop at the escaped quote and leak the remainder.
    let re = QUOTED_STRING_RE.get_or_init(|| {
        regex::Regex::new(r#""(?:\\.|[^"\\])*""#)
            .expect("quoted-string regex should compile")
    });

    let mut out = re.replace_all(message, r#""<redacted>""#).into_owned();
    let re_single = SINGLE_QUOTED_STRING_RE.get_or_init(|| {
        regex::Regex::new(r#"'(?:\\.|[^'\\])*'"#)
            .expect("single-quoted-string regex should compile")
    });
    out = re_single
        .replace_all(&out, "'<redacted>'")
        .into_owned();

    // `serde` uses backticks in a few different diagnostics:
    //
    // - `unknown field `secret`, expected ...` (user-controlled key → redact)
    // - `unknown variant `secret`, expected ...` (user-controlled variant → redact)
    // - `invalid type: integer `123`, expected ...` (user-controlled scalar → redact)
    // - `missing field `foo`` (schema field name → keep)
    //
    // Redact only when the backticked segment is known to contain user-controlled content.
    let mut start = ["unknown field `", "unknown variant `"]
        .iter()
        .filter_map(|pattern| out.find(pattern).map(|pos| pos + pattern.len().saturating_sub(1)))
        .min();
    if start.is_none() && (out.contains("invalid type:") || out.contains("invalid value:")) {
        // `invalid type/value` errors include the unexpected scalar value before `, expected ...`.
        // Redact only backticked values in that prefix so we don't hide schema names in the
        // expected portion.
        let boundary = out.find(", expected").unwrap_or(out.len());
        start = out[..boundary].find('`');
        if start.is_none() && boundary == out.len() {
            // Some serde errors omit the `, expected ...` suffix. Fall back to the first backtick.
            start = out.find('`');
        }
    }
    if let Some(start) = start {
        let after_start = &out[start.saturating_add(1)..];
        let end = if let Some(end_rel) = after_start.rfind("`, expected") {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else if let Some(end_rel) = after_start.rfind('`') {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else {
            None
        };
        if let Some(end) = end {
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

impl From<toml::de::Error> for ConfigError {
    fn from(err: toml::de::Error) -> Self {
        // `toml::de::Error`'s default `Display` includes a source snippet, which may contain
        // secrets (for example `ai.api_key = "..."`) or credentials embedded into URLs.
        //
        // Avoid including any raw input text in the error string. Keep just the message and
        // location (when available) to remain actionable without leaking configuration contents
        // into logs.
        //
        // Note: `toml::de::Error` does not expose stable line/column APIs on all supported `toml`
        // versions; keep this simple and snippet-free.
        ConfigError::Toml(sanitize_toml_error_message(err.message()))
    }
}

impl NovaConfig {
    /// Load a config file from TOML.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let mut config: NovaConfig = toml::from_str(&text)?;
        config.extensions.normalize();
        Ok(config)
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
        let mut toolchains: BTreeMap<u16, PathBuf> = BTreeMap::new();
        for toolchain in &self.jdk.toolchains {
            if toolchain.release == 0 {
                tracing::warn!(
                    target: "nova.config",
                    "ignoring JDK toolchain release 0 (must be >= 1)"
                );
                continue;
            }

            if toolchains
                .insert(toolchain.release, toolchain.home.clone())
                .is_some()
            {
                tracing::warn!(
                    target: "nova.config",
                    release = toolchain.release,
                    "duplicate JDK toolchain configured for --release {}; last entry wins",
                    toolchain.release
                );
            }
        }

        nova_core::JdkConfig {
            home: self.jdk.home.clone(),
            release: self.jdk.release.filter(|release| *release >= 1),
            toolchains,
        }
    }

    pub fn memory_budget_overrides(&self) -> nova_memory::MemoryBudgetOverrides {
        self.memory.memory_budget_overrides()
    }
}

pub const NOVA_CONFIG_ENV_VAR: &str = "NOVA_CONFIG_PATH";

static CONFIG_ENV_LOCK: OnceLock<ReentrantMutex<()>> = OnceLock::new();

fn config_env_lock() -> &'static ReentrantMutex<()> {
    CONFIG_ENV_LOCK.get_or_init(|| ReentrantMutex::new(()))
}

/// Run `f` while holding Nova's config environment lock.
///
/// Tests sometimes need to temporarily set [`NOVA_CONFIG_ENV_VAR`] (`NOVA_CONFIG_PATH`). Because
/// environment variables are process-global, concurrent config discovery in other threads/tests can
/// observe the temporary override and become flaky. Wrapping the mutation + config discovery logic
/// in this helper ensures access is serialized.
pub fn with_config_env_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = config_env_lock().lock();
    f()
}

/// Discover the Nova configuration file for a workspace root.
///
/// Search order:
/// 1) `NOVA_CONFIG_PATH` (absolute or relative to `workspace_root`)
/// 2) `nova.toml` in `workspace_root`
/// 3) `.nova.toml` in `workspace_root`
/// 4) `nova.config.toml` in `workspace_root`
/// 5) `.nova/config.toml` in `workspace_root` (legacy fallback)
pub fn discover_config_path(workspace_root: &Path) -> Option<PathBuf> {
    let _guard = config_env_lock().lock();
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

        if jdk.contains_key("target_release") {
            out.push(ConfigWarning::DeprecatedKey {
                path: "jdk.target_release".to_string(),
                message: "jdk.target_release is deprecated; use jdk.release instead".to_string(),
            });
        }
    }

    if let Some(privacy) = value
        .get("ai")
        .and_then(|v| v.as_table())
        .and_then(|ai| ai.get("privacy"))
        .and_then(|v| v.as_table())
    {
        if privacy.contains_key("anonymize") {
            out.push(ConfigWarning::DeprecatedKey {
                path: "ai.privacy.anonymize".to_string(),
                message:
                    "ai.privacy.anonymize is deprecated; use ai.privacy.anonymize_identifiers instead"
                        .to_string(),
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

fn log_line_contains_serde_json_error(line: &str) -> bool {
    // `serde_json::Error` values can embed user-controlled scalar values in their display strings
    // (for example `invalid type: string "..."` or `unknown field `...`, expected ...`). Those
    // errors sometimes make their way into tracing logs (notably via `?err`), which are later
    // included in bug report bundles.
    //
    // Keep this detection intentionally narrow: only sanitize log lines that look like they
    // contain serde/serde_json diagnostics that are known to echo scalar values.
    line.contains("invalid type:")
        || line.contains("invalid value:")
        || line.contains("unknown field")
        || line.contains("unknown variant")
}

fn sanitize_json_error_message(message: &str) -> String {
    nova_core::sanitize_json_error_message(message)
}

fn sanitize_bugreport_log_line(line: &str) -> String {
    if log_line_contains_serde_json_error(line) {
        sanitize_json_error_message(line)
    } else {
        line.to_owned()
    }
}

fn sanitize_plain_log_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(text.len());
    for chunk in text.split_inclusive('\n') {
        if let Some(line) = chunk.strip_suffix('\n') {
            let line = line.trim_end_matches('\r');
            out.push_str(&sanitize_bugreport_log_line(line));
            out.push('\n');
        } else {
            let line = chunk.trim_end_matches('\r');
            out.push_str(&sanitize_bugreport_log_line(line));
        }
    }
    out
}

fn sanitize_json_log_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            if log_line_contains_serde_json_error(&s) {
                serde_json::Value::String(sanitize_json_error_message(&s))
            } else {
                serde_json::Value::String(s)
            }
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(sanitize_json_log_value)
                .collect::<Vec<_>>(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, sanitize_json_log_value(value)))
                .collect(),
        ),
        other => other,
    }
}

fn sanitize_json_log_line(line: &str) -> String {
    if !log_line_contains_serde_json_error(line) {
        return line.to_owned();
    }

    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return sanitize_json_error_message(line),
    };
    let sanitized = sanitize_json_log_value(value);
    serde_json::to_string(&sanitized).unwrap_or_else(|_| sanitize_json_error_message(line))
}

fn sanitize_json_log_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(text.len());
    for chunk in text.split_inclusive('\n') {
        if let Some(line) = chunk.strip_suffix('\n') {
            let line = line.trim_end_matches('\r');
            out.push_str(&sanitize_json_log_line(line));
            out.push('\n');
        } else {
            let line = chunk.trim_end_matches('\r');
            out.push_str(&sanitize_json_log_line(line));
        }
    }
    out
}

#[derive(Clone, Copy, Debug)]
enum LogSanitizeMode {
    PlainText,
    Json,
}

struct SanitizingMakeWriter<M> {
    inner: M,
    mode: LogSanitizeMode,
}

impl<'a, M> MakeWriter<'a> for SanitizingMakeWriter<M>
where
    M: MakeWriter<'a>,
{
    type Writer = SanitizingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        SanitizingWriter {
            inner: self.inner.make_writer(),
            bytes: Vec::new(),
            mode: self.mode,
        }
    }
}

struct SanitizingWriter<W: Write> {
    inner: W,
    bytes: Vec<u8>,
    mode: LogSanitizeMode,
}

impl<W: Write> Write for SanitizingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for SanitizingWriter<W> {
    fn drop(&mut self) {
        if self.bytes.is_empty() {
            return;
        }

        let text = String::from_utf8_lossy(&self.bytes);
        let sanitized = match self.mode {
            LogSanitizeMode::PlainText => sanitize_plain_log_text(&text),
            LogSanitizeMode::Json => sanitize_json_log_text(&text),
        };
        let _ = self.inner.write_all(sanitized.as_bytes());
        let _ = self.inner.flush();
    }
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
                self.buffer.push_line(sanitize_bugreport_log_line(line));
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
        #[cfg(unix)]
        let audit_insecure_permissions = audit_path.as_ref().and_then(|path| {
            use std::os::unix::fs::PermissionsExt;

            let metadata = std::fs::metadata(path).ok()?;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                Some(mode)
            } else {
                None
            }
        });
        #[cfg(not(unix))]
        let audit_insecure_permissions: Option<u32> = None;
        let audit_file = audit_path
            .as_ref()
            .and_then(|path| {
                let mut options = std::fs::OpenOptions::new();
                options.create(true).append(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;

                    // AI audit logs can contain sensitive data (even after redaction).
                    // Create with owner-only permissions by default.
                    options.mode(0o600);
                }

                options.open(path).ok()
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

        // Best-effort: sanitize serde_json-style diagnostics before writing them to stderr/file
        // sinks. When `logging.json=true`, keep the output as valid JSON by parsing and
        // re-serializing the structured log line.
        make_writer = BoxMakeWriter::new(SanitizingMakeWriter {
            inner: make_writer,
            mode: if logging.json {
                LogSanitizeMode::Json
            } else {
                LogSanitizeMode::PlainText
            },
        });

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
        if tracing::subscriber::set_global_default(subscriber).is_ok() {
            if audit_open_failed {
                if let Some(path) = audit_path.as_ref() {
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

            if let (Some(path), Some(mode)) = (audit_path.as_ref(), audit_insecure_permissions) {
                tracing::warn!(
                    target: "nova.config",
                    path = %path.display(),
                    permissions = %format!("{mode:03o}"),
                    "AI audit log file has group/other permissions set; consider chmod 600"
                );
            }
        }
    });

    buffer
}

#[cfg(test)]
mod toml_tests {
    use super::*;

    fn shared_audit_log_path() -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("audit.log");
            // Keep the directory alive for the duration of the test process so
            // the file path stays valid even if multiple tests race to
            // initialize global tracing.
            std::mem::forget(dir);
            path
        })
        .clone()
    }

    #[test]
    fn logging_level_parses_simple_levels() {
        let logging = LoggingConfig {
            level: "DEBUG".to_owned(),
            ..Default::default()
        };

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
        let logging = LoggingConfig {
            level: "warn,nova_config=trace".to_owned(),
            ..Default::default()
        };

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
        let audit_path = shared_audit_log_path();

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

    #[cfg(unix)]
    #[test]
    fn audit_log_file_is_created_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let audit_path = shared_audit_log_path();

        let mut config = NovaConfig::default();
        config.ai.enabled = true;
        config.ai.audit_log.enabled = true;
        config.ai.audit_log.path = Some(audit_path.clone());

        init_tracing_with_config(&config);

        let mode = std::fs::metadata(&audit_path)
            .expect("audit log metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode & 0o077,
            0,
            "AI audit log file should not be accessible by group/other (mode {:03o})",
            mode
        );
    }

    #[test]
    fn log_buffer_sanitizes_serde_json_error_messages() {
        let buffer = Arc::new(LogBuffer::new(64));
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::default()
                    .add_directive(tracing_subscriber::filter::LevelFilter::WARN.into()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(LogBufferMakeWriter {
                        buffer: buffer.clone(),
                    })
                    .with_ansi(false),
            );

        tracing::subscriber::with_default(subscriber, || {
            let secret_suffix = "nova-config-log-buffer-secret-token";
            let secret = format!("prefix\"{secret_suffix}");
            let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
                .expect_err("expected type mismatch");
            let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);
            tracing::warn!(target: "nova.config", error = ?io_err, "failed to parse JSON");
        });

        let text = buffer.last_lines(64).join("\n");
        assert!(
            !text.contains("nova-config-log-buffer-secret-token"),
            "expected log buffer to omit string scalar values from serde_json errors: {text}"
        );
        assert!(
            text.contains("<redacted>"),
            "expected log buffer to include redaction marker: {text}"
        );
    }

    #[test]
    fn log_buffer_sanitizes_serde_json_error_messages_with_backticked_unknown_fields() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let buffer = Arc::new(LogBuffer::new(64));
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::default()
                    .add_directive(tracing_subscriber::filter::LevelFilter::WARN.into()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(LogBufferMakeWriter {
                        buffer: buffer.clone(),
                    })
                    .with_ansi(false),
            );

        tracing::subscriber::with_default(subscriber, || {
            let secret_suffix = "nova-config-log-buffer-backtick-secret-token";
            let secret = format!("prefix`, expected {secret_suffix}");
            let json = format!(r#"{{"{secret}": 1}}"#);
            let serde_err =
                serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field");
            let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);
            tracing::warn!(target: "nova.config", error = ?io_err, "failed to parse JSON");
        });

        let text = buffer.last_lines(64).join("\n");
        assert!(
            !text.contains("nova-config-log-buffer-backtick-secret-token"),
            "expected log buffer to omit backticked scalar values from serde_json errors: {text}"
        );
        assert!(
            text.contains("<redacted>"),
            "expected log buffer to include redaction marker: {text}"
        );
    }

    #[test]
    fn json_logging_sanitizes_serde_json_error_messages_and_remains_valid_json() {
        use std::io;

        #[derive(Clone)]
        struct SharedBytesMakeWriter {
            bytes: Arc<std::sync::Mutex<Vec<u8>>>,
        }

        struct SharedBytesWriter {
            bytes: Arc<std::sync::Mutex<Vec<u8>>>,
        }

        impl io::Write for SharedBytesWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let mut guard = self.bytes.lock().expect("bytes mutex poisoned");
                guard.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for SharedBytesMakeWriter {
            type Writer = SharedBytesWriter;

            fn make_writer(&'a self) -> Self::Writer {
                SharedBytesWriter {
                    bytes: self.bytes.clone(),
                }
            }
        }

        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make_writer = SanitizingMakeWriter {
            inner: SharedBytesMakeWriter { bytes: bytes.clone() },
            mode: LogSanitizeMode::Json,
        };

        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::default()
                    .add_directive(tracing_subscriber::filter::LevelFilter::WARN.into()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(make_writer)
                    .with_ansi(false),
            );

        tracing::subscriber::with_default(subscriber, || {
            let secret_suffix = "nova-config-json-log-secret-token";
            let secret = format!("prefix\"{secret_suffix}");
            let serde_err = serde_json::from_value::<bool>(serde_json::json!(secret))
                .expect_err("expected type mismatch");
            let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);
            tracing::warn!(target: "nova.config", error = ?io_err, "failed to parse JSON");
        });

        let out = {
            let guard = bytes.lock().expect("bytes mutex poisoned");
            String::from_utf8_lossy(&guard).into_owned()
        };

        let line = out
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("expected at least one log line");
        let value: serde_json::Value =
            serde_json::from_str(line).expect("expected sanitized output to remain valid JSON");

        let rendered = value.to_string();
        assert!(
            !rendered.contains("nova-config-json-log-secret-token"),
            "expected JSON log line to omit string scalar values from serde_json errors: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "expected JSON log line to include redaction marker: {rendered}"
        );
    }

    #[test]
    fn json_logging_sanitizes_serde_json_error_messages_with_backticked_unknown_fields() {
        use std::io;

        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        #[derive(Clone)]
        struct SharedBytesMakeWriter {
            bytes: Arc<std::sync::Mutex<Vec<u8>>>,
        }

        struct SharedBytesWriter {
            bytes: Arc<std::sync::Mutex<Vec<u8>>>,
        }

        impl io::Write for SharedBytesWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let mut guard = self.bytes.lock().expect("bytes mutex poisoned");
                guard.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for SharedBytesMakeWriter {
            type Writer = SharedBytesWriter;

            fn make_writer(&'a self) -> Self::Writer {
                SharedBytesWriter {
                    bytes: self.bytes.clone(),
                }
            }
        }

        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let make_writer = SanitizingMakeWriter {
            inner: SharedBytesMakeWriter { bytes: bytes.clone() },
            mode: LogSanitizeMode::Json,
        };

        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::default()
                    .add_directive(tracing_subscriber::filter::LevelFilter::WARN.into()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(make_writer)
                    .with_ansi(false),
            );

        tracing::subscriber::with_default(subscriber, || {
            let secret_suffix = "nova-config-json-log-backtick-secret-token";
            let secret = format!("prefix`, expected {secret_suffix}");
            let json = format!(r#"{{"{secret}": 1}}"#);
            let serde_err =
                serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field");
            let io_err = std::io::Error::new(std::io::ErrorKind::Other, serde_err);
            tracing::warn!(target: "nova.config", error = ?io_err, "failed to parse JSON");
        });

        let out = {
            let guard = bytes.lock().expect("bytes mutex poisoned");
            String::from_utf8_lossy(&guard).into_owned()
        };
        let line = out
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("expected at least one log line");
        let value: serde_json::Value =
            serde_json::from_str(line).expect("expected sanitized output to remain valid JSON");

        let rendered = value.to_string();
        assert!(
            !rendered.contains("nova-config-json-log-backtick-secret-token"),
            "expected JSON log line to omit backticked scalar values from serde_json errors: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "expected JSON log line to include redaction marker: {rendered}"
        );
    }

    #[test]
    fn toml_error_sanitization_preserves_missing_field_names() {
        #[derive(Debug, serde::Deserialize)]
        struct Dummy {
            #[allow(dead_code)]
            required: String,
        }

        let raw_err = toml::from_str::<Dummy>("").expect_err("expected missing field error");
        let raw_message = raw_err.message();
        assert!(
            raw_message.contains("missing field"),
            "expected raw toml error message to mention missing field: {raw_message}"
        );
        assert!(
            raw_message.contains("`required`"),
            "expected raw toml error message to include the missing field name: {raw_message}"
        );

        let message = sanitize_toml_error_message(raw_message);
        assert!(
            message.contains("`required`"),
            "expected sanitized toml error message to preserve the missing field name: {message}"
        );
        assert!(
            !message.contains("<redacted>"),
            "expected missing-field toml error message to avoid unnecessary redaction: {message}"
        );
    }

    #[test]
    fn toml_error_sanitization_redacts_single_quoted_values() {
        let secret_suffix = "nova-config-toml-single-quote-secret";
        let message = format!("invalid semver version '{secret_suffix}': boom");
        let sanitized = sanitize_toml_error_message(&message);

        assert!(
            !sanitized.contains(secret_suffix),
            "expected toml error sanitizer to redact single-quoted values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected toml error sanitizer to include redaction marker: {sanitized}"
        );
    }

    #[test]
    fn toml_error_sanitization_redacts_backticked_numeric_values() {
        #[derive(Debug, serde::Deserialize)]
        struct Dummy {
            #[allow(dead_code)]
            flag: bool,
        }

        let raw_err =
            toml::from_str::<Dummy>("flag = 123").expect_err("expected invalid type error");
        let raw_message = raw_err.message();
        assert!(
            raw_message.contains("123"),
            "expected raw toml error message to include the numeric value so this test catches leaks: {raw_message}"
        );

        let sanitized = sanitize_toml_error_message(raw_message);
        assert!(
            !sanitized.contains("123"),
            "expected toml error sanitizer to redact backticked numeric values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected toml error sanitizer to include redaction marker: {sanitized}"
        );
        assert!(
            sanitized.contains("expected"),
            "expected toml error sanitizer to preserve the rest of the message: {sanitized}"
        );
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
    fn ai_config_debug_does_not_expose_api_key() {
        let key = "super-secret-api-key";
        let config = AiConfig {
            api_key: Some(key.to_string()),
            ..Default::default()
        };

        let output = format!("{config:?}");
        assert!(
            !output.contains(key),
            "AiConfig debug output leaked api_key: {output}"
        );
        assert!(
            output.contains("api_key_present"),
            "AiConfig debug output should include api_key presence indicator: {output}"
        );
    }

    #[test]
    fn ai_privacy_config_debug_does_not_expose_redact_patterns() {
        let secret = "super-secret-pattern";
        let config = AiPrivacyConfig {
            redact_patterns: vec![secret.to_string()],
            ..Default::default()
        };

        let output = format!("{config:?}");
        assert!(
            !output.contains(secret),
            "AiPrivacyConfig debug output leaked redact_patterns: {output}"
        );
        assert!(
            output.contains("redact_patterns_count"),
            "AiPrivacyConfig debug output should include redact_patterns_count: {output}"
        );
    }

    #[test]
    fn ai_provider_config_debug_redacts_sensitive_url_parts() {
        let username = "super-secret-user";
        let password = "super-secret-pass";
        let token = "super-secret-token";
        let config = AiProviderConfig {
            url: Url::parse(&format!(
                "https://{username}:{password}@example.com/path?token={token}&other=1"
            ))
            .expect("parse url"),
            ..Default::default()
        };

        let output = format!("{config:?}");
        assert!(
            !output.contains(username),
            "AiProviderConfig debug output leaked url username: {output}"
        );
        assert!(
            !output.contains(password),
            "AiProviderConfig debug output leaked url password: {output}"
        );
        assert!(
            !output.contains(token),
            "AiProviderConfig debug output leaked url token param: {output}"
        );
        assert!(
            output.contains("<redacted>"),
            "AiProviderConfig debug output should include redaction markers: {output}"
        );
    }

    #[test]
    fn ai_config_features_and_timeouts_roundtrip_toml() {
        let mut config = AiConfig {
            enabled: true,
            ..Default::default()
        };
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
    fn toml_without_build_table_uses_defaults() {
        let config: NovaConfig = toml::from_str("").expect("config should parse");
        assert_eq!(config.build, BuildIntegrationConfig::default());
    }

    #[test]
    fn toml_build_table_parses_mode_and_timeouts() {
        let text = r#"
[build]
mode = "on"
timeout_ms = 30000

[build.gradle]
mode = "off"
timeout_ms = 10000
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");
        assert_eq!(config.build.mode, BuildIntegrationMode::On);
        assert_eq!(config.build.timeout_ms, 30_000);
        assert_eq!(config.build.maven.mode, None);
        assert_eq!(config.build.maven.timeout_ms, None);
        assert_eq!(config.build.gradle.mode, Some(BuildIntegrationMode::Off));
        assert_eq!(config.build.gradle.timeout_ms, Some(10_000));

        assert_eq!(config.build.maven_mode(), BuildIntegrationMode::On);
        assert_eq!(config.build.gradle_mode(), BuildIntegrationMode::Off);
        assert_eq!(config.build.maven_timeout(), Duration::from_millis(30_000));
        assert_eq!(config.build.gradle_timeout(), Duration::from_millis(10_000));
    }

    #[test]
    fn toml_build_integration_table_is_accepted_as_alias() {
        let text = r#"
[build_integration]
mode = "off"
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");
        assert_eq!(config.build.mode, BuildIntegrationMode::Off);
    }

    #[test]
    fn toml_build_table_parses_enabled_and_timeout() {
        let text = r#"
[build]
enabled = true
timeout_ms = 12345
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");
        assert_eq!(config.build.enabled, Some(true));
        assert_eq!(config.build.timeout_ms, 12_345);
        assert_eq!(config.build.maven_mode(), BuildIntegrationMode::On);
        assert_eq!(config.build.gradle_mode(), BuildIntegrationMode::On);

        let round_trip = toml::to_string(&config).expect("serialize");
        let decoded: NovaConfig = toml::from_str(&round_trip).expect("deserialize");
        assert_eq!(decoded.build, config.build);
    }

    #[test]
    fn toml_build_tool_toggles_parse() {
        let text = r#"
[build]
enabled = true
timeout_ms = 1000

[build.maven]
enabled = false

[build.gradle]
enabled = true
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");
        assert_eq!(config.build.enabled, Some(true));
        assert_eq!(config.build.timeout_ms, 1_000);
        assert_eq!(config.build.maven.enabled, Some(false));
        assert_eq!(config.build.gradle.enabled, Some(true));
        assert_eq!(config.build.maven_mode(), BuildIntegrationMode::Off);
        assert_eq!(config.build.gradle_mode(), BuildIntegrationMode::On);
    }

    #[test]
    fn build_timeout_ms_zero_is_invalid_when_enabled() {
        let text = r#"
[build]
enabled = true
timeout_ms = 0
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");

        // `BuildIntegrationConfig::timeout()` normalizes the effective value.
        assert_eq!(config.build.timeout(), Duration::from_millis(1));

        let diagnostics = config.validate();
        assert!(diagnostics.errors.is_empty());
        assert_eq!(
            diagnostics.warnings,
            vec![ConfigWarning::InvalidValue {
                toml_path: "build.timeout_ms".to_string(),
                message: "must be >= 1 when build.enabled is true (0 is treated as 1)".to_string(),
            }]
        );
    }

    #[test]
    fn build_enabled_with_all_tools_disabled_warns() {
        let text = r#"
[build]
enabled = true

[build.maven]
enabled = false

[build.gradle]
enabled = false
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");
        assert_eq!(config.build.enabled, Some(true));
        assert_eq!(config.build.maven.enabled, Some(false));
        assert_eq!(config.build.gradle.enabled, Some(false));

        let diagnostics = config.validate();
        assert!(diagnostics.errors.is_empty());
        assert_eq!(
            diagnostics.warnings,
            vec![ConfigWarning::InvalidValue {
                toml_path: "build.enabled".to_string(),
                message: "build.enabled=true but all build tools are disabled; enable build.maven.enabled and/or build.gradle.enabled"
                    .to_string(),
            }]
        );
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

    #[test]
    fn extensions_config_allow_none_means_no_allow_restriction() {
        let config = ExtensionsConfig {
            deny: vec!["com.evil.*".to_owned()],
            ..Default::default()
        };

        assert!(config.is_extension_allowed("com.example.one"));
        assert!(!config.is_extension_allowed("com.evil.rules"));
    }

    #[test]
    fn extensions_config_exact_allow_and_deny() {
        let config = ExtensionsConfig {
            allow: Some(vec!["com.good.one".to_owned()]),
            deny: vec!["com.good.two".to_owned()],
            ..Default::default()
        };

        assert!(config.is_extension_allowed("com.good.one"));
        assert!(!config.is_extension_allowed("com.good.one.extra"));
        assert!(!config.is_extension_allowed("com.good.two"));
    }

    #[test]
    fn extensions_config_wildcards_at_beginning_middle_end() {
        let config = ExtensionsConfig {
            allow: Some(vec!["*.rules".to_owned()]),
            ..Default::default()
        };
        assert!(config.is_extension_allowed("com.mycorp.rules"));
        assert!(!config.is_extension_allowed("com.mycorp.rule"));

        let config = ExtensionsConfig {
            allow: Some(vec!["com.*.rules".to_owned()]),
            ..Default::default()
        };
        assert!(config.is_extension_allowed("com.mycorp.rules"));
        assert!(config.is_extension_allowed("com.mycorp.internal.rules"));
        assert!(!config.is_extension_allowed("org.mycorp.rules"));

        let config = ExtensionsConfig {
            allow: Some(vec!["com.mycorp.*".to_owned()]),
            ..Default::default()
        };
        assert!(config.is_extension_allowed("com.mycorp.rules"));
        assert!(!config.is_extension_allowed("com.mycorpish.rules"));
    }

    #[test]
    fn extensions_config_multiple_wildcards_match() {
        let config = ExtensionsConfig {
            allow: Some(vec!["com.*rules*beta".to_owned()]),
            ..Default::default()
        };

        assert!(config.is_extension_allowed("com.acme.rules.v1.beta"));
        assert!(!config.is_extension_allowed("com.acme.rules.v1.betas"));

        let config = ExtensionsConfig {
            allow: Some(vec!["com.**.beta".to_owned()]),
            ..Default::default()
        };
        assert!(config.is_extension_allowed("com.acme.beta"));
        assert!(config.is_extension_allowed("com.acme.internal.beta"));
    }

    #[test]
    fn extensions_config_deny_overrides_allow() {
        let config = ExtensionsConfig {
            allow: Some(vec!["com.*".to_owned()]),
            deny: vec!["com.evil.*".to_owned()],
            ..Default::default()
        };

        assert!(config.is_extension_allowed("com.good.one"));
        assert!(!config.is_extension_allowed("com.evil.one"));
    }

    #[test]
    fn extensions_config_disabled_disables_all() {
        let config = ExtensionsConfig {
            enabled: false,
            allow: None,
            deny: vec!["*".to_owned()],
            ..Default::default()
        };

        assert!(!config.is_extension_allowed("com.good.one"));
        assert!(!config.is_extension_allowed("com.evil.one"));
    }

    #[test]
    fn toml_jdk_home_alias_parses() {
        let text = r#"
[jdk]
jdk_home = "/opt/jdks/jdk-21"
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");
        let jdk = config.jdk_config();

        assert_eq!(jdk.home, Some(PathBuf::from("/opt/jdks/jdk-21")));
        assert_eq!(jdk.release, None);
        assert!(jdk.toolchains.is_empty());
    }

    #[test]
    fn toml_jdk_release_parses() {
        let config: NovaConfig = toml::from_str(
            r#"
[jdk]
release = 17
"#,
        )
        .expect("config should parse");
        assert_eq!(config.jdk_config().release, Some(17));

        let config: NovaConfig = toml::from_str(
            r#"
[jdk]
target_release = 11
"#,
        )
        .expect("config should parse");
        assert_eq!(config.jdk_config().release, Some(11));
    }

    #[test]
    fn toml_jdk_toolchains_list_parses_into_core_config() {
        let config: NovaConfig = toml::from_str(
            r#"
[jdk]
home = "/opt/jdks/jdk-21"
release = 17

[[jdk.toolchains]]
release = 8
home = "/opt/jdks/jdk8"

[[jdk.toolchains]]
release = 17
home = "/opt/jdks/jdk-17"
"#,
        )
        .expect("config should parse");

        let jdk = config.jdk_config();
        assert_eq!(jdk.home, Some(PathBuf::from("/opt/jdks/jdk-21")));
        assert_eq!(jdk.release, Some(17));
        let expected: BTreeMap<u16, PathBuf> = [
            (8u16, PathBuf::from("/opt/jdks/jdk8")),
            (17u16, PathBuf::from("/opt/jdks/jdk-17")),
        ]
        .into_iter()
        .collect();
        assert_eq!(jdk.toolchains, expected);
    }

    #[test]
    fn toml_jdk_toolchains_duplicate_release_last_wins() {
        let config: NovaConfig = toml::from_str(
            r#"
[jdk]

[[jdk.toolchains]]
release = 8
home = "/opt/jdks/jdk8-a"

[[jdk.toolchains]]
release = 8
home = "/opt/jdks/jdk8-b"
"#,
        )
        .expect("config should parse");

        let expected: BTreeMap<u16, PathBuf> = [(8u16, PathBuf::from("/opt/jdks/jdk8-b"))]
            .into_iter()
            .collect();
        assert_eq!(config.jdk_config().toolchains, expected);
    }

    #[test]
    fn toml_memory_table_parses_and_converts_to_overrides() {
        let text = r#"
[memory]
total_bytes = "1G"
query_cache_bytes = "512M"
"#;

        let config: NovaConfig = toml::from_str(text).expect("config should parse");
        let overrides = config.memory_budget_overrides();

        assert_eq!(overrides.total, Some(nova_memory::GB));
        assert_eq!(
            overrides.categories.query_cache,
            Some(512 * nova_memory::MB)
        );
    }
}
