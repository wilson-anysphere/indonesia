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

    // Spring / JPA analyzers are feature-gated. These crates exist in the
    // repository today but do not currently expose `FrameworkAnalyzer`
    // implementations.
    //
    // Once `nova-framework-spring` / `nova-framework-jpa` provide analyzers, we
    // can register them here behind the corresponding feature flags.
    #[cfg(feature = "spring")]
    {
        let _ = &nova_framework_spring::is_spring_applicable as *const _;
    }

    #[cfg(feature = "jpa")]
    {
        let _ = &nova_framework_jpa::is_jpa_applicable as *const _;
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
