use std::path::PathBuf;
use std::time::Duration;

/// Configuration for all AI-adjacent capabilities in Nova.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiConfig {
    pub features: AiFeatures,
    pub local: LocalModelConfig,
    pub cloud: Option<CloudConfig>,
    pub privacy: PrivacyConfig,
    pub timeouts: AiTimeouts,
}

impl AiConfig {
    /// A configuration that disables all AI features.
    pub fn disabled() -> Self {
        Self {
            features: AiFeatures::disabled(),
            local: LocalModelConfig::default(),
            cloud: None,
            privacy: PrivacyConfig::default(),
            timeouts: AiTimeouts::default(),
        }
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            features: AiFeatures::default(),
            local: LocalModelConfig::default(),
            cloud: None,
            privacy: PrivacyConfig::default(),
            timeouts: AiTimeouts::default(),
        }
    }
}

/// Feature toggles. Nova must always work without AI enabled.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AiFeatures {
    pub completion_ranking: bool,
    pub semantic_search: bool,
}

impl AiFeatures {
    pub fn disabled() -> Self {
        Self {
            completion_ranking: false,
            semantic_search: false,
        }
    }
}

impl Default for AiFeatures {
    fn default() -> Self {
        Self {
            completion_ranking: false,
            semantic_search: false,
        }
    }
}

/// Local model settings (paths/resources). Models are optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalModelConfig {
    pub model_dir: PathBuf,
    pub use_gpu: bool,
    pub max_memory_bytes: usize,
}

impl Default for LocalModelConfig {
    fn default() -> Self {
        Self {
            model_dir: PathBuf::from(".nova/models"),
            use_gpu: false,
            max_memory_bytes: 512 * 1024 * 1024,
        }
    }
}

/// Cloud provider settings. This is optional and must not be required for core functionality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudConfig {
    pub provider: CloudProvider,
    pub api_key: String,
    pub endpoint: Option<String>,
    pub model: String,
    pub max_tokens: usize,
    pub timeout: Duration,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum CloudProvider {
    OpenAi,
    Anthropic,
    Google,
    AzureOpenAi,
    SelfHosted,
}

/// Privacy settings controlling what information may leave the local machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyConfig {
    /// Never send code to cloud providers (even if `cloud` is configured).
    pub local_only: bool,

    /// Anonymize code before sending externally.
    pub anonymize: bool,

    /// Redact string literals that look sensitive.
    pub redact_sensitive_strings: bool,

    /// Paths that must never be sent to external services.
    pub excluded_paths: Vec<PathBuf>,

    /// Record all AI interactions for audit purposes.
    pub audit_logging: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            local_only: true,
            anonymize: true,
            redact_sensitive_strings: true,
            excluded_paths: Vec::new(),
            audit_logging: false,
        }
    }
}

/// Timeouts for AI operations. These protect interactive editor requests.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AiTimeouts {
    pub completion_ranking: Duration,
}

impl Default for AiTimeouts {
    fn default() -> Self {
        Self {
            completion_ranking: Duration::from_millis(20),
        }
    }
}
