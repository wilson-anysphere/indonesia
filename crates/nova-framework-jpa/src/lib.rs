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
pub use entity::{
    analyze_java_sources, Entity, EntityModel, Field, Relationship, RelationshipKind,
};
pub use jpql::{extract_jpql_strings, jpql_completions, jpql_diagnostics, Token, TokenKind};

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};
