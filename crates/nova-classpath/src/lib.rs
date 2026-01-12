mod module_name;
mod persist;

use std::borrow::Cow;
use std::collections::{hash_map::DefaultHasher, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use nova_classfile::{parse_module_info_class, ClassFile};
use nova_core::{Name, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_deps_cache::{
    DependencyIndexBundle, DependencyIndexStore, DepsClassStub, DepsFieldStub, DepsMethodStub,
};
use nova_modules::{ModuleInfo, ModuleName};
use nova_types::{FieldStub, MethodStub, TypeDefStub, TypeProvider};

/// Derive the automatic module name for a module-path entry based on its path.
///
/// This follows the same derivation the Java module system uses for automatic
/// modules (see `java.lang.module.ModuleFinder`) and matches the logic used
/// internally by [`ClasspathEntry::module_meta`].
///
/// This is primarily useful when callers treat a directory as a module-path
/// entry (i.e. an automatic module). `ClasspathEntry::module_meta` returns
/// `None` for directories because class directories are treated as belonging to
/// the classpath "unnamed module" by default.
pub fn derive_automatic_module_name_from_path(path: &Path) -> Option<ModuleName> {
    // Prefer filesystem metadata when available (handles directory entries that
    // happen to end with `.jar`, etc). When the path does not exist yet, fall
    // back to inspecting the extension so dotted directory names like
    // `com.example.app` do not get truncated by `Path::file_stem()`.
    if path.is_dir() {
        let stem = path.file_name()?.to_string_lossy();
        return module_name::derive_automatic_module_name_from_jar_stem(&stem).map(ModuleName::new);
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_archive = ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod");
    if is_archive {
        module_name::derive_automatic_module_name_from_jar_path(path).map(ModuleName::new)
    } else {
        let stem = path.file_name()?.to_string_lossy();
        module_name::derive_automatic_module_name_from_jar_stem(&stem).map(ModuleName::new)
    }
}

#[derive(Debug, Error)]
pub enum ClasspathError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("classfile error: {0}")]
    ClassFile(#[from] nova_classfile::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleNameKind {
    Explicit,
    Automatic,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryModuleMeta {
    pub name: Option<ModuleName>,
    pub kind: ModuleNameKind,
}

/// Optional indexing counters used by tests and the CLI.
#[derive(Debug, Default)]
pub struct IndexingStats {
    classfiles_parsed: AtomicUsize,
    deps_cache_hits: AtomicUsize,
}

/// Options that control how classpath entries are indexed.
///
/// When `target_release` is set (e.g. the project's `--release` value),
/// multi-release JARs (`META-INF/versions/<n>/...`) are resolved according to
/// that release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexOptions {
    /// Java feature release number (8, 11, 17, ...).
    ///
    /// When `None`, Nova preserves the legacy conservative behavior where base
    /// entries always win and multi-release variants are only used when the base
    /// class is missing.
    pub target_release: Option<u16>,
}

impl IndexingStats {
    pub fn classfiles_parsed(&self) -> usize {
        self.classfiles_parsed.load(Ordering::Relaxed)
    }

    pub fn deps_cache_hits(&self) -> usize {
        self.deps_cache_hits.load(Ordering::Relaxed)
    }
}

fn record_parsed(stats: Option<&IndexingStats>) {
    if let Some(stats) = stats {
        stats.classfiles_parsed.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_deps_cache_hit(stats: Option<&IndexingStats>) {
    if let Some(stats) = stats {
        stats.deps_cache_hits.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ClasspathFingerprint(u64);

impl ClasspathFingerprint {
    pub fn to_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClasspathEntry {
    ClassDir(PathBuf),
    Jar(PathBuf),
    Jmod(PathBuf),
}

impl From<&nova_build_model::ClasspathEntry> for ClasspathEntry {
    fn from(value: &nova_build_model::ClasspathEntry) -> Self {
        match value.kind {
            nova_build_model::ClasspathEntryKind::Directory => {
                ClasspathEntry::ClassDir(value.path.clone())
            }
            nova_build_model::ClasspathEntryKind::Jar => ClasspathEntry::Jar(value.path.clone()),
        }
    }
}

impl ClasspathEntry {
    pub fn normalize(&self) -> std::io::Result<Self> {
        Ok(match self {
            ClasspathEntry::ClassDir(p) => ClasspathEntry::ClassDir(canonicalize_if_possible(p)?),
            ClasspathEntry::Jar(p) => ClasspathEntry::Jar(canonicalize_if_possible(p)?),
            ClasspathEntry::Jmod(p) => ClasspathEntry::Jmod(canonicalize_if_possible(p)?),
        })
    }

    pub fn fingerprint(&self) -> std::io::Result<ClasspathFingerprint> {
        match self {
            ClasspathEntry::ClassDir(dir) => fingerprint_class_dir(dir),
            ClasspathEntry::Jar(path) | ClasspathEntry::Jmod(path) => fingerprint_file(path),
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            ClasspathEntry::ClassDir(p) | ClasspathEntry::Jar(p) | ClasspathEntry::Jmod(p) => p,
        }
    }

    /// Best-effort JPMS module descriptor discovery for this entry.
    ///
    /// Returns `Ok(None)` if the entry does not contain a `module-info.class`.
    pub fn module_info(&self) -> Result<Option<ModuleInfo>, ClasspathError> {
        match self {
            ClasspathEntry::ClassDir(dir) => read_module_info_from_dir(dir),
            ClasspathEntry::Jar(path) => read_module_info_from_zip(path, ZipKind::Jar),
            ClasspathEntry::Jmod(path) => read_module_info_from_zip(path, ZipKind::Jmod),
        }
    }
    pub fn module_meta(&self) -> Result<EntryModuleMeta, ClasspathError> {
        match self {
            ClasspathEntry::ClassDir(_) => Ok(EntryModuleMeta {
                name: None,
                kind: ModuleNameKind::None,
            }),
            ClasspathEntry::Jar(path) => jar_module_meta(path),
            ClasspathEntry::Jmod(path) => match self.module_info()? {
                Some(info) => Ok(EntryModuleMeta {
                    name: Some(info.name),
                    kind: ModuleNameKind::Explicit,
                }),
                None => {
                    // Best-effort: allow missing `.jmod` paths (e.g. user overrides) to still be
                    // treated as automatic modules, derived from the filename.
                    let name = derive_automatic_module_name_from_path(path);
                    let kind = if name.is_some() {
                        ModuleNameKind::Automatic
                    } else {
                        ModuleNameKind::None
                    };
                    Ok(EntryModuleMeta { name, kind })
                }
            },
        }
    }

    /// JPMS module metadata when this entry is treated as a `--module-path` item.
    ///
    /// Unlike [`ClasspathEntry::module_meta`], class directories are treated as
    /// modules on the module path:
    /// - if they contain `module-info.class`, they are explicit modules
    /// - otherwise they are treated as automatic modules with a derived name
    ///
    /// This is useful for JPMS-aware resolution, where class directories can
    /// represent exploded modules (e.g. another module's output directory).
    pub fn module_meta_for_module_path(&self) -> Result<EntryModuleMeta, ClasspathError> {
        match self {
            ClasspathEntry::ClassDir(dir) => match self.module_info()? {
                Some(info) => Ok(EntryModuleMeta {
                    name: Some(info.name),
                    kind: ModuleNameKind::Explicit,
                }),
                None => {
                    let name = derive_automatic_module_name_from_path(dir);
                    let kind = if name.is_some() {
                        ModuleNameKind::Automatic
                    } else {
                        ModuleNameKind::None
                    };
                    Ok(EntryModuleMeta { name, kind })
                }
            },
            _ => self.module_meta(),
        }
    }
}

fn jar_module_meta(path: &Path) -> Result<EntryModuleMeta, ClasspathError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Best-effort: JPMS tooling can still assign an automatic module name based on the jar
            // filename even when the archive hasn't been downloaded yet.
            let name =
                module_name::derive_automatic_module_name_from_jar_path(path).map(ModuleName::new);
            let kind = if name.is_some() {
                ModuleNameKind::Automatic
            } else {
                ModuleNameKind::None
            };
            return Ok(EntryModuleMeta { name, kind });
        }
        Err(err) => return Err(err.into()),
    };
    let mut archive = zip::ZipArchive::new(file)?;

    for candidate in ["module-info.class", "META-INF/versions/9/module-info.class"] {
        match archive.by_name(candidate) {
            Ok(mut entry) => {
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut bytes)?;
                let info = parse_module_info_class(&bytes)?;
                return Ok(EntryModuleMeta {
                    name: Some(info.name),
                    kind: ModuleNameKind::Explicit,
                });
            }
            Err(zip::result::ZipError::FileNotFound) => continue,
            Err(err) => return Err(err.into()),
        }
    }

    let name = module_name::automatic_module_name_from_jar_manifest(&mut archive).or_else(|| {
        module_name::derive_automatic_module_name_from_jar_path(path).map(ModuleName::new)
    });
    let kind = if name.is_some() {
        ModuleNameKind::Automatic
    } else {
        ModuleNameKind::None
    };

    Ok(EntryModuleMeta { name, kind })
}

fn canonicalize_if_possible(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(e) => Err(e),
    }
}

fn fingerprint_file(path: &Path) -> std::io::Result<ClasspathFingerprint> {
    let meta = std::fs::metadata(path)?;
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    meta.len().hash(&mut hasher);
    hash_mtime(&mut hasher, &meta.modified()?);
    Ok(ClasspathFingerprint(hasher.finish()))
}

fn fingerprint_class_dir(dir: &Path) -> std::io::Result<ClasspathFingerprint> {
    let mut class_files: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension() != Some(OsStr::new("class")) {
            continue;
        }
        class_files.push(entry.into_path());
    }
    class_files.sort();

    let mut hasher = DefaultHasher::new();
    dir.to_string_lossy().hash(&mut hasher);

    for file in class_files {
        let rel = file.strip_prefix(dir).unwrap_or(&file);
        let meta = std::fs::metadata(&file)?;
        rel.to_string_lossy().hash(&mut hasher);
        meta.len().hash(&mut hasher);
        hash_mtime(&mut hasher, &meta.modified()?);
    }

    Ok(ClasspathFingerprint(hasher.finish()))
}

fn hash_mtime(hasher: &mut DefaultHasher, time: &SystemTime) {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
    duration.as_secs().hash(hasher);
    duration.subsec_nanos().hash(hasher);
}

fn internal_name_to_binary(internal: &str) -> String {
    internal.replace('/', ".")
}

fn is_ignored_class(internal_name: &str) -> bool {
    internal_name == "module-info"
        || internal_name == "package-info"
        || internal_name.ends_with("/package-info")
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ClasspathFieldStub {
    pub name: String,
    pub descriptor: String,
    pub signature: Option<String>,
    pub access_flags: u16,
    pub annotations: Vec<String>,
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ClasspathMethodStub {
    pub name: String,
    pub descriptor: String,
    pub signature: Option<String>,
    pub access_flags: u16,
    pub annotations: Vec<String>,
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ClasspathClassStub {
    pub binary_name: String,
    pub internal_name: String,
    pub access_flags: u16,
    pub super_binary_name: Option<String>,
    pub interfaces: Vec<String>,
    pub signature: Option<String>,
    pub annotations: Vec<String>,
    pub fields: Vec<ClasspathFieldStub>,
    pub methods: Vec<ClasspathMethodStub>,
}

impl From<&ClasspathFieldStub> for FieldStub {
    fn from(value: &ClasspathFieldStub) -> Self {
        FieldStub {
            name: value.name.clone(),
            descriptor: value.descriptor.clone(),
            signature: value.signature.clone(),
            access_flags: value.access_flags,
        }
    }
}

impl From<&ClasspathMethodStub> for MethodStub {
    fn from(value: &ClasspathMethodStub) -> Self {
        MethodStub {
            name: value.name.clone(),
            descriptor: value.descriptor.clone(),
            signature: value.signature.clone(),
            access_flags: value.access_flags,
        }
    }
}

impl From<&ClasspathClassStub> for TypeDefStub {
    fn from(value: &ClasspathClassStub) -> Self {
        TypeDefStub {
            binary_name: value.binary_name.clone(),
            access_flags: value.access_flags,
            super_binary_name: value.super_binary_name.clone(),
            interfaces: value.interfaces.clone(),
            signature: value.signature.clone(),
            fields: value.fields.iter().map(FieldStub::from).collect(),
            methods: value.methods.iter().map(MethodStub::from).collect(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ClasspathIndex {
    stubs_by_binary: HashMap<String, ClasspathClassStub>,
    binary_names_sorted: Vec<String>,
    packages_sorted: Vec<String>,
    internal_to_binary: HashMap<String, String>,
}

impl ClasspathIndex {
    pub fn build(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
    ) -> Result<Self, ClasspathError> {
        Self::build_with_options(entries, cache_dir, IndexOptions::default())
    }

    pub fn build_with_options(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let deps_store = DependencyIndexStore::from_env().ok();
        Self::build_with_deps_store_and_options(
            entries,
            cache_dir,
            deps_store.as_ref(),
            None,
            options,
        )
    }

    pub fn build_with_deps_store(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, ClasspathError> {
        Self::build_with_deps_store_and_options(
            entries,
            cache_dir,
            deps_store,
            stats,
            IndexOptions::default(),
        )
    }

    pub fn build_with_deps_store_and_options(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();

        for entry in entries {
            let entry = entry.normalize()?;
            let stubs = match &entry {
                ClasspathEntry::ClassDir(_) => {
                    let fingerprint = entry.fingerprint()?;
                    if let Some(cache_dir) = cache_dir {
                        persist::load_or_build_entry(
                            cache_dir,
                            &entry,
                            fingerprint,
                            options.target_release,
                            || index_entry(&entry, deps_store, stats, options),
                        )?
                    } else {
                        index_entry(&entry, deps_store, stats, options)?
                    }
                }
                ClasspathEntry::Jar(_) | ClasspathEntry::Jmod(_) => {
                    index_entry(&entry, deps_store, stats, options)?
                }
            };

            for stub in stubs {
                if stubs_by_binary.contains_key(&stub.binary_name) {
                    continue;
                }
                internal_to_binary
                    .entry(stub.internal_name.clone())
                    .or_insert_with(|| stub.binary_name.clone());
                stubs_by_binary.insert(stub.binary_name.clone(), stub);
            }
        }

        let mut binary_names_sorted: Vec<String> = stubs_by_binary.keys().cloned().collect();
        binary_names_sorted.sort();

        let mut packages: BTreeSet<String> = BTreeSet::new();
        for name in &binary_names_sorted {
            if let Some((pkg, _)) = name.rsplit_once('.') {
                packages.insert(pkg.to_owned());
            }
        }

        Ok(Self {
            stubs_by_binary,
            binary_names_sorted,
            packages_sorted: packages.into_iter().collect(),
            internal_to_binary,
        })
    }

    pub fn len(&self) -> usize {
        self.stubs_by_binary.len()
    }

    /// All indexed class binary names (`java.lang.String`, `com.example.Foo`, ...) in sorted order.
    ///
    /// This is intended for deterministic pre-interning in `TypeStore` so `ClassId` allocation is
    /// stable across clones/snapshots.
    pub fn binary_names_sorted(&self) -> &[String] {
        &self.binary_names_sorted
    }

    /// Iterate all indexed class binary names (`java.lang.String`, `com.example.Foo`, ...) in
    /// **stable sorted order**.
    ///
    /// This is a convenience wrapper around [`Self::iter_binary_class_names`].
    pub fn iter_binary_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.iter_binary_class_names()
    }

    /// Approximate heap memory usage of this index in bytes.
    ///
    /// This is intended for best-effort integration with `nova-memory`.
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        fn add_string(bytes: &mut u64, s: &String) {
            *bytes = bytes.saturating_add(s.capacity() as u64);
        }

        fn add_opt_string(bytes: &mut u64, s: &Option<String>) {
            if let Some(s) = s {
                add_string(bytes, s);
            }
        }

        fn add_vec_string(bytes: &mut u64, v: &Vec<String>) {
            *bytes = bytes.saturating_add((v.capacity() * size_of::<String>()) as u64);
            for s in v {
                add_string(bytes, s);
            }
        }

        fn add_field_stub(bytes: &mut u64, stub: &ClasspathFieldStub) {
            add_string(bytes, &stub.name);
            add_string(bytes, &stub.descriptor);
            add_opt_string(bytes, &stub.signature);
            add_vec_string(bytes, &stub.annotations);
        }

        fn add_method_stub(bytes: &mut u64, stub: &ClasspathMethodStub) {
            add_string(bytes, &stub.name);
            add_string(bytes, &stub.descriptor);
            add_opt_string(bytes, &stub.signature);
            add_vec_string(bytes, &stub.annotations);
        }

        fn add_class_stub(bytes: &mut u64, stub: &ClasspathClassStub) {
            add_string(bytes, &stub.binary_name);
            add_string(bytes, &stub.internal_name);
            add_opt_string(bytes, &stub.super_binary_name);
            add_vec_string(bytes, &stub.interfaces);
            add_opt_string(bytes, &stub.signature);
            add_vec_string(bytes, &stub.annotations);

            *bytes = bytes
                .saturating_add((stub.fields.capacity() * size_of::<ClasspathFieldStub>()) as u64);
            for field in &stub.fields {
                add_field_stub(bytes, field);
            }

            *bytes = bytes.saturating_add(
                (stub.methods.capacity() * size_of::<ClasspathMethodStub>()) as u64,
            );
            for method in &stub.methods {
                add_method_stub(bytes, method);
            }
        }

        let mut bytes = 0u64;

        bytes = bytes.saturating_add(
            (self.stubs_by_binary.capacity() * size_of::<(String, ClasspathClassStub)>()) as u64,
        );
        for (key, stub) in &self.stubs_by_binary {
            add_string(&mut bytes, key);
            add_class_stub(&mut bytes, stub);
        }

        bytes = bytes.saturating_add(
            (self.internal_to_binary.capacity() * size_of::<(String, String)>()) as u64,
        );
        for (k, v) in &self.internal_to_binary {
            add_string(&mut bytes, k);
            add_string(&mut bytes, v);
        }

        bytes = bytes
            .saturating_add((self.binary_names_sorted.capacity() * size_of::<String>()) as u64);
        for name in &self.binary_names_sorted {
            add_string(&mut bytes, name);
        }

        bytes =
            bytes.saturating_add((self.packages_sorted.capacity() * size_of::<String>()) as u64);
        for pkg in &self.packages_sorted {
            add_string(&mut bytes, pkg);
        }

        bytes
    }
    pub fn lookup_binary(&self, binary_name: &str) -> Option<&ClasspathClassStub> {
        self.stubs_by_binary.get(binary_name)
    }

    pub fn lookup_internal(&self, internal_name: &str) -> Option<&ClasspathClassStub> {
        let binary = self.internal_to_binary.get(internal_name)?;
        self.lookup_binary(binary)
    }

    /// All indexed class binary names (`java.lang.String`) in **stable sorted order**.
    ///
    /// This is a zero-allocation view over the internal index data and is intended
    /// for bulk iteration (e.g. pre-interning ids into a project-level `TypeStore`).
    pub fn binary_class_names(&self) -> &[String] {
        &self.binary_names_sorted
    }

    /// Iterate all indexed class binary names (`java.lang.String`) in **stable sorted order**.
    ///
    /// This is equivalent to `self.binary_class_names().iter().map(|s| s.as_str())` but
    /// more convenient at call sites.
    pub fn iter_binary_class_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.binary_names_sorted.iter().map(|s| s.as_str())
    }

    pub fn class_names_with_prefix(&self, prefix: &str) -> Vec<String> {
        let prefix = normalize_binary_prefix(prefix);
        let names = &self.binary_names_sorted;
        let start = names.partition_point(|name| name.as_str() < prefix.as_ref());
        let mut out = Vec::new();
        for name in &names[start..] {
            if name.starts_with(prefix.as_ref()) {
                out.push(name.clone());
            } else {
                break;
            }
        }
        out
    }

    pub fn packages_with_prefix(&self, prefix: &str) -> Vec<String> {
        let prefix = normalize_binary_prefix(prefix);
        let pkgs = &self.packages_sorted;
        let start = pkgs.partition_point(|pkg| pkg.as_str() < prefix.as_ref());
        let mut out = Vec::new();
        for pkg in &pkgs[start..] {
            if pkg.starts_with(prefix.as_ref()) {
                out.push(pkg.clone());
            } else {
                break;
            }
        }
        out
    }
}

fn normalize_binary_prefix(prefix: &str) -> Cow<'_, str> {
    if prefix.contains('/') {
        Cow::Owned(prefix.replace('/', "."))
    } else {
        Cow::Borrowed(prefix)
    }
}

impl TypeProvider for ClasspathIndex {
    fn lookup_type(&self, binary_name: &str) -> Option<TypeDefStub> {
        self.stubs_by_binary.get(binary_name).map(TypeDefStub::from)
    }
}

#[derive(Debug)]
pub struct ModuleAwareClasspathIndex {
    pub types: ClasspathIndex,
    pub type_to_module: HashMap<String, Option<ModuleName>>,
    pub modules: Vec<(Option<ModuleName>, ModuleNameKind)>,
}

impl ModuleAwareClasspathIndex {
    pub fn build(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
    ) -> Result<Self, ClasspathError> {
        Self::build_with_options(entries, cache_dir, IndexOptions::default())
    }

    pub fn build_with_options(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let deps_store = DependencyIndexStore::from_env().ok();
        Self::build_with_deps_store_and_options(
            entries,
            cache_dir,
            deps_store.as_ref(),
            None,
            options,
        )
    }

    pub fn build_with_deps_store(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, ClasspathError> {
        Self::build_with_deps_store_and_options(
            entries,
            cache_dir,
            deps_store,
            stats,
            IndexOptions::default(),
        )
    }

    pub fn build_with_deps_store_and_options(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();
        let mut type_to_module = HashMap::new();
        let mut modules = Vec::with_capacity(entries.len());

        Self::extend_index_with_meta(
            &mut stubs_by_binary,
            &mut internal_to_binary,
            &mut type_to_module,
            &mut modules,
            entries,
            cache_dir,
            deps_store,
            stats,
            options,
            |entry| entry.module_meta(),
        )?;

        Ok(Self::finish_index(
            stubs_by_binary,
            internal_to_binary,
            type_to_module,
            modules,
        ))
    }

    /// Build a module-aware index for entries that are treated as `--module-path`.
    ///
    /// This differs from [`ModuleAwareClasspathIndex::build`] in how it handles
    /// class directories: directories are treated as modules (explicit when they
    /// contain `module-info.class`, otherwise automatic) instead of being forced
    /// into the unnamed module.
    pub fn build_module_path(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
    ) -> Result<Self, ClasspathError> {
        Self::build_module_path_with_options(entries, cache_dir, IndexOptions::default())
    }

    pub fn build_module_path_with_options(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let deps_store = DependencyIndexStore::from_env().ok();
        Self::build_module_path_with_deps_store_and_options(
            entries,
            cache_dir,
            deps_store.as_ref(),
            None,
            options,
        )
    }

    pub fn build_module_path_with_deps_store(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, ClasspathError> {
        Self::build_module_path_with_deps_store_and_options(
            entries,
            cache_dir,
            deps_store,
            stats,
            IndexOptions::default(),
        )
    }

    pub fn build_module_path_with_deps_store_and_options(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();
        let mut type_to_module = HashMap::new();
        let mut modules = Vec::with_capacity(entries.len());

        Self::extend_index_with_meta(
            &mut stubs_by_binary,
            &mut internal_to_binary,
            &mut type_to_module,
            &mut modules,
            entries,
            cache_dir,
            deps_store,
            stats,
            options,
            |entry| entry.module_meta_for_module_path(),
        )?;

        Ok(Self::finish_index(
            stubs_by_binary,
            internal_to_binary,
            type_to_module,
            modules,
        ))
    }

    /// Build a module-aware index for entries that are treated as classpath items.
    ///
    /// All entries (including JARs with `module-info.class`) are treated as
    /// belonging to the unnamed module. This matches traditional Java classpath
    /// semantics.
    pub fn build_classpath(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
    ) -> Result<Self, ClasspathError> {
        Self::build_classpath_with_options(entries, cache_dir, IndexOptions::default())
    }

    pub fn build_classpath_with_options(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let deps_store = DependencyIndexStore::from_env().ok();
        Self::build_classpath_with_deps_store_and_options(
            entries,
            cache_dir,
            deps_store.as_ref(),
            None,
            options,
        )
    }

    pub fn build_classpath_with_deps_store(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, ClasspathError> {
        Self::build_classpath_with_deps_store_and_options(
            entries,
            cache_dir,
            deps_store,
            stats,
            IndexOptions::default(),
        )
    }

    pub fn build_classpath_with_deps_store_and_options(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();
        let mut type_to_module = HashMap::new();
        let mut modules = Vec::with_capacity(entries.len());

        Self::extend_index_with_meta(
            &mut stubs_by_binary,
            &mut internal_to_binary,
            &mut type_to_module,
            &mut modules,
            entries,
            cache_dir,
            deps_store,
            stats,
            options,
            |_| {
                Ok(EntryModuleMeta {
                    name: None,
                    kind: ModuleNameKind::None,
                })
            },
        )?;

        Ok(Self::finish_index(
            stubs_by_binary,
            internal_to_binary,
            type_to_module,
            modules,
        ))
    }

    /// Build a module-aware index from a JPMS module-path and an additional classpath.
    ///
    /// - `module_path_entries` are treated as named modules (explicit or automatic)
    /// - `classpath_entries` are treated as belonging to the unnamed module
    ///
    /// When a type is present in both, the module-path entry wins (first match).
    pub fn build_mixed(
        module_path_entries: &[ClasspathEntry],
        classpath_entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
    ) -> Result<Self, ClasspathError> {
        Self::build_mixed_with_options(
            module_path_entries,
            classpath_entries,
            cache_dir,
            IndexOptions::default(),
        )
    }

    pub fn build_mixed_with_options(
        module_path_entries: &[ClasspathEntry],
        classpath_entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let deps_store = DependencyIndexStore::from_env().ok();
        Self::build_mixed_with_deps_store_and_options(
            module_path_entries,
            classpath_entries,
            cache_dir,
            deps_store.as_ref(),
            None,
            options,
        )
    }

    pub fn build_mixed_with_deps_store(
        module_path_entries: &[ClasspathEntry],
        classpath_entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, ClasspathError> {
        Self::build_mixed_with_deps_store_and_options(
            module_path_entries,
            classpath_entries,
            cache_dir,
            deps_store,
            stats,
            IndexOptions::default(),
        )
    }

    pub fn build_mixed_with_deps_store_and_options(
        module_path_entries: &[ClasspathEntry],
        classpath_entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
        options: IndexOptions,
    ) -> Result<Self, ClasspathError> {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();
        let mut type_to_module = HashMap::new();
        let mut modules = Vec::with_capacity(module_path_entries.len() + classpath_entries.len());

        Self::extend_index_with_meta(
            &mut stubs_by_binary,
            &mut internal_to_binary,
            &mut type_to_module,
            &mut modules,
            module_path_entries,
            cache_dir,
            deps_store,
            stats,
            options,
            |entry| entry.module_meta_for_module_path(),
        )?;

        Self::extend_index_with_meta(
            &mut stubs_by_binary,
            &mut internal_to_binary,
            &mut type_to_module,
            &mut modules,
            classpath_entries,
            cache_dir,
            deps_store,
            stats,
            options,
            |_| {
                Ok(EntryModuleMeta {
                    name: None,
                    kind: ModuleNameKind::None,
                })
            },
        )?;

        Ok(Self::finish_index(
            stubs_by_binary,
            internal_to_binary,
            type_to_module,
            modules,
        ))
    }

    fn finish_index(
        stubs_by_binary: HashMap<String, ClasspathClassStub>,
        internal_to_binary: HashMap<String, String>,
        type_to_module: HashMap<String, Option<ModuleName>>,
        modules: Vec<(Option<ModuleName>, ModuleNameKind)>,
    ) -> Self {
        let mut binary_names_sorted: Vec<String> = stubs_by_binary.keys().cloned().collect();
        binary_names_sorted.sort();

        let mut packages: BTreeSet<String> = BTreeSet::new();
        for name in &binary_names_sorted {
            if let Some((pkg, _)) = name.rsplit_once('.') {
                packages.insert(pkg.to_owned());
            }
        }

        let types = ClasspathIndex {
            stubs_by_binary,
            binary_names_sorted,
            packages_sorted: packages.into_iter().collect(),
            internal_to_binary,
        };

        Self {
            types,
            type_to_module,
            modules,
        }
    }

    fn extend_index_with_meta<F>(
        stubs_by_binary: &mut HashMap<String, ClasspathClassStub>,
        internal_to_binary: &mut HashMap<String, String>,
        type_to_module: &mut HashMap<String, Option<ModuleName>>,
        modules: &mut Vec<(Option<ModuleName>, ModuleNameKind)>,
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
        options: IndexOptions,
        mut meta_fn: F,
    ) -> Result<(), ClasspathError>
    where
        F: FnMut(&ClasspathEntry) -> Result<EntryModuleMeta, ClasspathError>,
    {
        for entry in entries {
            let entry = entry.normalize()?;
            let module_meta = meta_fn(&entry)?;
            modules.push((module_meta.name.clone(), module_meta.kind));

            let stubs = match &entry {
                ClasspathEntry::ClassDir(_) => {
                    let fingerprint = entry.fingerprint()?;
                    if let Some(cache_dir) = cache_dir {
                        persist::load_or_build_entry(
                            cache_dir,
                            &entry,
                            fingerprint,
                            options.target_release,
                            || index_entry(&entry, deps_store, stats, options),
                        )?
                    } else {
                        index_entry(&entry, deps_store, stats, options)?
                    }
                }
                ClasspathEntry::Jar(_) | ClasspathEntry::Jmod(_) => {
                    index_entry(&entry, deps_store, stats, options)?
                }
            };

            for stub in stubs {
                if stubs_by_binary.contains_key(&stub.binary_name) {
                    continue;
                }

                let binary_name = stub.binary_name.clone();
                internal_to_binary.insert(stub.internal_name.clone(), binary_name.clone());
                stubs_by_binary.insert(binary_name.clone(), stub);
                type_to_module.insert(binary_name, module_meta.name.clone());
            }
        }

        Ok(())
    }

    /// Construct an in-memory index from existing class stubs.
    ///
    /// Each stub can optionally be associated with a named module.
    pub fn from_stubs<I>(stubs: I) -> Self
    where
        I: IntoIterator<Item = (ClasspathClassStub, Option<ModuleName>)>,
    {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();
        let mut type_to_module = HashMap::new();

        for (stub, module) in stubs {
            if stubs_by_binary.contains_key(&stub.binary_name) {
                continue;
            }

            let binary_name = stub.binary_name.clone();
            internal_to_binary.insert(stub.internal_name.clone(), binary_name.clone());
            stubs_by_binary.insert(binary_name.clone(), stub);
            type_to_module.insert(binary_name, module);
        }

        let mut binary_names_sorted: Vec<String> = stubs_by_binary.keys().cloned().collect();
        binary_names_sorted.sort();

        let mut packages: BTreeSet<String> = BTreeSet::new();
        for name in &binary_names_sorted {
            if let Some((pkg, _)) = name.rsplit_once('.') {
                packages.insert(pkg.to_owned());
            }
        }

        let types = ClasspathIndex {
            stubs_by_binary,
            binary_names_sorted,
            packages_sorted: packages.into_iter().collect(),
            internal_to_binary,
        };

        let mut module_names: BTreeSet<ModuleName> = BTreeSet::new();
        for module in type_to_module.values().filter_map(|m| m.clone()) {
            module_names.insert(module);
        }
        let modules = module_names
            .into_iter()
            .map(|m| (Some(m), ModuleNameKind::Explicit))
            .collect();

        Self {
            types,
            type_to_module,
            modules,
        }
    }

    pub fn module_of(&self, binary_name: &str) -> Option<&ModuleName> {
        self.type_to_module.get(binary_name)?.as_ref()
    }

    pub fn module_kind_of(&self, binary_name: &str) -> ModuleNameKind {
        let Some(module) = self.type_to_module.get(binary_name) else {
            return ModuleNameKind::None;
        };

        let Some(module) = module else {
            return ModuleNameKind::None;
        };

        self.modules
            .iter()
            .find_map(|(candidate, kind)| {
                candidate.as_ref().filter(|m| *m == module).map(|_| *kind)
            })
            .unwrap_or(ModuleNameKind::None)
    }
}

impl TypeIndex for ClasspathIndex {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        let dotted = name.to_dotted();
        self.stubs_by_binary
            .contains_key(&dotted)
            .then(|| TypeName::new(dotted))
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        let pkg = package.to_dotted();
        let fq = if pkg.is_empty() {
            name.as_str().to_string()
        } else {
            format!("{pkg}.{}", name.as_str())
        };
        self.stubs_by_binary
            .contains_key(&fq)
            .then(|| TypeName::new(fq))
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.packages_sorted
            .binary_search(&package.to_dotted())
            .is_ok()
    }

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        const ACC_STATIC: u16 = 0x0008;

        let stub = self.stubs_by_binary.get(owner.as_str())?;
        let needle = name.as_str();

        let is_static = |flags: u16| flags & ACC_STATIC != 0;
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

impl TypeProvider for ModuleAwareClasspathIndex {
    fn lookup_type(&self, binary_name: &str) -> Option<TypeDefStub> {
        self.types.lookup_type(binary_name)
    }
}

impl TypeIndex for ModuleAwareClasspathIndex {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        self.types.resolve_type(name)
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        self.types.resolve_type_in_package(package, name)
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.types.package_exists(package)
    }

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        self.types.resolve_static_member(owner, name)
    }
}

fn index_entry(
    entry: &ClasspathEntry,
    deps_store: Option<&DependencyIndexStore>,
    stats: Option<&IndexingStats>,
    options: IndexOptions,
) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    match entry {
        ClasspathEntry::ClassDir(dir) => index_class_dir(dir, options),
        ClasspathEntry::Jar(path) => {
            index_zip_with_deps_cache(path, ZipKind::Jar, deps_store, stats, options)
        }
        ClasspathEntry::Jmod(path) => {
            index_zip_with_deps_cache(path, ZipKind::Jmod, deps_store, stats, options)
        }
    }
}

fn index_class_dir(
    dir: &Path,
    options: IndexOptions,
) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    // Exploded multi-release JARs typically include the manifest, but some build tooling may drop
    // it when extracting. We treat the directory as multi-release if either:
    // - the manifest opts into multi-release behavior, or
    // - a `META-INF/versions` directory is present.
    let is_multi_release = dir_is_multi_release(dir) || dir.join("META-INF/versions").is_dir();

    // Ensure deterministic directory iteration (WalkDir does not guarantee ordering).
    let mut class_files: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension() != Some(OsStr::new("class")) {
            continue;
        }
        class_files.push(entry.into_path());
    }
    class_files.sort();

    let mut best: HashMap<String, (u32, ClasspathClassStub)> = HashMap::new();
    let target_release = options.target_release.map(|r| r as u32);

    for path in class_files {
        let rel = path.strip_prefix(dir).unwrap_or(&path);
        let mr_version = match mr_version_from_dir_path(rel, is_multi_release) {
            Some(v) => v,
            None => continue,
        };
        let version = mr_version.unwrap_or(0);

        if let Some(target) = target_release {
            if version > target {
                continue;
            }
        }

        let bytes = std::fs::read(&path)?;
        let cf = ClassFile::parse(&bytes)?;
        if is_ignored_class(&cf.this_class) {
            continue;
        }
        let stub = stub_from_classfile(cf);
        let key = stub.binary_name.clone();

        match target_release {
            Some(_) => match best.get(&key) {
                None => {
                    best.insert(key, (version, stub));
                }
                Some((existing_version, _)) => {
                    if version > *existing_version {
                        best.insert(key, (version, stub));
                    }
                }
            },
            None => match best.get(&key) {
                None => {
                    best.insert(key, (version, stub));
                }
                Some((existing_version, _)) => {
                    if *existing_version == 0 {
                        // Base entry already exists; keep it.
                        continue;
                    }

                    if version == 0 || version > *existing_version {
                        // Prefer base over any MR entry, otherwise pick the highest MR version.
                        best.insert(key, (version, stub));
                    }
                }
            },
        }
    }

    let mut out: Vec<ClasspathClassStub> = best.into_values().map(|(_, stub)| stub).collect();
    out.sort_by(|a, b| a.binary_name.cmp(&b.binary_name));
    Ok(out)
}

enum ZipKind {
    Jar,
    Jmod,
}

fn read_module_info_from_dir(dir: &Path) -> Result<Option<ModuleInfo>, ClasspathError> {
    // Multi-release JARs can store `module-info.class` under `META-INF/versions/9/`.
    // While directories are not formally multi-release archives, some build tools
    // (or Bazel-style dependency extraction) surface JAR contents as directories,
    // so we check the versioned location as a best-effort.
    for candidate in ["module-info.class", "META-INF/versions/9/module-info.class"] {
        let path = dir.join(candidate);
        if !path.is_file() {
            continue;
        }

        let bytes = std::fs::read(path)?;
        return Ok(Some(parse_module_info_class(&bytes)?));
    }

    Ok(None)
}

fn read_module_info_from_zip(
    path: &Path,
    kind: ZipKind,
) -> Result<Option<ModuleInfo>, ClasspathError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut archive = zip::ZipArchive::new(file)?;

    let candidates: &[&str] = match kind {
        ZipKind::Jar => &["module-info.class", "META-INF/versions/9/module-info.class"],
        ZipKind::Jmod => &["classes/module-info.class"],
    };

    for candidate in candidates {
        match archive.by_name(candidate) {
            Ok(mut entry) => {
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut bytes)?;
                return Ok(Some(parse_module_info_class(&bytes)?));
            }
            Err(zip::result::ZipError::FileNotFound) => continue,
            Err(err) => return Err(err.into()),
        }
    }

    Ok(None)
}

fn index_zip_with_deps_cache(
    path: &Path,
    kind: ZipKind,
    deps_store: Option<&DependencyIndexStore>,
    stats: Option<&IndexingStats>,
    options: IndexOptions,
) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    let Some(store) = deps_store else {
        return index_zip(path, kind, stats, options);
    };

    let jar_sha256 = match nova_deps_cache::sha256_hex(path) {
        Ok(sha) => sha,
        Err(_) => {
            // If hashing fails for any reason, fall back to parsing without
            // caching (hashing reads the same underlying file).
            return index_zip(path, kind, stats, options);
        }
    };

    let jar_cache_key = match options.target_release {
        Some(r) => format!("{jar_sha256}-r{r}"),
        None => jar_sha256.clone(),
    };

    match store.try_load(&jar_cache_key) {
        Ok(Some(bundle)) => {
            record_deps_cache_hit(stats);
            return Ok(bundle
                .classes
                .into_iter()
                .map(ClasspathClassStub::from)
                .collect());
        }
        Ok(None) => {}
        Err(_) => {}
    }

    let stubs = index_zip(path, kind, stats, options)?;

    let bundle = bundle_from_classpath_stubs(jar_cache_key, &stubs);
    // Best-effort cache write; indexing should still succeed if persistence fails.
    let _ = store.store(&bundle);

    Ok(stubs)
}

fn index_zip(
    path: &Path,
    kind: ZipKind,
    stats: Option<&IndexingStats>,
    options: IndexOptions,
) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut archive = zip::ZipArchive::new(file)?;

    match kind {
        ZipKind::Jmod => {
            let mut out = Vec::new();
            for i in 0..archive.len() {
                let mut file = archive.by_index(i)?;
                if !file.is_file() {
                    continue;
                }
                let name = file.name().to_owned();

                if !name.ends_with(".class") {
                    continue;
                }

                // JMODs place class files under `classes/`.
                if !name.starts_with("classes/") {
                    continue;
                }

                let mut bytes = Vec::with_capacity(file.size() as usize);
                file.read_to_end(&mut bytes)?;
                record_parsed(stats);
                let cf = ClassFile::parse(&bytes)?;
                if is_ignored_class(&cf.this_class) {
                    continue;
                }
                out.push(stub_from_classfile(cf));
            }
            Ok(out)
        }
        ZipKind::Jar => {
            let is_multi_release = jar_is_multi_release(&mut archive);

            // JARs can be multi-release, where version-specific class files live
            // under `META-INF/versions/<n>/...`.
            //
            // For now we:
            // - index base classes normally
            // - index multi-release classes only if the base class is missing,
            //   preferring the highest version present
            // This avoids accidentally overriding base classes when Nova doesn't
            // know the target JDK for the project.
            let mut best: HashMap<String, (u32, ClasspathClassStub)> = HashMap::new();
            let target_release = options.target_release.map(|r| r as u32);

            for i in 0..archive.len() {
                let mut file = archive.by_index(i)?;
                if !file.is_file() {
                    continue;
                }
                let name = file.name().to_owned();

                if !name.ends_with(".class") {
                    continue;
                }

                let mr_version = if is_multi_release {
                    if let Some(rest) = name.strip_prefix("META-INF/versions/") {
                        let Some((version, _path)) = rest.split_once('/') else {
                            continue;
                        };
                        match version.parse::<u32>() {
                            Ok(v) => Some(v),
                            Err(_) => continue,
                        }
                    } else if name.starts_with("META-INF/") {
                        continue;
                    } else {
                        None
                    }
                } else if name.starts_with("META-INF/") {
                    continue;
                } else {
                    None
                };

                let mut bytes = Vec::with_capacity(file.size() as usize);
                file.read_to_end(&mut bytes)?;
                record_parsed(stats);
                let cf = ClassFile::parse(&bytes)?;
                if is_ignored_class(&cf.this_class) {
                    continue;
                }

                let stub = stub_from_classfile(cf);
                let key = stub.binary_name.clone();
                let version = mr_version.unwrap_or(0);

                if let Some(target) = target_release {
                    if version > target {
                        // For release-aware indexing we ignore MR variants that are newer than the
                        // target release. This also means classes that exist only in MR entries and
                        // have no applicable version are omitted from the index.
                        continue;
                    }

                    match best.get(&key) {
                        None => {
                            best.insert(key, (version, stub));
                        }
                        Some((existing_version, _)) => {
                            if version > *existing_version {
                                best.insert(key, (version, stub));
                            }
                        }
                    }
                } else {
                    // Legacy conservative behavior: base wins; MR is only used when the base is
                    // missing, preferring the highest MR version present.
                    match best.get(&key) {
                        None => {
                            best.insert(key, (version, stub));
                        }
                        Some((existing_version, _)) => {
                            if *existing_version == 0 {
                                // Base entry already exists; keep it.
                                continue;
                            }

                            if version == 0 || version > *existing_version {
                                // Prefer base over any MR entry, otherwise pick the highest MR version.
                                best.insert(key, (version, stub));
                            }
                        }
                    }
                }
            }

            let mut out: Vec<ClasspathClassStub> =
                best.into_values().map(|(_, stub)| stub).collect();
            out.sort_by(|a, b| a.binary_name.cmp(&b.binary_name));
            Ok(out)
        }
    }
}

