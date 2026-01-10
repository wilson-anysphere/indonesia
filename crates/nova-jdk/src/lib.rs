//! JDK discovery and standard-library symbol indexing.
//!
//! `JdkIndex::new()` provides a small built-in index used by early resolver tests
//! without requiring a system JDK. For richer semantic analysis, Nova can ingest
//! a real JDK's `.jmod` modules and expose class/member stubs via
//! [`JdkIndex::lookup_type`] and [`JdkIndex::java_lang_symbols`].

mod discovery;
mod index;
mod jmod;
mod stub;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use nova_core::{Name, PackageName, ProjectConfig, QualifiedName, StaticMemberId, TypeId, TypeIndex};

pub use discovery::{JdkDiscoveryError, JdkInstallation};
pub use index::JdkIndexError;
pub use stub::{JdkClassStub, JdkFieldStub, JdkMethodStub};

#[derive(Debug, Default)]
pub struct JdkIndex {
    // Built-in, dependency-free index used for unit tests / bootstrapping.
    types: HashMap<String, TypeId>,
    package_to_types: HashMap<String, HashMap<String, TypeId>>,
    packages: HashSet<String>,
    static_members: HashMap<String, HashMap<String, StaticMemberId>>,

    // Optional richer symbol index backed by JMOD ingestion.
    symbols: Option<index::JdkSymbolIndex>,
}

impl JdkIndex {
    /// Construct a small built-in index (no disk IO, no system JDK required).
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

    /// Build an index backed by a JDK installation's `jmods/` directory.
    pub fn from_jdk_root(root: impl AsRef<Path>) -> Result<Self, JdkIndexError> {
        let mut this = Self::new();
        this.symbols = Some(index::JdkSymbolIndex::from_jdk_root(root)?);
        Ok(this)
    }

    /// Discover a JDK installation and build an index backed by its `jmods/`.
    pub fn discover(config: Option<&ProjectConfig>) -> Result<Self, JdkIndexError> {
        let mut this = Self::new();
        this.symbols = Some(index::JdkSymbolIndex::discover(config)?);
        Ok(this)
    }

    /// Lookup a parsed class stub by binary name (`java.lang.String`), internal
    /// name (`java/lang/String`), or unqualified name (`String`, resolved against
    /// the implicit `java.lang.*` universe scope).
    pub fn lookup_type(&self, name: &str) -> Result<Option<Arc<JdkClassStub>>, JdkIndexError> {
        match &self.symbols {
            Some(symbols) => symbols.lookup_type(name),
            None => Ok(None),
        }
    }

    /// All types in the implicit `java.lang.*` universe scope.
    pub fn java_lang_symbols(&self) -> Result<Vec<Arc<JdkClassStub>>, JdkIndexError> {
        match &self.symbols {
            Some(symbols) => symbols.java_lang_symbols(),
            None => Ok(Vec::new()),
        }
    }

    /// All packages present in the JDK module set.
    pub fn packages(&self) -> Result<Vec<String>, JdkIndexError> {
        match &self.symbols {
            Some(symbols) => symbols.packages(),
            None => Ok(self.packages.iter().cloned().collect()),
        }
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
        if let Some(symbols) = &self.symbols {
            if let Ok(Some(stub)) = symbols.lookup_type(&name.to_dotted()) {
                return Some(TypeId::new(stub.binary_name.clone()));
            }
        }

        self.types.get(&name.to_dotted()).cloned()
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeId> {
        if let Some(symbols) = &self.symbols {
            let dotted = if package.segments().is_empty() {
                name.as_str().to_string()
            } else {
                format!("{}.{}", package.to_dotted(), name.as_str())
            };

            if let Ok(Some(stub)) = symbols.lookup_type(&dotted) {
                return Some(TypeId::new(stub.binary_name.clone()));
            }
        }

        let pkg = package.to_dotted();
        self.package_to_types
            .get(&pkg)
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        if let Some(symbols) = &self.symbols {
            if let Ok(pkgs) = symbols.packages() {
                if pkgs.contains(&package.to_dotted()) {
                    return true;
                }
            }
        }

        self.packages.contains(&package.to_dotted())
    }

    fn resolve_static_member(&self, owner: &TypeId, name: &Name) -> Option<StaticMemberId> {
        self.static_members
            .get(owner.as_str())
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }
}

