//! Nova framework analyzer built-ins.
//!
//! This crate centralizes construction/registration of Nova's built-in
//! `nova-framework-*` analyzers so downstream crates (IDE, LSP, etc.) don't need
//! to maintain their own lists.

use nova_framework::{AnalyzerRegistry, FrameworkAnalyzer};

/// Construct the built-in framework analyzers.
///
/// This returns boxed trait objects so callers can introspect and/or register
/// analyzers with a [`nova_framework::AnalyzerRegistry`].
pub fn builtin_analyzers() -> Vec<Box<dyn FrameworkAnalyzer>> {
    let mut analyzers: Vec<Box<dyn FrameworkAnalyzer>> = Vec::new();

    analyzers.push(Box::new(nova_framework_lombok::LombokAnalyzer::new()));
    analyzers.push(Box::new(nova_framework_dagger::DaggerAnalyzer::default()));
    analyzers.push(Box::new(nova_framework_mapstruct::MapStructAnalyzer::new()));
    analyzers.push(Box::new(nova_framework_micronaut::MicronautAnalyzer::new()));
    analyzers.push(Box::new(nova_framework_quarkus::QuarkusAnalyzer::new()));

    // Spring / JPA analyzers are feature-gated: they can be relatively expensive
    // and/or pull in heavier dependencies.
    #[cfg(feature = "spring")]
    {
        analyzers.push(Box::new(nova_framework_spring::SpringAnalyzer::new()));
    }

    #[cfg(feature = "jpa")]
    {
        analyzers.push(Box::new(nova_framework_jpa::JpaAnalyzer::new()));
    }

    analyzers
}

/// Register Nova's built-in framework analyzers into an existing registry.
pub fn register_builtin_analyzers(registry: &mut AnalyzerRegistry) {
    for analyzer in builtin_analyzers() {
        registry.register(analyzer);
    }
}

/// Construct an [`AnalyzerRegistry`] with all built-in analyzers registered.
pub fn builtin_registry() -> AnalyzerRegistry {
    let mut registry = AnalyzerRegistry::new();
    register_builtin_analyzers(&mut registry);
    registry
}
