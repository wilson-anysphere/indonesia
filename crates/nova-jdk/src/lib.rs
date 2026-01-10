//! Minimal JDK symbol index for Nova.
//!
//! The real implementation will likely read from the configured JDK and build
//! an index of types/packages/members. For now we hardcode a small subset used
//! by `nova-resolve` tests and early IDE features.

use std::collections::{HashMap, HashSet};

use nova_core::{Name, PackageName, QualifiedName, StaticMemberId, TypeId, TypeIndex};

#[derive(Debug, Default)]
pub struct JdkIndex {
    types: HashMap<String, TypeId>,
    package_to_types: HashMap<String, HashMap<String, TypeId>>,
    packages: HashSet<String>,
    static_members: HashMap<String, HashMap<String, StaticMemberId>>,
}

impl JdkIndex {
    pub fn new() -> Self {
        let mut this = Self::default();

        // java.lang
        this.add_type("java.lang", "Object");
        this.add_type("java.lang", "String");
        this.add_type("java.lang", "Integer");
        this.add_type("java.lang", "System");
        this.add_type("java.lang", "Math");

        // java.util
        this.add_type("java.util", "List");
        this.add_type("java.util", "ArrayList");

        // A tiny set of static members for static-import testing.
        this.add_static_member("java.lang.Math", "max");
        this.add_static_member("java.lang.Math", "PI");

        this
    }

    fn add_type(&mut self, package: &str, name: &str) {
        let fq = if package.is_empty() {
            name.to_string()
        } else {
            format!("{package}.{name}")
        };
        let ty = TypeId::new(fq.clone());
        self.types.insert(fq.clone(), ty.clone());
        self.packages.insert(package.to_string());
        self.package_to_types
            .entry(package.to_string())
            .or_default()
            .insert(name.to_string(), ty);
    }

    fn add_static_member(&mut self, owner: &str, member: &str) {
        self.static_members
            .entry(owner.to_string())
            .or_default()
            .insert(
                member.to_string(),
                StaticMemberId::new(format!("{owner}::{member}")),
            );
    }
}

impl TypeIndex for JdkIndex {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeId> {
        self.types.get(&name.to_dotted()).cloned()
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeId> {
        let pkg = package.to_dotted();
        self.package_to_types
            .get(&pkg)
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.packages.contains(&package.to_dotted())
    }

    fn resolve_static_member(&self, owner: &TypeId, name: &Name) -> Option<StaticMemberId> {
        self.static_members
            .get(owner.as_str())
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }
}
