//! JDK discovery and standard-library symbol indexing.
//!
//! `JdkIndex::new()` provides a small built-in index used by early resolver tests
//! without requiring a system JDK. For richer semantic analysis, Nova can ingest
//! a real JDK's `.jmod` modules and expose class/member stubs via
//! [`JdkIndex::lookup_type`] and [`JdkIndex::java_lang_symbols`].
//!
//! For `nova-types` unit tests, this crate also exposes [`minimal_jdk`], a tiny
//! semantic class/type model of a few key JDK types.

mod discovery;
mod index;
mod jmod;
mod stub;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::borrow::Cow;

use nova_core::{
    Name, PackageName, ProjectConfig, QualifiedName, StaticMemberId, TypeIndex, TypeName,
};
use nova_types::{FieldStub, MethodStub, TypeDefStub, TypeProvider};

pub use discovery::{JdkDiscoveryError, JdkInstallation};
pub use index::JdkIndexError;
pub use stub::{JdkClassStub, JdkFieldStub, JdkMethodStub};

impl From<&JdkFieldStub> for FieldStub {
    fn from(value: &JdkFieldStub) -> Self {
        FieldStub {
            name: value.name.clone(),
            descriptor: value.descriptor.clone(),
            signature: value.signature.clone(),
            access_flags: value.access_flags,
        }
    }
}

impl From<&JdkMethodStub> for MethodStub {
    fn from(value: &JdkMethodStub) -> Self {
        MethodStub {
            name: value.name.clone(),
            descriptor: value.descriptor.clone(),
            signature: value.signature.clone(),
            access_flags: value.access_flags,
        }
    }
}

impl From<&JdkClassStub> for TypeDefStub {
    fn from(value: &JdkClassStub) -> Self {
        TypeDefStub {
            binary_name: value.binary_name.clone(),
            access_flags: value.access_flags,
            super_binary_name: value
                .super_internal_name
                .as_deref()
                .map(crate::stub::internal_to_binary),
            interfaces: value
                .interfaces_internal_names
                .iter()
                .map(|i| crate::stub::internal_to_binary(i))
                .collect(),
            signature: value.signature.clone(),
            fields: value.fields.iter().map(FieldStub::from).collect(),
            methods: value.methods.iter().map(MethodStub::from).collect(),
        }
    }
}
// === Name/type index (used by nova-resolve) ==================================

#[derive(Debug, Default)]
pub struct JdkIndex {
    // Built-in, dependency-free index used for unit tests / bootstrapping.
    types: HashMap<String, TypeName>,
    package_to_types: HashMap<String, HashMap<String, TypeName>>,
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

    /// All packages starting with `prefix` (binary name style, e.g. `java.ut`).
    ///
    /// For convenience this also accepts `/`-separated prefixes (e.g. `java/ut`)
    /// and normalizes them to dotted form.
    pub fn packages_with_prefix(&self, prefix: &str) -> Result<Vec<String>, JdkIndexError> {
        match &self.symbols {
            Some(symbols) => symbols.packages_with_prefix(prefix),
            None => {
                let prefix = normalize_binary_prefix(prefix);
                let mut pkgs: Vec<String> = self.packages.iter().cloned().collect();
                pkgs.sort();

                let start = pkgs.partition_point(|pkg| pkg.as_str() < prefix.as_ref());
                let mut out = Vec::new();
                for pkg in &pkgs[start..] {
                    if pkg.starts_with(prefix.as_ref()) {
                        out.push(pkg.clone());
                    } else {
                        break;
                    }
                }
                Ok(out)
            }
        }
    }

    /// All class binary names starting with `prefix` (e.g. `java.lang.St`).
    ///
    /// This is intended for type-completion/search. It may trigger module
    /// indexing the first time it is called.
    pub fn class_names_with_prefix(&self, prefix: &str) -> Result<Vec<String>, JdkIndexError> {
        match &self.symbols {
            Some(symbols) => symbols.class_names_with_prefix(prefix),
            None => {
                let prefix = normalize_binary_prefix(prefix);
                let mut names: Vec<String> = self.types.keys().cloned().collect();
                names.sort();

                let start = names.partition_point(|name| name.as_str() < prefix.as_ref());
                let mut out = Vec::new();
                for name in &names[start..] {
                    if name.starts_with(prefix.as_ref()) {
                        out.push(name.clone());
                    } else {
                        break;
                    }
                }
                Ok(out)
            }
        }
    }

