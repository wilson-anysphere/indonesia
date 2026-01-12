//! JDK discovery and standard-library symbol indexing.
//!
//! `JdkIndex::new()` provides a small built-in index used by early resolver tests
//! without requiring a system JDK. For richer semantic analysis, Nova can ingest
//! a real JDK's `.jmod` modules and expose class/member stubs via
//! [`JdkIndex::lookup_type`] and [`JdkIndex::java_lang_symbols`].
//!
//! For `nova-types` unit tests, this crate also exposes [`minimal_jdk`], a tiny
//! semantic class/type model of a few key JDK types.

mod ct_sym;
mod ct_sym_index;
mod discovery;
mod index;
mod jar;
mod jmod;
mod persist;
mod stub;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use nova_cache::CacheConfig;
use nova_core::{
    JdkConfig, Name, PackageName, QualifiedName, StaticMemberId, StaticMemberInfo,
    StaticMemberKind, TypeIndex, TypeName,
};
use nova_modules::{ModuleGraph, ModuleInfo, ModuleName};
use nova_types::{FieldStub, MethodStub, TypeDefStub, TypeProvider};

pub use discovery::{JdkDiscoveryError, JdkInstallation};
pub use index::IndexingStats;
pub use index::JdkIndexError;
pub use stub::{JdkClassStub, JdkFieldStub, JdkMethodStub};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JdkIndexBacking {
    Builtin,
    Jmods,
    CtSym,
    BootJars,
}

impl Default for JdkIndexBacking {
    fn default() -> Self {
        Self::Builtin
    }
}

#[derive(Debug, Clone)]
pub struct JdkIndexInfo {
    /// JDK installation root directory, if known.
    ///
    /// For [`JdkIndexBacking::Builtin`] this is an empty [`PathBuf`].
    pub root: PathBuf,
    pub backing: JdkIndexBacking,
    pub api_release: Option<u16>,
    pub src_zip: Option<PathBuf>,
}

impl Default for JdkIndexInfo {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            backing: JdkIndexBacking::Builtin,
            api_release: None,
            src_zip: None,
        }
    }
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

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
    builtin_binary_names_sorted: Vec<String>,
    builtin_packages_sorted: Vec<String>,
    package_to_types: HashMap<String, HashMap<String, TypeName>>,
    packages: HashSet<String>,
    static_members: HashMap<String, HashMap<String, StaticMemberEntry>>,

    info: JdkIndexInfo,

    // Optional richer symbol index backed by platform containers.
    symbols: Option<index::JdkSymbolIndex>,
}

#[derive(Debug, Clone)]
struct StaticMemberEntry {
    id: StaticMemberId,
    kind: StaticMemberKind,
}

impl JdkIndex {
    /// Construct a small built-in index (no disk IO, no system JDK required).
    pub fn new() -> Self {
        let mut this = Self::default();

        // java.lang
        this.add_type("java.lang", "Object");
        this.add_type("java.lang", "Throwable");
        this.add_type("java.lang", "Class");
        this.add_type("java.lang", "Iterable");
        this.add_type("java.lang", "Runnable");
        this.add_type("java.lang", "String");
        this.add_type("java.lang", "Integer");
        this.add_type("java.lang", "Number");
        this.add_type("java.lang", "Boolean");
        this.add_type("java.lang", "Byte");
        this.add_type("java.lang", "Short");
        this.add_type("java.lang", "Character");
        this.add_type("java.lang", "Long");
        this.add_type("java.lang", "Float");
        this.add_type("java.lang", "Double");
        this.add_type("java.lang", "System");
        this.add_type("java.lang", "Math");
        this.add_type("java.lang", "Cloneable");

        // java.io
        this.add_type("java.io", "Serializable");

        // java.io
        this.add_type("java.io", "PrintStream");

        // java.util
        this.add_type("java.util", "List");
        this.add_type("java.util", "ArrayList");
        this.add_type("java.util", "Collections");
        // Keep a few nested-type examples around so resolver tests can validate
        // `Outer.Inner` → `Outer$Inner` translation without relying on an
        // on-disk JDK index.
        this.add_type("java.util", "Map");
        this.add_type("java.util", "Map$Entry");

        // java.util.function
        this.add_type("java.util.function", "Function");
        this.add_type("java.util.function", "Supplier");
        this.add_type("java.util.function", "Consumer");
        this.add_type("java.util.function", "Predicate");

        // A tiny set of static members for static-import testing.
        this.add_static_member("java.lang.Math", "max", StaticMemberKind::Method);
        this.add_static_member("java.lang.Math", "min", StaticMemberKind::Method);
        this.add_static_member("java.lang.Math", "PI", StaticMemberKind::Field);
        this.add_static_member("java.lang.Math", "E", StaticMemberKind::Field);
        this.add_static_member(
            "java.util.Collections",
            "emptyList",
            StaticMemberKind::Method,
        );
        this.add_static_member(
            "java.util.Collections",
            "singletonList",
            StaticMemberKind::Method,
        );

        // Ensure deterministic ordering for callers that iterate the built-in index.
        this.builtin_binary_names_sorted.sort();
        this.builtin_binary_names_sorted.dedup();
        this.builtin_packages_sorted = this.packages.iter().cloned().collect();
        this.builtin_packages_sorted.sort();
        this.builtin_packages_sorted.dedup();

        this
    }

