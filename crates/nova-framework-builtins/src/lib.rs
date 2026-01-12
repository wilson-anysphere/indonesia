//! Nova framework analyzer built-ins.
//!
//! This crate centralizes construction/registration of Nova's built-in
//! `nova-framework-*` analyzers so downstream crates (IDE, LSP, etc.) don't need
//! to maintain their own lists.

use nova_framework::{AnalyzerRegistry, FrameworkAnalyzer};

/// Descriptor for a built-in framework analyzer with a stable, namespaced id.
///
/// The id is intended to be used as the `nova-ext` provider id when registering the analyzer as an
/// individual provider (per-analyzer timeouts/metrics/circuit breakers).
pub struct BuiltinAnalyzerDescriptor {
    pub id: &'static str,
    pub analyzer: Box<dyn FrameworkAnalyzer>,
}

/// Construct the built-in framework analyzers with stable ids.
pub fn builtin_analyzers_with_ids() -> Vec<BuiltinAnalyzerDescriptor> {
    let mut analyzers: Vec<BuiltinAnalyzerDescriptor> = Vec::new();

    analyzers.push(BuiltinAnalyzerDescriptor {
        id: "nova.framework.lombok",
        analyzer: Box::new(nova_framework_lombok::LombokAnalyzer::new()),
    });
    analyzers.push(BuiltinAnalyzerDescriptor {
        id: "nova.framework.dagger",
        analyzer: Box::new(nova_framework_dagger::DaggerAnalyzer::default()),
    });
    analyzers.push(BuiltinAnalyzerDescriptor {
        id: "nova.framework.mapstruct",
        analyzer: Box::new(nova_framework_mapstruct::MapStructAnalyzer::new()),
    });
    analyzers.push(BuiltinAnalyzerDescriptor {
        id: "nova.framework.micronaut",
        analyzer: Box::new(nova_framework_micronaut::MicronautAnalyzer::new()),
    });
    analyzers.push(BuiltinAnalyzerDescriptor {
        id: "nova.framework.quarkus",
        analyzer: Box::new(nova_framework_quarkus::QuarkusAnalyzer::new()),
    });

    // Spring / JPA analyzers are feature-gated: they can be relatively expensive
    // and/or pull in heavier dependencies.
    #[cfg(feature = "spring")]
    {
        analyzers.push(BuiltinAnalyzerDescriptor {
            id: "nova.framework.spring",
            analyzer: Box::new(nova_framework_spring::SpringAnalyzer::new()),
        });
    }

    #[cfg(feature = "jpa")]
    {
        analyzers.push(BuiltinAnalyzerDescriptor {
            id: "nova.framework.jpa",
            analyzer: Box::new(nova_framework_jpa::JpaAnalyzer::new()),
        });
    }

    analyzers
}

/// Construct the built-in framework analyzers.
///
/// This returns boxed trait objects so callers can introspect and/or register
/// analyzers with a [`nova_framework::AnalyzerRegistry`].
pub fn builtin_analyzers() -> Vec<Box<dyn FrameworkAnalyzer>> {
    builtin_analyzers_with_ids()
        .into_iter()
        .map(|desc| desc.analyzer)
        .collect()
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
