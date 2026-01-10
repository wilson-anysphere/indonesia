//! JPA framework intelligence for Nova.
//!
//! This crate provides a small but useful slice of Jakarta EE / JPA analysis:
//!
//! - Applicability detection (does the project use JPA?)
//! - Entity discovery and a lightweight entity model
//! - Relationship validation diagnostics
//! - A minimal JPQL tokenizer/parser that supports entity/field completion and
//!   basic diagnostics in query strings.

mod applicability;
mod entity;
mod jpql;

pub use applicability::is_jpa_applicable;
pub use applicability::is_jpa_applicable_with_classpath;
pub use entity::{AnalysisResult, Entity, EntityModel, Field, Relationship, RelationshipKind};
pub use jpql::{
    extract_jpql_strings, jpql_completions, jpql_completions_in_java_source, jpql_diagnostics,
    jpql_diagnostics_in_java_source, Token, TokenKind,
};

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};

/// Analyze a set of Java sources for JPA entities + related JPQL diagnostics.
///
/// The returned diagnostics include:
/// - entity/relationship validations (`JPA_*`)
/// - JPQL validations within `@Query(...)` / `@NamedQuery(query=...)` strings (`JPQL_*`)
pub fn analyze_java_sources(sources: &[&str]) -> AnalysisResult {
    let mut result = entity::analyze_entities(sources);
    for src in sources {
        result
            .diagnostics
            .extend(jpql::jpql_diagnostics_in_java_source(src, &result.model));
    }
    result
}
