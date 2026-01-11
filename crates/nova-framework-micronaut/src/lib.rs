//! Micronaut framework intelligence for Nova.
//!
//! This crate provides best-effort Micronaut support inspired by IntelliJ's
//! baseline framework awareness:
//!
//! - Applicability detection (dependency / classpath scan)
//! - Bean discovery:
//!   - `@Singleton`, `@Prototype`
//!   - `@Factory` + `@Bean` methods
//! - DI wiring with `@Inject` fields/constructors + qualifier filtering
//!   (`@Named` and custom `@Qualifier` annotations defined in source)
//! - Diagnostics:
//!   - missing bean (`MICRONAUT_NO_BEAN`)
//!   - ambiguous beans (`MICRONAUT_AMBIGUOUS_BEAN`)
//!   - circular dependencies (`MICRONAUT_CIRCULAR_DEPENDENCY`, best-effort)
//! - HTTP endpoint discovery:
//!   - `@Controller` base path + mapping annotations (`@Get`, `@Post`, ...)
//! - Config key discovery from `application.yml` / `application.properties`
//!   and simple prefix-based completions for `@Value("${...}")`.

mod applicability;
mod beans;
mod config;
mod endpoints;
mod parse;
mod validation;

pub use applicability::{is_micronaut_applicable, is_micronaut_applicable_with_classpath};
pub use beans::{Bean, BeanKind, InjectionPoint, InjectionResolution, Qualifier};
pub use config::{
    collect_config_keys, completions_for_value_placeholder, config_completions, ConfigFile,
    ConfigFileKind,
};
pub use endpoints::{Endpoint, HandlerLocation};
pub use validation::{
    validation_diagnostics, MICRONAUT_VALIDATION_CONSTRAINT_MISMATCH,
    MICRONAUT_VALIDATION_PRIMITIVE_NONNULL,
};

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};

use nova_framework::{Database, FrameworkAnalyzer, VirtualMember};
use nova_core::ProjectId;
use nova_types::ClassId;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalysisResult {
    pub beans: Vec<Bean>,
    pub injection_resolutions: Vec<InjectionResolution>,
    pub endpoints: Vec<Endpoint>,
    pub diagnostics: Vec<Diagnostic>,
    pub config_keys: Vec<String>,
}

/// In-memory representation of a Java source file for analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JavaSource {
    pub path: String,
    pub text: String,
}

impl JavaSource {
    pub fn new(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
        }
    }
}

/// Analyze a set of Java sources for Micronaut beans/endpoints/diagnostics.
pub fn analyze_sources(sources: &[JavaSource]) -> AnalysisResult {
    analyze_sources_with_config(sources, &[])
}

/// Analyze sources plus configuration files.
pub fn analyze_sources_with_config(sources: &[JavaSource], config_files: &[ConfigFile]) -> AnalysisResult {
    let bean_analysis = beans::analyze_beans(sources);
    let endpoints = endpoints::discover_endpoints(sources);
    let config_keys = collect_config_keys(config_files);

    let mut diagnostics = bean_analysis.diagnostics;
    diagnostics.extend(validation::validation_diagnostics(sources));
    diagnostics.sort_by_key(|d| (d.code, d.span.map(|s| s.start).unwrap_or(0)));

    AnalysisResult {
        beans: bean_analysis.beans,
        injection_resolutions: bean_analysis.injection_resolutions,
        endpoints,
        diagnostics,
        config_keys,
    }
}

/// Convenience helper for fixture tests: analyze sources without file paths.
pub fn analyze_java_sources(sources: &[&str]) -> AnalysisResult {
    let sources: Vec<JavaSource> = sources
        .iter()
        .enumerate()
        .map(|(idx, text)| JavaSource::new(format!("<memory{idx}>"), (*text).to_string()))
        .collect();
    analyze_sources(&sources)
}

/// A minimal `FrameworkAnalyzer` implementation so Micronaut participates in
/// the framework analyzer registry (even though we currently don't synthesize
/// virtual members like Lombok does).
pub struct MicronautAnalyzer;

impl MicronautAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MicronautAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkAnalyzer for MicronautAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Prefer classpath-based detection (covers transitive deps).
        if db.has_class_on_classpath_prefix(project, "io.micronaut.")
            || db.has_class_on_classpath_prefix(project, "io/micronaut/")
        {
            return true;
        }

        // Known Micronaut artifacts.
        const ARTIFACTS: &[&str] = &[
            "micronaut-runtime",
            "micronaut-inject",
            "micronaut-http",
            "micronaut-http-server",
            "micronaut-http-server-netty",
            "micronaut-validation",
        ];

        ARTIFACTS
            .iter()
            .any(|artifact| db.has_dependency(project, "io.micronaut", artifact))
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }
}
