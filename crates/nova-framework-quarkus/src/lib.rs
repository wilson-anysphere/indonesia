//! Quarkus framework intelligence for Nova.
//!
//! This crate focuses on Quarkus' "everyday" developer ergonomics:
//! - CDI bean discovery and injection diagnostics
//! - REST endpoint discovery (via shared `nova-framework-web` JAX-RS extractor)
//! - Config property collection + completion helpers

mod applicability;
mod cdi;
mod config;

pub use applicability::{
    is_quarkus_applicable, is_quarkus_applicable_with_classpath, is_quarkus_applicable_with_db,
};
pub use cdi::{CdiAnalysis, CdiModel};
pub use cdi::{CDI_AMBIGUOUS_CODE, CDI_CIRCULAR_CODE, CDI_UNSATISFIED_CODE};
pub use config::{collect_config_property_names, config_property_completions};

use nova_core::ProjectId;
use nova_framework::{Database, FrameworkAnalyzer, VirtualMember};
use nova_types::ClassId;

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};

/// Framework analyzer hook used by Nova's resolver for "virtual member" generation.
///
/// Quarkus itself doesn't generate source-level members in the way Lombok does,
/// but we still register an analyzer so Nova can detect that a project is Quarkus
/// based on dependencies/classpath markers.
pub struct QuarkusAnalyzer;

impl QuarkusAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for QuarkusAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkAnalyzer for QuarkusAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        is_quarkus_applicable_with_db(db, project)
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }
}

#[derive(Debug, Clone)]
pub struct AnalysisResult {
    pub cdi: CdiModel,
    pub diagnostics: Vec<Diagnostic>,
    pub endpoints: Vec<nova_framework_web::Endpoint>,
    pub config_properties: Vec<String>,
}

/// Analyze a set of Java sources for Quarkus-relevant framework features.
pub fn analyze_java_sources(sources: &[&str]) -> AnalysisResult {
    let CdiAnalysis { model, diagnostics } = cdi::analyze_cdi(sources);

    let endpoints = nova_framework_web::extract_http_endpoints_from_sources(sources);
    let config_properties = config::collect_config_property_names(sources, &[]);

    AnalysisResult {
        cdi: model,
        diagnostics,
        endpoints,
        config_properties,
    }
}
