mod persist;
mod module_name;

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
use nova_deps_cache::{DepsClassStub, DepsFieldStub, DepsMethodStub, DependencyIndexBundle, DependencyIndexStore};
use nova_modules::{ModuleInfo, ModuleName};
use nova_types::{FieldStub, MethodStub, TypeDefStub, TypeProvider};

#[derive(Debug, Error)]
pub enum ClasspathError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("classfile error: {0}")]
    ClassFile(#[from] nova_classfile::Error),
    #[error("bincode error: {0}")]
    Bincode(#[from] Box<bincode::ErrorKind>),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

impl From<&nova_project::ClasspathEntry> for ClasspathEntry {
    fn from(value: &nova_project::ClasspathEntry) -> Self {
        match value.kind {
            nova_project::ClasspathEntryKind::Directory => {
                ClasspathEntry::ClassDir(value.path.clone())
            }
            nova_project::ClasspathEntryKind::Jar => ClasspathEntry::Jar(value.path.clone()),
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
            ClasspathEntry::Jmod(_) => match self.module_info()? {
                Some(info) => Ok(EntryModuleMeta {
                    name: Some(info.name),
                    kind: ModuleNameKind::Explicit,
                }),
                None => Ok(EntryModuleMeta {
                    name: None,
                    kind: ModuleNameKind::None,
                }),
            },
        }
    }
}

fn jar_module_meta(path: &Path) -> Result<EntryModuleMeta, ClasspathError> {
    let file = std::fs::File::open(path)?;
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

    let name = module_name::automatic_module_name_from_jar_manifest(&mut archive)
        .or_else(|| module_name::derive_automatic_module_name_from_jar_path(path).map(ModuleName::new));
    let kind = if name.is_some() {
        ModuleNameKind::Automatic
    } else {
        ModuleNameKind::None
    };

    Ok(EntryModuleMeta {
        name,
        kind,
    })
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClasspathFieldStub {
    pub name: String,
    pub descriptor: String,
    pub signature: Option<String>,
    pub access_flags: u16,
    pub annotations: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClasspathMethodStub {
    pub name: String,
    pub descriptor: String,
    pub signature: Option<String>,
    pub access_flags: u16,
    pub annotations: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        let deps_store = DependencyIndexStore::from_env().ok();
        Self::build_with_deps_store(entries, cache_dir, deps_store.as_ref(), None)
    }

    pub fn build_with_deps_store(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, ClasspathError> {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();

        for entry in entries {
            let entry = entry.normalize()?;
            let stubs = match &entry {
                ClasspathEntry::ClassDir(_) => {
                    let fingerprint = entry.fingerprint()?;
                    if let Some(cache_dir) = cache_dir {
                        persist::load_or_build_entry(cache_dir, &entry, fingerprint, || {
                            index_entry(&entry, deps_store, stats)
                        })?
                    } else {
                        index_entry(&entry, deps_store, stats)?
                    }
                }
                ClasspathEntry::Jar(_) | ClasspathEntry::Jmod(_) => {
                    index_entry(&entry, deps_store, stats)?
                }
            };

            for stub in stubs {
                if stubs_by_binary.contains_key(&stub.binary_name) {
                    continue;
                }
                internal_to_binary.insert(stub.internal_name.clone(), stub.binary_name.clone());
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

    pub fn lookup_binary(&self, binary_name: &str) -> Option<&ClasspathClassStub> {
        self.stubs_by_binary.get(binary_name)
    }

    pub fn lookup_internal(&self, internal_name: &str) -> Option<&ClasspathClassStub> {
        let binary = self.internal_to_binary.get(internal_name)?;
        self.lookup_binary(binary)
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
        let deps_store = DependencyIndexStore::from_env().ok();
        Self::build_with_deps_store(entries, cache_dir, deps_store.as_ref(), None)
    }

    pub fn build_with_deps_store(
        entries: &[ClasspathEntry],
        cache_dir: Option<&Path>,
        deps_store: Option<&DependencyIndexStore>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, ClasspathError> {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();
        let mut type_to_module = HashMap::new();
        let mut modules = Vec::with_capacity(entries.len());

        for entry in entries {
            let entry = entry.normalize()?;
            let module_meta = entry.module_meta()?;
            modules.push((module_meta.name.clone(), module_meta.kind));

            let stubs = match &entry {
                ClasspathEntry::ClassDir(_) => {
                    let fingerprint = entry.fingerprint()?;
                    if let Some(cache_dir) = cache_dir {
                        persist::load_or_build_entry(cache_dir, &entry, fingerprint, || {
                            index_entry(&entry, deps_store, stats)
                        })?
                    } else {
                        index_entry(&entry, deps_store, stats)?
                    }
                }
                ClasspathEntry::Jar(_) | ClasspathEntry::Jmod(_) => index_entry(&entry, deps_store, stats)?,
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

        Ok(Self {
            types,
            type_to_module,
            modules,
        })
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
            .find_map(|(candidate, kind)| candidate.as_ref().filter(|m| *m == module).map(|_| *kind))
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
) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    match entry {
        ClasspathEntry::ClassDir(dir) => index_class_dir(dir),
        ClasspathEntry::Jar(path) => {
            index_zip_with_deps_cache(path, ZipKind::Jar, deps_store, stats)
        }
        ClasspathEntry::Jmod(path) => {
            index_zip_with_deps_cache(path, ZipKind::Jmod, deps_store, stats)
        }
    }
}

fn index_class_dir(dir: &Path) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    let mut out = Vec::new();
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

        let bytes = std::fs::read(entry.path())?;
        let cf = ClassFile::parse(&bytes)?;
        if is_ignored_class(&cf.this_class) {
            continue;
        }
        out.push(stub_from_classfile(cf));
    }
    Ok(out)
}

enum ZipKind {
    Jar,
    Jmod,
}

fn read_module_info_from_dir(dir: &Path) -> Result<Option<ModuleInfo>, ClasspathError> {
    let path = dir.join("module-info.class");
    if !path.is_file() {
        return Ok(None);
    }

    let bytes = std::fs::read(path)?;
    Ok(Some(parse_module_info_class(&bytes)?))
}

fn read_module_info_from_zip(
    path: &Path,
    kind: ZipKind,
) -> Result<Option<ModuleInfo>, ClasspathError> {
    let file = std::fs::File::open(path)?;
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
) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    let Some(store) = deps_store else {
        return index_zip(path, kind, stats);
    };

    let jar_sha256 = match nova_deps_cache::sha256_hex(path) {
        Ok(sha) => sha,
        Err(_) => {
            // If hashing fails for any reason, fall back to parsing without
            // caching (hashing reads the same underlying file).
            return index_zip(path, kind, stats);
        }
    };

    match store.try_load(&jar_sha256) {
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

    let stubs = index_zip(path, kind, stats)?;

    let bundle = bundle_from_classpath_stubs(jar_sha256, &stubs);
    // Best-effort cache write; indexing should still succeed if persistence fails.
    let _ = store.store(&bundle);

    Ok(stubs)
}

fn index_zip(
    path: &Path,
    kind: ZipKind,
    stats: Option<&IndexingStats>,
) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    let file = std::fs::File::open(path)?;
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
    fn module_meta_reports_explicit_named_module() {
        let entry = ClasspathEntry::Jar(test_named_module_jar());
        let meta = entry.module_meta().unwrap();
        assert_eq!(meta.kind, ModuleNameKind::Explicit);
        assert_eq!(meta.name.unwrap().as_str(), "example.mod");
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

        let stubs_first = persist::load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            index_entry(&entry, None, None)
        })
        .unwrap();
        assert!(stubs_first
            .iter()
            .any(|s| s.binary_name == "com.example.dep.Foo"));

        let stubs_cached = persist::load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            panic!("expected cache hit, but builder was invoked")
        })
        .unwrap();

        assert_eq!(stubs_first.len(), stubs_cached.len());
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
