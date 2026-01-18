//! JPA framework intelligence for Nova.
//!
//! This crate provides a small but useful slice of Jakarta EE / JPA analysis:
//!
//! - Applicability detection (does the project use JPA?)
//! - Entity discovery and a lightweight entity model
//! - Relationship validation diagnostics
//! - A minimal JPQL tokenizer/parser that supports entity/field completion and
//!   basic diagnostics in query strings.

mod analyzer;
mod applicability;
mod entity;
mod jpql;
mod poison;

pub use analyzer::JpaAnalyzer;
pub use applicability::is_jpa_applicable;
pub use applicability::is_jpa_applicable_with_classpath;
pub use entity::{
    AnalysisResult, Entity, EntityModel, Field, Relationship, RelationshipKind, SourceDiagnostic,
    JPA_MAPPEDBY_MISSING, JPA_MAPPEDBY_NOT_RELATIONSHIP, JPA_MAPPEDBY_WRONG_TARGET, JPA_MISSING_ID,
    JPA_NO_NOARG_CTOR, JPA_PARSE_ERROR, JPA_REL_INVALID_TARGET_TYPE, JPA_REL_TARGET_NOT_ENTITY,
    JPA_REL_TARGET_UNKNOWN,
};
pub use jpql::{
    extract_jpql_strings, jpql_completions, jpql_completions_in_java_source, jpql_diagnostics,
    jpql_diagnostics_in_java_source, tokenize_jpql, Token, TokenKind, JPQL_UNKNOWN_ALIAS,
    JPQL_UNKNOWN_ENTITY, JPQL_UNKNOWN_FIELD,
};

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};

/// Analyze a set of Java sources for JPA entities + related JPQL diagnostics.
///
/// The returned diagnostics include:
/// - entity/relationship validations (`JPA_*`)
/// - JPQL validations within `@Query(...)` / `@NamedQuery(query=...)` strings (`JPQL_*`)
pub fn analyze_java_sources(sources: &[&str]) -> AnalysisResult {
    let mut result = entity::analyze_entities(sources);
    for (source_idx, src) in sources.iter().enumerate() {
        for diagnostic in jpql::jpql_diagnostics_in_java_source(src, &result.model) {
            result.diagnostics.push(SourceDiagnostic {
                source: source_idx,
                diagnostic,
            });
        }
    }
    result
}