fn jar_is_multi_release<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> bool {
    let mut file = match archive.by_name("META-INF/MANIFEST.MF") {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return false,
        Err(_) => return false,
    };

    let mut manifest = String::new();
    if file.read_to_string(&mut manifest).is_err() {
        return false;
    }

    manifest_is_multi_release(&manifest)
}

fn dir_is_multi_release(dir: &Path) -> bool {
    let manifest_path = dir.join("META-INF/MANIFEST.MF");
    let Ok(manifest) = std::fs::read_to_string(manifest_path) else {
        return false;
    };
    manifest_is_multi_release(&manifest)
}

/// Determines the multi-release version for a `.class` file in an exploded (directory) entry.
///
/// Returns:
/// - `Some(None)` for a base entry
/// - `Some(Some(v))` for a multi-release entry under `META-INF/versions/<v>/...`
/// - `None` when the file should be ignored (e.g. `META-INF/**` noise)
fn mr_version_from_dir_path(rel: &Path, is_multi_release: bool) -> Option<Option<u32>> {
    use std::path::Component;

    let mut components = rel.components();
    let Some(first) = components.next() else {
        return None;
    };

    if matches!(first, Component::Normal(name) if name == "META-INF") {
        if !is_multi_release {
            return None;
        }

        let Some(second) = components.next() else {
            return None;
        };
        if !matches!(second, Component::Normal(name) if name == "versions") {
            return None;
        }

        let Some(version_component) = components.next() else {
            return None;
        };
        let Component::Normal(version_component) = version_component else {
            return None;
        };
        let version = version_component.to_str()?.parse::<u32>().ok()?;
        return Some(Some(version));
    }

    Some(None)
}

