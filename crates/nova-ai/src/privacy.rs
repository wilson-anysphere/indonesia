use crate::anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};

/// Privacy controls that apply when building prompts for cloud models.
#[derive(Debug, Clone)]
pub struct PrivacyMode {
    /// Replace identifiers (class/method/variable names) with stable placeholders.
    pub anonymize_identifiers: bool,

    /// Redact suspicious literals (API keys, tokens, long IDs, etc).
    pub redaction: RedactionConfig,

    /// Whether the builder is allowed to include file system paths.
    pub include_file_paths: bool,
}

impl Default for PrivacyMode {
    fn default() -> Self {
        Self {
            anonymize_identifiers: false,
            redaction: RedactionConfig::default(),
            include_file_paths: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RedactionConfig {
    pub redact_string_literals: bool,
    pub redact_numeric_literals: bool,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            redact_string_literals: true,
            redact_numeric_literals: true,
        }
    }
}

pub fn redact_suspicious_literals(code: &str, cfg: &RedactionConfig) -> String {
    let mut anonymizer = CodeAnonymizer::new(CodeAnonymizerOptions {
        anonymize_identifiers: false,
        redact_sensitive_strings: cfg.redact_string_literals,
        redact_numeric_literals: cfg.redact_numeric_literals,
        strip_or_redact_comments: false,
    });
    anonymizer.anonymize(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_suspicious_string_literals() {
        let code = r#"String apiKey = "sk-verysecretstringthatislong";"#;
        let redacted = redact_suspicious_literals(code, &RedactionConfig::default());
        assert!(redacted.contains("\"[REDACTED]\""));
        assert!(!redacted.contains("sk-verysecret"));
    }
}