    /// Approximate heap memory usage of this index in bytes.
    ///
    /// This is intended for best-effort integration with `nova-memory`.
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        let mut bytes = 0u64;

        bytes =
            bytes.saturating_add((self.types.capacity() * size_of::<(String, TypeName)>()) as u64);
        for (k, v) in &self.types {
            bytes = bytes.saturating_add(k.capacity() as u64);
            // `TypeName` does not expose its backing `String` capacity, so we
            // fall back to `len()` for a best-effort approximation.
            bytes = bytes.saturating_add(v.as_str().len() as u64);
        }

        bytes = bytes.saturating_add(
            (self.package_to_types.capacity() * size_of::<(String, HashMap<String, TypeName>)>())
                as u64,
        );
        for (pkg, types) in &self.package_to_types {
            bytes = bytes.saturating_add(pkg.capacity() as u64);
            bytes =
                bytes.saturating_add((types.capacity() * size_of::<(String, TypeName)>()) as u64);
            for (name, ty) in types {
                bytes = bytes.saturating_add(name.capacity() as u64);
                bytes = bytes.saturating_add(ty.as_str().len() as u64);
            }
        }

        bytes = bytes.saturating_add((self.packages.capacity() * size_of::<String>()) as u64);
        for pkg in &self.packages {
            bytes = bytes.saturating_add(pkg.capacity() as u64);
        }

        bytes = bytes
            .saturating_add((self.builtin_packages_sorted.capacity() * size_of::<String>()) as u64);
        for pkg in &self.builtin_packages_sorted {
            bytes = bytes.saturating_add(pkg.capacity() as u64);
        }

        bytes = bytes.saturating_add(
            (self.static_members.capacity()
                * size_of::<(String, HashMap<String, StaticMemberEntry>)>()) as u64,
        );
        for (owner, members) in &self.static_members {
            bytes = bytes.saturating_add(owner.capacity() as u64);
            bytes = bytes.saturating_add(
                (members.capacity() * size_of::<(String, StaticMemberEntry)>()) as u64,
            );
            for (name, member_id) in members {
                bytes = bytes.saturating_add(name.capacity() as u64);
                bytes = bytes.saturating_add(member_id.id.as_str().len() as u64);
            }
        }

        bytes = bytes.saturating_add(self.info.root.as_os_str().len() as u64);
        if let Some(src_zip) = &self.info.src_zip {
            bytes = bytes.saturating_add(src_zip.as_os_str().len() as u64);
        }

        if let Some(symbols) = &self.symbols {
            bytes = bytes.saturating_add(symbols.estimated_bytes());
        }

