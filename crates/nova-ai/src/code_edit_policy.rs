use nova_config::AiPrivacyConfig;
use thiserror::Error;

/// Errors returned when AI code-editing features are blocked by privacy policy.
///
/// These messages are intended to be surfaced directly to end users (e.g. via
/// LSP error responses) and should remain actionable.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CodeEditPolicyError {
    #[error(
        "AI code edits are disabled in cloud mode unless nova.ai.privacy.allow_cloud_code_edits=true"
    )]
    CloudEditsDisabled,

    #[error(
        "AI code edits are disabled when anonymization is enabled in cloud mode (patches cannot be applied reliably). \
To enable cloud code edits, set nova.ai.privacy.anonymize=false, \
nova.ai.privacy.allow_cloud_code_edits=true, and \
nova.ai.privacy.allow_code_edits_without_anonymization=true (or use nova.ai.privacy.local_only=true)."
    )]
    CloudEditsWithAnonymizationEnabled,

    #[error(
        "AI code edits are disabled in cloud mode unless nova.ai.privacy.allow_code_edits_without_anonymization=true"
    )]
    CloudEditsWithoutAnonymizationDisabled,
}

/// Enforce Nova's privacy policy for AI code-editing operations (patches / file edits).
///
/// Explain-only AI actions should not call this function.
pub fn enforce_code_edit_policy(config: &AiPrivacyConfig) -> Result<(), CodeEditPolicyError> {
    // Local-only mode: code never leaves the machine, so we allow edits without
    // additional gating.
    if config.local_only {
        return Ok(());
    }

    if config.effective_anonymize() {
        // Patches produced against anonymized identifiers cannot reliably apply
        // to the original source. Until Nova has a reversible anonymization
        // pipeline for patches, refuse in this mode.
        return Err(CodeEditPolicyError::CloudEditsWithAnonymizationEnabled);
    }

    if !config.allow_cloud_code_edits {
        return Err(CodeEditPolicyError::CloudEditsDisabled);
    }

    if !config.allow_code_edits_without_anonymization {
        return Err(CodeEditPolicyError::CloudEditsWithoutAnonymizationDisabled);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_only_allows_code_edits_even_when_anonymize_enabled() {
        let cfg = AiPrivacyConfig {
            local_only: true,
            anonymize: Some(true),
            ..AiPrivacyConfig::default()
        };
        assert_eq!(enforce_code_edit_policy(&cfg), Ok(()));
    }

    #[test]
    fn cloud_anonymized_refuses_by_default() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(true),
            ..AiPrivacyConfig::default()
        };
        assert_eq!(
            enforce_code_edit_policy(&cfg),
            Err(CodeEditPolicyError::CloudEditsWithAnonymizationEnabled)
        );
    }

    #[test]
    fn cloud_anonymized_refuses_even_with_cloud_edits_enabled() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(true),
            allow_cloud_code_edits: true,
            ..AiPrivacyConfig::default()
        };
        assert_eq!(
            enforce_code_edit_policy(&cfg),
            Err(CodeEditPolicyError::CloudEditsWithAnonymizationEnabled)
        );
    }

    #[test]
    fn cloud_without_anonymization_requires_explicit_opt_in() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(false),
            allow_cloud_code_edits: true,
            ..AiPrivacyConfig::default()
        };
        assert_eq!(
            enforce_code_edit_policy(&cfg),
            Err(CodeEditPolicyError::CloudEditsWithoutAnonymizationDisabled)
        );
    }

    #[test]
    fn cloud_without_anonymization_allows_when_fully_opted_in() {
        let cfg = AiPrivacyConfig {
            local_only: false,
            anonymize: Some(false),
            allow_cloud_code_edits: true,
            allow_code_edits_without_anonymization: true,
            ..AiPrivacyConfig::default()
        };
        assert_eq!(enforce_code_edit_policy(&cfg), Ok(()));
    }
}