fn manifest_is_multi_release(manifest: &str) -> bool {
    for line in manifest.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("Multi-Release") {
            return value.trim().eq_ignore_ascii_case("true");
        }
    }
    false
}

fn stub_from_classfile(cf: ClassFile) -> ClasspathClassStub {
    let internal_name = cf.this_class;
    let binary_name = internal_name_to_binary(&internal_name);

    ClasspathClassStub {
        binary_name,
        internal_name,
        access_flags: cf.access_flags,
        super_binary_name: cf.super_class.map(|s| internal_name_to_binary(&s)),
        interfaces: cf
            .interfaces
            .into_iter()
            .map(|i| internal_name_to_binary(&i))
            .collect(),
        signature: cf.signature,
        annotations: cf
            .runtime_visible_annotations
            .into_iter()
            .chain(cf.runtime_invisible_annotations.into_iter())
            .map(|a| a.type_descriptor)
            .collect(),
        fields: cf
            .fields
            .into_iter()
            .map(|f| ClasspathFieldStub {
                name: f.name,
                descriptor: f.descriptor,
                signature: f.signature,
                access_flags: f.access_flags,
                annotations: f
                    .runtime_visible_annotations
                    .into_iter()
                    .chain(f.runtime_invisible_annotations.into_iter())
                    .map(|a| a.type_descriptor)
                    .collect(),
            })
            .collect(),
        methods: cf
            .methods
            .into_iter()
            .map(|m| ClasspathMethodStub {
                name: m.name,
                descriptor: m.descriptor,
                signature: m.signature,
                access_flags: m.access_flags,
                annotations: m
                    .runtime_visible_annotations
                    .into_iter()
                    .chain(m.runtime_invisible_annotations.into_iter())
                    .map(|a| a.type_descriptor)
                    .collect(),
            })
            .collect(),
    }
}

