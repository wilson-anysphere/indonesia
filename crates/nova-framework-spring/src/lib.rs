//! Spring framework intelligence for Nova.
//!
//! This crate focuses on the editor-facing "IntelliJ basics" for Spring:
//! - Applicability detection (is this a Spring project?)
//! - Bean discovery (`@Component` stereotypes + `@Configuration/@Bean`)
//! - Autowiring validation diagnostics (missing / ambiguous beans)
//! - Basic circular dependency detection
//! - Completions for `@Qualifier`, `@Profile`, and `@Value`
//! - Best-effort navigation between injection sites and bean definitions

mod analysis;
mod analyzer;
mod applicability;
mod completions;
mod config;
mod poison;

pub use analysis::{
    analyze_java_sources, AnalysisResult, Bean, BeanKind, BeanModel, InjectionKind, InjectionPoint,
    NavigationTarget, SourceDiagnostic, SourceSpan, SPRING_AMBIGUOUS_BEAN, SPRING_CIRCULAR_DEP,
    SPRING_NO_BEAN,
};
pub use analyzer::SpringAnalyzer;
pub use applicability::is_spring_applicable;
pub use completions::{
    profile_completions, property_keys_from_configs, qualifier_completions, value_completions,
};

pub use config::{
    completion_span_for_properties_file, completion_span_for_value_placeholder,
    completion_span_for_yaml_file, completions_for_properties_file,
    completions_for_value_placeholder, completions_for_yaml_file, diagnostics_for_config_file,
    find_references_for_value_placeholder, goto_definition_for_value_placeholder,
    goto_usages_for_config_key, ConfigLocation, SpringWorkspaceIndex, SPRING_CONFIG_TYPE_MISMATCH,
    SPRING_DEPRECATED_CONFIG_KEY, SPRING_DUPLICATE_CONFIG_KEY, SPRING_UNKNOWN_CONFIG_KEY,
};

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};