    fn add_type(&mut self, package: &str, name: &str) {
        let fq = if package.is_empty() {
            name.to_string()
        } else {
            format!("{package}.{name}")
        };
        let ty = TypeName::new(fq.clone());
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
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        if let Some(symbols) = &self.symbols {
            if let Ok(Some(stub)) = symbols.lookup_type(&name.to_dotted()) {
                return Some(TypeName::new(stub.binary_name.clone()));
            }
        }

        self.types.get(&name.to_dotted()).cloned()
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        if let Some(symbols) = &self.symbols {
            let dotted = if package.segments().is_empty() {
                name.as_str().to_string()
            } else {
                format!("{}.{}", package.to_dotted(), name.as_str())
            };

            if let Ok(Some(stub)) = symbols.lookup_type(&dotted) {
                return Some(TypeName::new(stub.binary_name.clone()));
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

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        if let Some(found) = self
            .static_members
            .get(owner.as_str())
            .and_then(|m| m.get(name.as_str()))
            .cloned()
        {
            return Some(found);
        }

        let symbols = self.symbols.as_ref()?;
        let needle = name.as_str();
        let stub = symbols.lookup_type(owner.as_str()).ok().flatten()?;

        let found = stub
            .fields
            .iter()
            .any(|f| f.name == needle && is_static(f.access_flags))
            || stub
                .methods
                .iter()
                .any(|m| m.name == needle && is_static(m.access_flags));

        found.then(|| StaticMemberId::new(format!("{}::{needle}", owner.as_str())))
    }
}

impl TypeProvider for JdkIndex {
    fn lookup_type(&self, binary_name: &str) -> Option<TypeDefStub> {
        let stub = JdkIndex::lookup_type(self, binary_name).ok().flatten()?;
        Some(TypeDefStub::from(stub.as_ref()))
    }
}

fn is_static(access_flags: u16) -> bool {
    const ACC_STATIC: u16 = 0x0008;
    access_flags & ACC_STATIC != 0
}

fn normalize_binary_prefix(prefix: &str) -> Cow<'_, str> {
    if prefix.contains('/') {
        Cow::Owned(prefix.replace('/', "."))
    } else {
        Cow::Borrowed(prefix)
    }
}
// === Minimal class/method/type model (used by nova-types) ====================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassKind {
    Class,
    Interface,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRef {
    /// A named class/interface type with optional type arguments.
    Named(&'static str, Vec<TypeRef>),
    /// A reference to a type parameter in the current scope.
    TypeParam(&'static str),
    Array(Box<TypeRef>),
    WildcardUnbounded,
    WildcardExtends(Box<TypeRef>),
    WildcardSuper(Box<TypeRef>),
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub name: &'static str,
    pub type_params: Vec<&'static str>,
    pub params: Vec<TypeRef>,
    pub return_type: TypeRef,
    pub is_static: bool,
    pub is_varargs: bool,
    pub is_abstract: bool,
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub name: &'static str,
    pub kind: ClassKind,
    pub type_params: Vec<&'static str>,
    pub super_class: Option<TypeRef>,
    pub interfaces: Vec<TypeRef>,
    pub methods: Vec<MethodInfo>,
}

pub mod well_known {
    pub const OBJECT: &str = "java.lang.Object";
    pub const STRING: &str = "java.lang.String";
    pub const INTEGER: &str = "java.lang.Integer";
    pub const CLONEABLE: &str = "java.lang.Cloneable";
    pub const SERIALIZABLE: &str = "java.io.Serializable";

    pub const LIST: &str = "java.util.List";
    pub const ARRAY_LIST: &str = "java.util.ArrayList";

    pub const FUNCTION: &str = "java.util.function.Function";
}

/// A very small, but semantically interesting, subset of the JDK.
pub fn minimal_jdk() -> Vec<ClassInfo> {
    use well_known::*;
    vec![
        ClassInfo {
            name: OBJECT,
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: None,
            interfaces: vec![],
            methods: vec![],
        },
        ClassInfo {
            name: STRING,
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(TypeRef::Named(OBJECT, vec![])),
            interfaces: vec![],
            methods: vec![],
        },
        ClassInfo {
            name: INTEGER,
            kind: ClassKind::Class,
            type_params: vec![],
            super_class: Some(TypeRef::Named(OBJECT, vec![])),
            interfaces: vec![],
            methods: vec![],
        },
        ClassInfo {
            name: CLONEABLE,
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: None,
            interfaces: vec![],
            methods: vec![],
        },
        ClassInfo {
            name: SERIALIZABLE,
            kind: ClassKind::Interface,
            type_params: vec![],
            super_class: None,
            interfaces: vec![],
            methods: vec![],
        },
        // java.util.List<E>
        ClassInfo {
            name: LIST,
            kind: ClassKind::Interface,
            type_params: vec!["E"],
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "get",
                type_params: vec![],
                params: vec![TypeRef::Named("int", vec![])],
                return_type: TypeRef::TypeParam("E"),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            }],
        },
        // java.util.ArrayList<E> implements List<E>
        ClassInfo {
            name: ARRAY_LIST,
            kind: ClassKind::Class,
            type_params: vec!["E"],
            super_class: Some(TypeRef::Named(OBJECT, vec![])),
            interfaces: vec![TypeRef::Named(LIST, vec![TypeRef::TypeParam("E")])],
            methods: vec![],
        },
        // java.util.function.Function<T, R>
        ClassInfo {
            name: FUNCTION,
            kind: ClassKind::Interface,
            type_params: vec!["T", "R"],
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "apply",
                type_params: vec![],
                params: vec![TypeRef::TypeParam("T")],
                return_type: TypeRef::TypeParam("R"),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            }],
        },
    ]
}