fn deps_field_stub(value: &ClasspathFieldStub) -> DepsFieldStub {
    DepsFieldStub {
        name: value.name.clone(),
        descriptor: value.descriptor.clone(),
        signature: value.signature.clone(),
        access_flags: value.access_flags,
        annotations: value.annotations.clone(),
    }
}

fn deps_method_stub(value: &ClasspathMethodStub) -> DepsMethodStub {
    DepsMethodStub {
        name: value.name.clone(),
        descriptor: value.descriptor.clone(),
        signature: value.signature.clone(),
        access_flags: value.access_flags,
        annotations: value.annotations.clone(),
    }
}

fn deps_class_stub(value: &ClasspathClassStub) -> DepsClassStub {
    DepsClassStub {
        binary_name: value.binary_name.clone(),
        internal_name: value.internal_name.clone(),
        access_flags: value.access_flags,
        super_binary_name: value.super_binary_name.clone(),
        interfaces: value.interfaces.clone(),
        signature: value.signature.clone(),
        annotations: value.annotations.clone(),
        fields: value.fields.iter().map(deps_field_stub).collect(),
        methods: value.methods.iter().map(deps_method_stub).collect(),
    }
}

fn bundle_from_classpath_stubs(
    jar_sha256: String,
    stubs: &[ClasspathClassStub],
) -> DependencyIndexBundle {
    let mut classes: Vec<DepsClassStub> = stubs.iter().map(deps_class_stub).collect();
    classes.sort_by(|a, b| a.binary_name.cmp(&b.binary_name));

    let binary_names_sorted: Vec<String> = classes.iter().map(|c| c.binary_name.clone()).collect();

    let mut packages = BTreeSet::new();
    let mut package_prefixes = BTreeSet::new();
    for name in &binary_names_sorted {
        if let Some((pkg, _)) = name.rsplit_once('.') {
            packages.insert(pkg.to_string());

            let mut acc = String::new();
            for (i, part) in pkg.split('.').enumerate() {
                if i > 0 {
                    acc.push('.');
                }
                acc.push_str(part);
                package_prefixes.insert(acc.clone());
            }
        }
    }

    DependencyIndexBundle {
        jar_sha256,
        classes,
        packages: packages.into_iter().collect(),
        package_prefixes: package_prefixes.into_iter().collect(),
        trigram_index: nova_deps_cache::build_trigram_index(&binary_names_sorted),
        binary_names_sorted,
    }
}

