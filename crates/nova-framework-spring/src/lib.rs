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
mod applicability;
mod completions;

pub use analysis::{
    analyze_java_sources, AnalysisResult, Bean, BeanKind, BeanModel, InjectionKind, InjectionPoint,
    NavigationTarget, SourceDiagnostic, SourceSpan, SPRING_AMBIGUOUS_BEAN, SPRING_CIRCULAR_DEP,
    SPRING_NO_BEAN,
};
pub use applicability::is_spring_applicable;
pub use completions::{
    profile_completions, property_keys_from_configs, qualifier_completions, value_completions,
};

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};