        bytes
    }

    /// Best-effort drop of large in-memory caches inside the symbol-backed index.
    ///
    /// This keeps the index usable, but may make subsequent lookups slower until caches re-warm.
    pub fn evict_symbol_caches(&self) {
        if let Some(symbols) = &self.symbols {
            symbols.evict_caches();
        }
    }

    /// Build an index backed by a JDK installation's standard-library containers
    /// (`jmods/` on JPMS JDKs, `rt.jar`/`tools.jar` on legacy JDK 8).
    pub fn from_jdk_root(root: impl AsRef<Path>) -> Result<Self, JdkIndexError> {
        let policy = cache_policy_from_env();
        let cache_dir = policy.as_ref().map(|p| p.dir.as_path());
        let allow_write = policy.as_ref().is_some_and(|p| p.allow_write);
        Self::from_jdk_root_with_cache_and_stats_policy(root, cache_dir, allow_write, None)
    }

    /// Discover a JDK installation and build an index backed by its platform
    /// containers.
    pub fn discover(config: Option<&JdkConfig>) -> Result<Self, JdkIndexError> {
        let policy = cache_policy_from_env();
        let cache_dir = policy.as_ref().map(|p| p.dir.as_path());
        let allow_write = policy.as_ref().is_some_and(|p| p.allow_write);
        Self::discover_with_cache_and_stats_policy(config, cache_dir, allow_write, None)
    }

    /// Discover a JDK installation for the requested API release and build an index backed by its
    /// platform containers.
    ///
    /// When `requested_release` is `None`, Nova falls back to `config.release`.
    pub fn discover_for_release(
        config: Option<&JdkConfig>,
        requested_release: Option<u16>,
    ) -> Result<Self, JdkIndexError> {
        let policy = cache_policy_from_env();
        let cache_dir = policy.as_ref().map(|p| p.dir.as_path());
        let allow_write = policy.as_ref().is_some_and(|p| p.allow_write);

        let requested_release = requested_release.filter(|release| *release >= 1);
        let effective_api_release = requested_release.or_else(|| {
            config
                .and_then(|cfg| cfg.release)
                .filter(|release| *release >= 1)
        });

        let install = JdkInstallation::discover_for_release(config, requested_release)?;
        Self::from_installation_with_cache_and_stats_policy(
            install,
            cache_dir,
            allow_write,
            None,
            effective_api_release,
        )
    }

    /// Build an index backed by a JDK installation's platform containers and an optional persisted cache.
    pub fn from_jdk_root_with_cache(
        root: impl AsRef<Path>,
        cache_dir: Option<&Path>,
    ) -> Result<Self, JdkIndexError> {
        Self::from_jdk_root_with_cache_and_stats(root, cache_dir, None)
    }

    /// Build an index backed by a JDK installation's platform containers and an optional persisted cache,
    /// emitting indexing stats as it loads or rebuilds the on-disk cache.
    pub fn from_jdk_root_with_cache_and_stats(
        root: impl AsRef<Path>,
        cache_dir: Option<&Path>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        Self::from_jdk_root_with_cache_and_stats_policy(root, cache_dir, cache_dir.is_some(), stats)
    }

    /// Discover a JDK installation and build an index backed by its platform containers and an optional persisted cache.
    pub fn discover_with_cache(
        config: Option<&JdkConfig>,
        cache_dir: Option<&Path>,
    ) -> Result<Self, JdkIndexError> {
        Self::discover_with_cache_and_stats(config, cache_dir, None)
    }

    /// Discover a JDK installation and build an index backed by its platform containers and an optional persisted cache,
    /// emitting indexing stats as it loads or rebuilds the on-disk cache.
    pub fn discover_with_cache_and_stats(
        config: Option<&JdkConfig>,
        cache_dir: Option<&Path>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        Self::discover_with_cache_and_stats_policy(config, cache_dir, cache_dir.is_some(), stats)
    }

    pub fn info(&self) -> &JdkIndexInfo {
        &self.info
    }

    pub fn src_zip(&self) -> Option<&Path> {
        self.info.src_zip.as_deref()
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

    /// Read the raw `.class` bytes for a type by *internal* name, e.g.
    /// `java/lang/String`.
    ///
    /// This is intended for decompilation / virtual documents. When this index
    /// is not backed by a real JDK symbol index (`symbols` is `None`) the method
    /// returns `Ok(None)`.
    pub fn read_class_bytes(&self, internal_name: &str) -> Result<Option<Vec<u8>>, JdkIndexError> {
        match &self.symbols {
            Some(symbols) => symbols.read_class_bytes(internal_name),
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
            None => Ok(self.builtin_packages_sorted.clone()),
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
                let pkgs = &self.builtin_packages_sorted;

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

    /// All package binary names in this index, in stable sorted order.
    ///
    /// This is intended for bulk iteration without allocating/cloning a `Vec<String>`. For
    /// symbol-backed indexes this may perform lazy container indexing the first time it is called.
    pub fn all_packages(&self) -> Result<&[String], JdkIndexError> {
        match &self.symbols {
            Some(symbols) => symbols.binary_packages(),
            None => Ok(self.builtin_packages_sorted.as_slice()),
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
                let names = &self.builtin_binary_names_sorted;

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

    /// All class binary names present in the *built-in* (dependency-free) index, in stable sorted
    /// order.
    ///
    /// When this `JdkIndex` is backed by a real JDK symbol index (`symbols` is `Some`), callers
    /// should use [`JdkIndex::class_names_with_prefix`] or other symbol-backed APIs instead.
    pub fn binary_class_names(&self) -> Option<&[String]> {
        self.symbols
            .is_none()
            .then_some(self.builtin_binary_names_sorted.as_slice())
    }

    /// All class binary names in this index, in stable sorted order.
    ///
    /// This is intended for bulk iteration (e.g. pre-interning ids into a project-level
    /// `TypeStore`). For symbol-backed indexes this may perform lazy container indexing the first
    /// time it is called.
    pub fn all_binary_class_names(&self) -> Result<&[String], JdkIndexError> {
        match &self.symbols {
            Some(symbols) => symbols.binary_class_names(),
            None => Ok(self.builtin_binary_names_sorted.as_slice()),
        }
    }

    /// Iterate all class binary names in this index, in stable sorted order.
    ///
    /// This is a zero-allocation view over the in-memory sorted name list (but may trigger lazy
    /// indexing for symbol-backed indexes).
    pub fn iter_binary_class_names(
        &self,
    ) -> Result<impl Iterator<Item = &str> + '_, JdkIndexError> {
        Ok(self
            .all_binary_class_names()?
            .iter()
            .map(|name| name.as_str()))
    }

    /// Iterate all class binary names in this index, in stable sorted order.
    ///
    /// This is a convenience alias for [`JdkIndex::iter_binary_class_names`] to mirror the
    /// `nova-classpath` API naming.
    pub fn iter_binary_names(&self) -> Result<impl Iterator<Item = &str> + '_, JdkIndexError> {
        self.iter_binary_class_names()
    }

    /// All static member names (methods + fields) on `owner` that start with `prefix`.
    ///
    /// `owner` should be a binary type name such as `java.lang.Math` (or a `/`-separated internal
    /// name such as `java/lang/Math`, which will be normalized to dotted form).
    ///
    /// In builtin mode (no `symbols`) this is backed by the deterministic `static_members` map so
    /// unit tests do not depend on a system JDK.
    pub fn static_member_names_with_prefix(
        &self,
        owner: &str,
        prefix: &str,
    ) -> Result<Vec<String>, JdkIndexError> {
        let owner = normalize_binary_prefix(owner);

        match &self.symbols {
            None => {
                let mut out = self
                    .static_members
                    .get(owner.as_ref())
                    .map(|m| {
                        m.keys()
                            .filter(|name| name.starts_with(prefix))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                out.sort();
                Ok(out)
            }
            Some(_) => {
                let Some(stub) = self.lookup_type(owner.as_ref())? else {
                    return Ok(Vec::new());
                };

                let mut seen = HashSet::new();
                for field in &stub.fields {
                    if is_static(field.access_flags) && field.name.starts_with(prefix) {
                        seen.insert(field.name.clone());
                    }
                }
                for method in &stub.methods {
                    if is_static(method.access_flags) && method.name.starts_with(prefix) {
                        seen.insert(method.name.clone());
                    }
                }

                let mut out: Vec<String> = seen.into_iter().collect();
                out.sort();
                Ok(out)
            }
        }
    }
    /// Module graph for the underlying JDK, if this index is backed by JMODs or `ct.sym`.
    pub fn module_graph(&self) -> Option<&ModuleGraph> {
        self.symbols
            .as_ref()
            .and_then(|symbols| symbols.module_graph())
    }

    /// Retrieve the parsed JPMS module descriptor for `name` (JMOD / `ct.sym`-backed only).
    pub fn module_info(&self, name: &ModuleName) -> Option<&ModuleInfo> {
        self.symbols.as_ref()?.module_info(name)
    }

    /// Best-effort lookup of the JPMS module that defines `binary_or_internal`.
    ///
    /// Accepts binary names (`java.lang.String`) or internal names (`java/lang/String`).
    /// Returns `None` when this index is not backed by JPMS modules (`jmods/` or `ct.sym`)
    /// or the type cannot be found.
    pub fn module_of_type(&self, binary_or_internal: &str) -> Option<ModuleName> {
        let symbols = self.symbols.as_ref()?;
        symbols.module_of_type(binary_or_internal).ok().flatten()
    }

    fn from_jdk_root_with_cache_and_stats_policy(
        root: impl AsRef<Path>,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let install = JdkInstallation::from_root(root)?;
        Self::from_installation_with_cache_and_stats_policy(
            install,
            cache_dir,
            allow_write,
            stats,
            None,
        )
    }

    fn discover_with_cache_and_stats_policy(
        config: Option<&JdkConfig>,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let install = JdkInstallation::discover(config)?;
        let api_release = config
            .and_then(|cfg| cfg.release)
            .filter(|release| *release >= 1);
        Self::from_installation_with_cache_and_stats_policy(
            install,
            cache_dir,
            allow_write,
            stats,
            api_release,
        )
    }

    fn from_installation_with_cache_and_stats_policy(
        install: JdkInstallation,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
        api_release: Option<u16>,
    ) -> Result<Self, JdkIndexError> {
        let mut this = Self::new();

        let root = canonicalize_best_effort(install.root());

        let symbols = index::JdkSymbolIndex::from_jdk_root_with_cache(
            install.root(),
            cache_dir,
            allow_write,
            stats,
            api_release,
        )?;

        let backing = match &symbols {
            index::JdkSymbolIndex::CtSym(_) => JdkIndexBacking::CtSym,
            index::JdkSymbolIndex::Jmods(_) => {
                if install.jmods_dir().is_some() {
                    JdkIndexBacking::Jmods
                } else {
                    JdkIndexBacking::BootJars
                }
            }
        };

        this.info = JdkIndexInfo {
            root: root.clone(),
            backing,
            api_release,
            src_zip: discovery::src_zip_from_root(&root),
        };

        this.symbols = Some(symbols);
        Ok(this)
    }

    fn add_type(&mut self, package: &str, name: &str) {
        let fq = if package.is_empty() {
            name.to_string()
        } else {
            format!("{package}.{name}")
        };
        let ty = TypeName::new(fq.clone());
        self.builtin_binary_names_sorted.push(fq.clone());
        self.types.insert(fq.clone(), ty.clone());
        self.packages.insert(package.to_string());
        self.package_to_types
            .entry(package.to_string())
            .or_default()
            .insert(name.to_string(), ty);
    }

    fn add_static_member(&mut self, owner: &str, member: &str, kind: StaticMemberKind) {
        self.static_members
            .entry(owner.to_string())
            .or_default()
            .insert(
                member.to_string(),
                StaticMemberEntry {
                    id: StaticMemberId::new(format!("{owner}::{member}")),
                    kind,
                },
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
            let dotted = package.to_dotted();
            if let Ok(pkgs) = symbols.binary_packages() {
                return pkgs
                    .binary_search_by(|pkg| pkg.as_str().cmp(dotted.as_str()))
                    .is_ok();
            }
        }

        self.packages.contains(&package.to_dotted())
    }

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        if let Some(found) = self
            .static_members
            .get(owner.as_str())
            .and_then(|m| m.get(name.as_str()))
        {
            return Some(found.id.clone());
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

    fn static_members(&self, owner: &TypeName) -> Vec<StaticMemberInfo> {
        if let Some(symbols) = &self.symbols {
            if let Ok(Some(stub)) = symbols.lookup_type(owner.as_str()) {
                let mut seen = HashMap::<Name, StaticMemberKind>::new();

                for f in &stub.fields {
                    if is_static(f.access_flags) {
                        seen.insert(Name::from(f.name.as_str()), StaticMemberKind::Field);
                    }
                }

                for m in &stub.methods {
                    if m.name == "<init>" || m.name == "<clinit>" {
                        continue;
                    }
                    if is_static(m.access_flags) {
                        // Prefer fields if a name is both a field and method (rare but possible).
                        seen.entry(Name::from(m.name.as_str()))
                            .or_insert(StaticMemberKind::Method);
                    }
                }

                let mut out: Vec<StaticMemberInfo> = seen
                    .into_iter()
                    .map(|(name, kind)| StaticMemberInfo { name, kind })
                    .collect();
                out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
                if !out.is_empty() {
                    return out;
                }
            }
        }

        let Some(members) = self.static_members.get(owner.as_str()) else {
            return Vec::new();
        };

        let mut out: Vec<StaticMemberInfo> = members
            .iter()
            .map(|(name, entry)| StaticMemberInfo {
                name: Name::from(name.as_str()),
                kind: entry.kind,
            })
            .collect();
        out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        out
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

/// Map a class internal name (e.g. `java/util/Map$Entry`) to its source file
/// entry path in `src.zip` (e.g. `java/util/Map.java`).
///
/// Nested classes (`$Inner`) and anonymous/local classes (`$1`, `$1Local`) are
/// mapped to their outer-most top-level type, since Java sources are organized
/// as one file per top-level type.
pub fn internal_name_to_source_entry_path(internal_name: &str) -> String {
    let (dir, leaf) = match internal_name.rsplit_once('/') {
        Some((dir, leaf)) => (Some(dir), leaf),
        None => (None, internal_name),
    };

    let outer = leaf.split('$').next().unwrap_or(leaf);
    match dir {
        Some(dir) if !dir.is_empty() => format!("{dir}/{outer}.java"),
        _ => format!("{outer}.java"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistenceMode {
    Disabled,
    ReadOnly,
    ReadWrite,
}

impl PersistenceMode {
    fn from_env() -> Self {
        let Some(raw) = std::env::var_os("NOVA_PERSISTENCE") else {
            return Self::default();
        };

        let raw = raw.to_string_lossy();
        let raw = raw.trim().to_ascii_lowercase();
        match raw.as_str() {
            "" => Self::default(),
            "0" | "off" | "disabled" | "false" | "no" => Self::Disabled,
            "ro" | "read-only" | "readonly" => Self::ReadOnly,
            "rw" | "read-write" | "readwrite" | "on" | "enabled" | "true" | "1" => Self::ReadWrite,
            _ => Self::default(),
        }
    }

    fn allows_read(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

impl Default for PersistenceMode {
    fn default() -> Self {
        // Default to RW in release builds, but keep debug/test builds deterministic and free of
        // surprise disk I/O unless explicitly enabled.
        if cfg!(test) || cfg!(debug_assertions) {
            Self::Disabled
        } else {
            Self::ReadWrite
        }
    }
}

#[derive(Clone, Debug)]
struct CachePolicy {
    dir: PathBuf,
    allow_write: bool,
}

fn cache_policy_from_env() -> Option<CachePolicy> {
    if let Some(dir) = std::env::var_os("NOVA_JDK_CACHE_DIR") {
        // Deprecated: prefer `NOVA_CACHE_DIR` (shared global cache root) which feeds the
        // `deps/` cache directory.
        return Some(CachePolicy {
            dir: PathBuf::from(dir),
            allow_write: true,
        });
    }

    let mode = PersistenceMode::from_env();
    if !mode.allows_read() {
        return None;
    }

    let config = CacheConfig::from_env();
    let deps = nova_cache::deps_cache_dir(&config).ok()?;
    Some(CachePolicy {
        dir: deps.join("jdk"),
        allow_write: mode.allows_write(),
    })
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
            methods: vec![
                MethodInfo {
                    name: "toString",
                    type_params: vec![],
                    params: vec![],
                    return_type: TypeRef::Named(STRING, vec![]),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                MethodInfo {
                    name: "equals",
                    type_params: vec![],
                    params: vec![TypeRef::Named(OBJECT, vec![])],
                    return_type: TypeRef::Named("boolean", vec![]),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
                MethodInfo {
                    name: "hashCode",
                    type_params: vec![],
                    params: vec![],
                    return_type: TypeRef::Named("int", vec![]),
                    is_static: false,
                    is_varargs: false,
                    is_abstract: false,
                },
            ],
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