impl From<DepsFieldStub> for ClasspathFieldStub {
    fn from(value: DepsFieldStub) -> Self {
        Self {
            name: value.name,
            descriptor: value.descriptor,
            signature: value.signature,
            access_flags: value.access_flags,
            annotations: value.annotations,
        }
    }
}

impl From<DepsMethodStub> for ClasspathMethodStub {
    fn from(value: DepsMethodStub) -> Self {
        Self {
            name: value.name,
            descriptor: value.descriptor,
            signature: value.signature,
            access_flags: value.access_flags,
            annotations: value.annotations,
        }
    }
}

impl From<DepsClassStub> for ClasspathClassStub {
    fn from(value: DepsClassStub) -> Self {
        Self {
            binary_name: value.binary_name,
            internal_name: value.internal_name,
            access_flags: value.access_flags,
            super_binary_name: value.super_binary_name,
            interfaces: value.interfaces,
            signature: value.signature,
            annotations: value.annotations,
            fields: value
                .fields
                .into_iter()
                .map(ClasspathFieldStub::from)
                .collect(),
            methods: value
                .methods
                .into_iter()
                .map(ClasspathMethodStub::from)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    fn test_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/dep.jar")
    }

    fn test_class_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/classdir")
    }

    fn test_jmod() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-jdk/testdata/fake-jdk/jmods/java.base.jmod")
    }

    fn test_multirelease_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/multirelease.jar")
    }

    fn test_not_multirelease_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/not-multirelease.jar")
    }

    fn test_named_module_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/named-module.jar")
    }

    fn test_named_module_jmod() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/named-module.jmod")
    }

    fn test_manifest_named_jar() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/automatic-module-name-1.2.3.jar")
    }

    #[test]
    fn derives_automatic_module_name_for_directories() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("foo-bar-1.2.3");
        std::fs::create_dir_all(&dir).unwrap();

        let name = derive_automatic_module_name_from_path(&dir).expect("expected derived name");
        assert_eq!(name.as_str(), "foo.bar");
    }

    #[test]
    fn derives_automatic_module_name_for_missing_dotted_directories() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("com.example.app");
        // Intentionally do not create `dir` to exercise the non-existent-path
        // behavior (where `Path::is_dir()` returns false).

        let name = derive_automatic_module_name_from_path(&dir).expect("expected derived name");
        assert_eq!(name.as_str(), "com.example.app");
    }

    #[test]
    fn reads_module_info_from_jmod_entry() {
        let entry = ClasspathEntry::Jmod(test_named_module_jmod());
        let info = entry
            .module_info()
            .unwrap()
            .expect("expected module-info.class in named-module.jmod fixture");
        assert_eq!(info.name.as_str(), "example.mod");
    }

    #[test]
    fn reads_module_info_from_jar_entry() {
        let entry = ClasspathEntry::Jar(test_named_module_jar());
        let info = entry
            .module_info()
            .unwrap()
            .expect("expected module-info.class in named-module.jar fixture");
        assert_eq!(info.name.as_str(), "example.mod");
    }

    #[test]
    fn module_info_returns_none_for_regular_jar() {
        let entry = ClasspathEntry::Jar(test_jar());
        assert!(entry.module_info().unwrap().is_none());
    }

    #[test]
    fn module_info_returns_none_for_missing_jar() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("missing-1.2.3.jar");
        let entry = ClasspathEntry::Jar(missing);
        assert!(entry.module_info().unwrap().is_none());
    }

    #[test]
    fn module_meta_reports_explicit_named_module() {
        let entry = ClasspathEntry::Jar(test_named_module_jar());
        let meta = entry.module_meta().unwrap();
        assert_eq!(meta.kind, ModuleNameKind::Explicit);
        assert_eq!(meta.name.unwrap().as_str(), "example.mod");
    }

    #[test]
    fn module_meta_derives_automatic_module_name_for_missing_jar() {
        let tmp = TempDir::new().unwrap();
        // Do not create the jar on disk; the module name should still be derivable from the
        // filename (JPMS automatic module naming).
        let missing = tmp.path().join("foo-bar-1.2.3.jar");
        let entry = ClasspathEntry::Jar(missing);
        let meta = entry.module_meta().unwrap();
        assert_eq!(meta.kind, ModuleNameKind::Automatic);
        assert_eq!(meta.name.unwrap().as_str(), "foo.bar");
    }

    #[test]
    fn module_meta_derives_automatic_module_name_for_missing_jmod() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("foo-bar.JMOD");
        let entry = ClasspathEntry::Jmod(missing);
        let meta = entry.module_meta_for_module_path().unwrap();
        assert_eq!(meta.kind, ModuleNameKind::Automatic);
        assert_eq!(meta.name.unwrap().as_str(), "foo.bar");
    }

    #[test]
    fn module_meta_prefers_manifest_automatic_module_name() {
        let entry = ClasspathEntry::Jar(test_manifest_named_jar());
        let meta = entry.module_meta().unwrap();
        assert_eq!(meta.kind, ModuleNameKind::Automatic);
        assert_eq!(meta.name.unwrap().as_str(), "com.example.manifest_override");
    }

    #[test]
    fn module_meta_derives_automatic_module_name_from_filename() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("foo-bar_baz-1.2.3.jar");
        std::fs::copy(test_jar(), &path).unwrap();

        let entry = ClasspathEntry::Jar(path);
        let meta = entry.module_meta().unwrap();
        assert_eq!(meta.kind, ModuleNameKind::Automatic);
        assert_eq!(meta.name.unwrap().as_str(), "foo.bar.baz");
    }

    #[test]
    fn module_aware_index_skips_missing_jar_entries() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let missing = tmp.path().join("missing-1.2.3.jar");

        let index = ModuleAwareClasspathIndex::build_mixed_with_deps_store(
            &[ClasspathEntry::Jar(missing)],
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();

        assert!(index.types.lookup_binary("com.example.dep.Foo").is_some());
    }

    #[test]
    fn reads_module_info_from_class_dir_entry() {
        let tmp = TempDir::new().unwrap();

        let file = std::fs::File::open(test_named_module_jar()).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entry = archive.by_name("module-info.class").unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();

        std::fs::write(tmp.path().join("module-info.class"), bytes).unwrap();

        let class_dir = ClasspathEntry::ClassDir(tmp.path().to_path_buf());
        let info = class_dir
            .module_info()
            .unwrap()
            .expect("expected module-info.class in temp dir");
        assert_eq!(info.name.as_str(), "example.mod");
    }

    #[test]
    fn lookup_type_from_jar() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            Some(tmp.path()),
            Some(&deps_store),
            None,
        )
        .unwrap();

        let foo = index.lookup_binary("com.example.dep.Foo").unwrap();
        let strings = foo.methods.iter().find(|m| m.name == "strings").unwrap();

        assert_eq!(strings.descriptor, "()Ljava/util/List;");
        assert_eq!(
            strings.signature.as_deref(),
            Some("()Ljava/util/List<Ljava/lang/String;>;")
        );

        let pref = index.class_names_with_prefix("com.example.dep.F");
        assert!(pref.contains(&"com.example.dep.Foo".to_string()));
        assert!(pref.contains(&"com.example.dep.Foo$Inner".to_string()));
    }

    #[test]
    fn iter_binary_class_names_is_sorted_and_deterministic() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();

        let expected = vec![
            "com.example.dep.Bar",
            "com.example.dep.Foo",
            "com.example.dep.Foo$Inner",
        ];

        let names: Vec<&str> = index.iter_binary_class_names().collect();
        assert_eq!(names, expected);

        // Repeated iteration should yield identical results.
        let names_again: Vec<&str> = index.iter_binary_class_names().collect();
        assert_eq!(names_again, expected);

        // Slice view should match the iterator view.
        let slice_names: Vec<&str> = index
            .binary_class_names()
            .iter()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(slice_names, expected);

        // A rebuild of the index should produce the same stable ordering.
        let index_rebuilt = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();
        assert_eq!(
            index_rebuilt.binary_class_names(),
            index.binary_class_names()
        );
    }

    #[test]
    fn lookup_type_from_class_dir() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::ClassDir(test_class_dir())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();
        assert!(index.lookup_binary("com.example.dep.Bar").is_some());
    }

    #[test]
    fn package_prefix_search() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();
        let pkgs = index.packages_with_prefix("com.example");
        assert!(pkgs.contains(&"com.example.dep".to_string()));
    }

    #[test]
    fn lookup_type_from_jmod() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jmod(test_jmod())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();
        assert!(index.lookup_binary("java.lang.String").is_some());
        assert!(index.lookup_internal("java/lang/String").is_some());
        assert!(index
            .packages_with_prefix("java")
            .contains(&"java.lang".to_string()));
    }

    #[test]
    fn prefix_search_accepts_internal_separators() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();
        let classes = index.class_names_with_prefix("com/example/dep/F");
        assert!(classes.contains(&"com.example.dep.Foo".to_string()));
        let packages = index.packages_with_prefix("com/example");
        assert!(packages.contains(&"com.example.dep".to_string()));
    }

    #[test]
    fn entry_index_is_cached_by_fingerprint() {
        let tmp = TempDir::new().unwrap();
        let entry = ClasspathEntry::Jar(test_jar()).normalize().unwrap();
        let fingerprint = entry.fingerprint().unwrap();

        let stubs_first =
            persist::load_or_build_entry(tmp.path(), &entry, fingerprint, None, || {
                index_entry(&entry, None, None, IndexOptions::default())
            })
            .unwrap();
        assert!(stubs_first
            .iter()
            .any(|s| s.binary_name == "com.example.dep.Foo"));

        let stubs_cached =
            persist::load_or_build_entry(tmp.path(), &entry, fingerprint, None, || {
                panic!("expected cache hit, but builder was invoked")
            })
            .unwrap();

        assert_eq!(stubs_first.len(), stubs_cached.len());
    }

    #[test]
    fn corrupted_classpath_entry_cache_is_ignored_and_rebuilt() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = TempDir::new().unwrap();
        let entry = ClasspathEntry::Jar(test_jar()).normalize().unwrap();
        let fingerprint = entry.fingerprint().unwrap();

        let build_calls = AtomicUsize::new(0);

        let stubs_first =
            persist::load_or_build_entry(tmp.path(), &entry, fingerprint, None, || {
                build_calls.fetch_add(1, Ordering::Relaxed);
                index_entry(&entry, None, None, IndexOptions::default())
            })
            .unwrap();
        assert_eq!(build_calls.load(Ordering::Relaxed), 1);

        // Ensure we're actually hitting the persisted cache.
        let stubs_cached =
            persist::load_or_build_entry(tmp.path(), &entry, fingerprint, None, || {
                panic!("expected cache hit, but builder was invoked")
            })
            .unwrap();
        assert_eq!(stubs_first.len(), stubs_cached.len());

        // Truncate the persisted cache file to simulate corruption.
        let cache_path = tmp
            .path()
            .join(format!("classpath-entry-{}.bin", fingerprint.to_hex()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&cache_path)
            .unwrap();
        file.set_len(10).unwrap();
        drop(file);

        let stubs_rebuilt =
            persist::load_or_build_entry(tmp.path(), &entry, fingerprint, None, || {
                build_calls.fetch_add(1, Ordering::Relaxed);
                index_entry(&entry, None, None, IndexOptions::default())
            })
            .unwrap();
        assert_eq!(build_calls.load(Ordering::Relaxed), 2);
        assert_eq!(stubs_first.len(), stubs_rebuilt.len());

        let bytes = std::fs::read(&cache_path).unwrap();
        let header =
            nova_storage::StorageHeader::decode(&bytes[..nova_storage::HEADER_LEN]).unwrap();
        assert_eq!(header.kind, nova_storage::ArtifactKind::ClasspathEntryStubs);
        assert_eq!(header.schema_version, 2);
    }

    #[test]
    fn resolve_type_returns_typename() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();
        let ty = index
            .resolve_type(&QualifiedName::from_dotted("com.example.dep.Foo"))
            .unwrap();
        assert_eq!(ty, TypeName::new("com.example.dep.Foo"));

        let ty = index
            .resolve_type_in_package(
                &PackageName::from_dotted("com.example.dep"),
                &Name::from("Foo"),
            )
            .unwrap();
        assert_eq!(ty, TypeName::new("com.example.dep.Foo"));
    }

    #[test]
    fn resolve_static_member_uses_classpath_stubs() {
        const ACC_STATIC: u16 = 0x0008;

        let mut index = ClasspathIndex::default();
        index.stubs_by_binary.insert(
            "com.example.Static".to_string(),
            ClasspathClassStub {
                binary_name: "com.example.Static".to_string(),
                internal_name: "com/example/Static".to_string(),
                access_flags: 0,
                super_binary_name: None,
                interfaces: Vec::new(),
                signature: None,
                annotations: Vec::new(),
                fields: vec![ClasspathFieldStub {
                    name: "FOO".to_string(),
                    descriptor: "I".to_string(),
                    signature: None,
                    access_flags: ACC_STATIC,
                    annotations: Vec::new(),
                }],
                methods: vec![ClasspathMethodStub {
                    name: "bar".to_string(),
                    descriptor: "()V".to_string(),
                    signature: None,
                    access_flags: ACC_STATIC,
                    annotations: Vec::new(),
                }],
            },
        );

        let owner = TypeName::new("com.example.Static");
        let member = index
            .resolve_static_member(&owner, &Name::from("FOO"))
            .unwrap();
        assert_eq!(member.as_str(), "com.example.Static::FOO");

        let member = index
            .resolve_static_member(&owner, &Name::from("bar"))
            .unwrap();
        assert_eq!(member.as_str(), "com.example.Static::bar");
    }

    #[test]
    fn indexes_multi_release_jar_versions_directory() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_multirelease_jar())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();
        assert!(index
            .lookup_binary("com.example.mr.MultiReleaseOnly")
            .is_some());
    }

    #[test]
    fn ignores_versions_directory_without_multi_release_manifest() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));
        let index = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_not_multirelease_jar())],
            None,
            Some(&deps_store),
            None,
        )
        .unwrap();
        assert!(index
            .lookup_binary("com.example.mr.MultiReleaseOnly")
            .is_none());
    }

    #[test]
    fn dependency_bundle_is_reused_across_runs() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

        let stats_first = IndexingStats::default();
        let _ = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            Some(&stats_first),
        )
        .unwrap();

        assert!(stats_first.classfiles_parsed() > 0);
        assert_eq!(stats_first.deps_cache_hits(), 0);

        let sha = nova_deps_cache::sha256_hex(&test_jar()).unwrap();
        assert!(deps_store.bundle_path(&sha).exists());

        let stats_second = IndexingStats::default();
        let _ = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            Some(&stats_second),
        )
        .unwrap();

        assert_eq!(stats_second.classfiles_parsed(), 0);
        assert_eq!(stats_second.deps_cache_hits(), 1);
    }

    #[test]
    fn corrupted_bundle_is_ignored_and_rebuilt() {
        let tmp = TempDir::new().unwrap();
        let deps_store = DependencyIndexStore::new(tmp.path().join("deps"));

        let stats_first = IndexingStats::default();
        let _ = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            Some(&stats_first),
        )
        .unwrap();

        let sha = nova_deps_cache::sha256_hex(&test_jar()).unwrap();
        let bundle_path = deps_store.bundle_path(&sha);
        assert!(bundle_path.exists());

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&bundle_path)
            .unwrap();
        file.set_len(10).unwrap();

        let stats_second = IndexingStats::default();
        let _ = ClasspathIndex::build_with_deps_store(
            &[ClasspathEntry::Jar(test_jar())],
            None,
            Some(&deps_store),
            Some(&stats_second),
        )
        .unwrap();

        assert!(stats_second.classfiles_parsed() > 0);
        assert_eq!(stats_second.deps_cache_hits(), 0);
        assert!(deps_store.try_load(&sha).unwrap().is_some());
    }
}
