use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use nova_classfile::{parse_module_info_class, ClassFile};
use nova_core::JdkConfig;
use nova_modules::{ModuleGraph, ModuleInfo, ModuleName, JAVA_BASE};
use once_cell::sync::OnceCell;
use thiserror::Error;

use crate::discovery::{JdkDiscoveryError, JdkInstallation};
use crate::ct_sym;
use crate::jar;
use crate::jmod;
use crate::persist;
use crate::stub::{binary_to_internal, internal_to_binary};
use crate::{JdkClassStub, JdkFieldStub, JdkMethodStub};

/// Optional indexing counters used by tests and the CLI.
#[derive(Debug, Default)]
pub struct IndexingStats {
    module_scans: AtomicUsize,
    cache_hits: AtomicUsize,
    cache_writes: AtomicUsize,
}

impl IndexingStats {
    pub fn module_scans(&self) -> usize {
        self.module_scans.load(Ordering::Relaxed)
    }

    pub fn cache_hits(&self) -> usize {
        self.cache_hits.load(Ordering::Relaxed)
    }

    pub fn cache_writes(&self) -> usize {
        self.cache_writes.load(Ordering::Relaxed)
    }
}

fn record_module_scan(stats: Option<&IndexingStats>) {
    if let Some(stats) = stats {
        stats.module_scans.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_cache_hit(stats: Option<&IndexingStats>) {
    if let Some(stats) = stats {
        stats.cache_hits.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_cache_write(stats: Option<&IndexingStats>) {
    if let Some(stats) = stats {
        stats.cache_writes.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
enum JdkContainerKind {
    JmodModule { name: ModuleName },
    Jar,
}

impl JdkContainerKind {
    fn module_name(&self) -> Option<&ModuleName> {
        match self {
            Self::JmodModule { name } => Some(name),
            Self::Jar => None,
        }
    }
}

#[derive(Debug)]
struct JdkContainer {
    kind: JdkContainerKind,
    path: PathBuf,
    indexed: OnceCell<()>,
}

#[derive(Debug)]
pub(crate) struct JdkSymbolIndex {
    containers: Vec<JdkContainer>,
    module_graph: Option<ModuleGraph>,

    by_internal: Mutex<HashMap<String, Arc<JdkClassStub>>>,
    by_binary: Mutex<HashMap<String, Arc<JdkClassStub>>>,

    class_to_container: Mutex<HashMap<String, usize>>,
    missing: Mutex<HashSet<String>>,

    packages: OnceLock<Vec<String>>,
    java_lang: OnceLock<Vec<Arc<JdkClassStub>>>,
    binary_names_sorted: OnceLock<Vec<String>>,
}

impl JdkSymbolIndex {
    pub fn discover_with_cache(
        config: Option<&JdkConfig>,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let install = JdkInstallation::discover(config)?;
        Self::from_installation_with_cache(install, cache_dir, allow_write, stats)
    }

    pub fn from_jdk_root_with_cache(
        root: impl AsRef<Path>,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let install = JdkInstallation::from_root(root)?;
        Self::from_installation_with_cache(install, cache_dir, allow_write, stats)
    }

    fn from_installation_with_cache(
        install: JdkInstallation,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let cache_key_path =
            std::fs::canonicalize(install.root()).unwrap_or_else(|_| install.root().to_path_buf());

        if let Some(jmods_dir) = install.jmods_dir() {
            return Self::from_jpms_with_cache(
                &cache_key_path,
                jmods_dir,
                cache_dir,
                allow_write,
                stats,
            );
        }

        Self::from_legacy_with_cache(&cache_key_path, &install, cache_dir, allow_write, stats)
    }

    fn from_jpms_with_cache(
        cache_key_path: &Path,
        jmods_dir: &Path,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let jmods_dir = jmods_dir.to_path_buf();
        if !jmods_dir.is_dir() {
            return Err(JdkIndexError::MissingJmodsDir { dir: jmods_dir });
        }

        let jmods_dir = std::fs::canonicalize(&jmods_dir).unwrap_or(jmods_dir);

        let mut module_paths: Vec<PathBuf> = std::fs::read_dir(&jmods_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "jmod"))
            .collect();

        // Put `java.base.jmod` first since it's where most core types live.
        module_paths.sort_by_key(|p| {
            let file_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            (file_name != "java.base.jmod", file_name.to_owned())
        });

        if module_paths.is_empty() {
            return Err(JdkIndexError::NoModulesFound { dir: jmods_dir });
        }

        let fingerprints = if cache_dir.is_some() {
            Some(persist::fingerprint_containers(&module_paths)?)
        } else {
            None
        };

        let mut module_graph = ModuleGraph::new();
        let mut containers = Vec::with_capacity(module_paths.len());
        for path in module_paths {
            let Some(bytes) = jmod::read_module_info_class_bytes(&path)? else {
                return Err(JdkIndexError::MissingModuleInfo { path });
            };
            let info = parse_module_info_class(&bytes)?;
            let name = info.name.clone();
            module_graph.insert(info);

            containers.push(JdkContainer {
                kind: JdkContainerKind::JmodModule { name },
                path,
                indexed: OnceCell::new(),
            });
        }

        let this = Self {
            containers,
            module_graph: Some(module_graph),
            by_internal: Mutex::new(HashMap::new()),
            by_binary: Mutex::new(HashMap::new()),
            class_to_container: Mutex::new(HashMap::new()),
            missing: Mutex::new(HashSet::new()),
            packages: OnceLock::new(),
            java_lang: OnceLock::new(),
            binary_names_sorted: OnceLock::new(),
        };

        Self::load_or_build_cache(
            this,
            cache_key_path,
            cache_dir,
            allow_write,
            fingerprints,
            stats,
        )
    }

    fn from_legacy_with_cache(
        cache_key_path: &Path,
        install: &JdkInstallation,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let rt_jar = install.java_home().join("lib").join("rt.jar");
        if !rt_jar.is_file() {
            return Err(JdkIndexError::MissingRtJar { path: rt_jar });
        }

        let mut jar_paths = vec![rt_jar];
        let tools_jar = install.root().join("lib").join("tools.jar");
        if tools_jar.is_file() {
            jar_paths.push(tools_jar);
        }

        let jar_paths: Vec<PathBuf> = jar_paths
            .into_iter()
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
            .collect();

        let fingerprints = if cache_dir.is_some() {
            Some(persist::fingerprint_containers(&jar_paths)?)
        } else {
            None
        };

        let containers = jar_paths
            .into_iter()
            .map(|path| JdkContainer {
                kind: JdkContainerKind::Jar,
                path,
                indexed: OnceCell::new(),
            })
            .collect();

        let this = Self {
            containers,
            module_graph: None,
            by_internal: Mutex::new(HashMap::new()),
            by_binary: Mutex::new(HashMap::new()),
            class_to_container: Mutex::new(HashMap::new()),
            missing: Mutex::new(HashSet::new()),
            packages: OnceLock::new(),
            java_lang: OnceLock::new(),
            binary_names_sorted: OnceLock::new(),
        };

        Self::load_or_build_cache(
            this,
            cache_key_path,
            cache_dir,
            allow_write,
            fingerprints,
            stats,
        )
    }

    fn load_or_build_cache(
        this: Self,
        cache_key_path: &Path,
        cache_dir: Option<&Path>,
        allow_write: bool,
        fingerprints: Option<Vec<persist::ContainerFingerprint>>,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let Some(cache_dir) = cache_dir else {
            return Ok(this);
        };
        let Some(fingerprints) = fingerprints else {
            return Ok(this);
        };

        if let Some(cached) = persist::load_symbol_index(cache_dir, cache_key_path, &fingerprints) {
            record_cache_hit(stats);

            {
                let mut map = this.class_to_container.lock().expect("mutex poisoned");
                *map = cached
                    .class_to_container
                    .into_iter()
                    .map(|(k, v)| (k, v as usize))
                    .collect();
            }

            let _ = this.packages.set(cached.packages_sorted);
            let _ = this.binary_names_sorted.set(cached.binary_names_sorted);

            for container in &this.containers {
                let _ = container.indexed.set(());
            }

            return Ok(this);
        }

        // Cache miss: eagerly scan all containers to build and persist the class map.
        for idx in 0..this.containers.len() {
            this.ensure_container_indexed(idx)?;
            record_module_scan(stats);
        }

        let packages_sorted = this.packages_sorted()?.clone();
        let binary_names_sorted = this.binary_names_sorted()?.clone();

        let class_to_container: HashMap<String, u32> = this
            .class_to_container
            .lock()
            .expect("mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), *v as u32))
            .collect();

        if allow_write
            && persist::store_symbol_index(
                cache_dir,
                cache_key_path,
                fingerprints,
                class_to_container,
                packages_sorted,
                binary_names_sorted,
            )
        {
            record_cache_write(stats);
        }

        Ok(this)
    }

    pub fn module_graph(&self) -> Option<&ModuleGraph> {
        self.module_graph.as_ref()
    }

    pub fn module_info(&self, name: &ModuleName) -> Option<&ModuleInfo> {
        self.module_graph.as_ref()?.get(name)
    }

    pub fn module_of_type(
        &self,
        binary_or_internal: &str,
    ) -> Result<Option<ModuleName>, JdkIndexError> {
        if self.module_graph.is_none() {
            return Ok(None);
        }

        let internal = if binary_or_internal.contains('/') {
            binary_or_internal.to_owned()
        } else if binary_or_internal.contains('.') {
            binary_to_internal(binary_or_internal)
        } else {
            format!("java/lang/{binary_or_internal}")
        };

        if is_non_type_classfile(&internal) {
            return Ok(None);
        }

        if self
            .missing
            .lock()
            .expect("mutex poisoned")
            .contains(&internal)
        {
            return Ok(None);
        }

        if let Some(container_idx) = self
            .class_to_container
            .lock()
            .expect("mutex poisoned")
            .get(&internal)
            .copied()
        {
            return Ok(self.containers[container_idx].kind.module_name().cloned());
        }

        // Lazily index containers until we locate the class. This mirrors
        // `lookup_type` but avoids parsing the classfile itself.
        let mut found_container = None;
        for container_idx in 0..self.containers.len() {
            self.ensure_container_indexed(container_idx)?;
            let container = self
                .class_to_container
                .lock()
                .expect("mutex poisoned")
                .get(&internal)
                .copied();
            if container.is_some() {
                found_container = container;
                break;
            }
        }

        if let Some(container_idx) = found_container {
            return Ok(self.containers[container_idx].kind.module_name().cloned());
        }

        self.missing
            .lock()
            .expect("mutex poisoned")
            .insert(internal);
        Ok(None)
    }

    /// Lookup a type by binary name (`java.lang.String`), internal name
    /// (`java/lang/String`), or an unqualified simple name (`String`) which is
    /// resolved against the implicit `java.lang.*` universe scope.
    pub fn lookup_type(&self, name: &str) -> Result<Option<Arc<JdkClassStub>>, JdkIndexError> {
        let internal = if name.contains('/') {
            name.to_owned()
        } else if name.contains('.') {
            binary_to_internal(name)
        } else {
            format!("java/lang/{name}")
        };

        if let Some(stub) = self
            .by_internal
            .lock()
            .expect("mutex poisoned")
            .get(&internal)
            .cloned()
        {
            return Ok(Some(stub));
        }

        if is_non_type_classfile(&internal) {
            return Ok(None);
        }

        if self
            .missing
            .lock()
            .expect("mutex poisoned")
            .contains(&internal)
        {
            return Ok(None);
        }

        if let Some(container_idx) = self
            .class_to_container
            .lock()
            .expect("mutex poisoned")
            .get(&internal)
            .copied()
        {
            if let Some(stub) = self.load_stub_from_container(container_idx, &internal)? {
                return Ok(Some(stub));
            }
        }

        // Lazily index containers until we locate the class. This avoids opening
        // and scanning every container for each lookup.
        let mut found_container = None;
        for container_idx in 0..self.containers.len() {
            self.ensure_container_indexed(container_idx)?;
            let container = self
                .class_to_container
                .lock()
                .expect("mutex poisoned")
                .get(&internal)
                .copied();

            if container.is_some() {
                found_container = container;
                break;
            }
        }

        if let Some(container_idx) = found_container {
            if let Some(stub) = self.load_stub_from_container(container_idx, &internal)? {
                return Ok(Some(stub));
            }
        }

        self.missing
            .lock()
            .expect("mutex poisoned")
            .insert(internal);
        Ok(None)
    }

    /// Read the raw `.class` bytes for a type by *internal* name, e.g.
    /// `java/lang/String`.
    ///
    /// This uses the same lazy container indexing strategy as [`Self::lookup_type`]
    /// and will not scan every platform container on every call.
    pub fn read_class_bytes(&self, internal_name: &str) -> Result<Option<Vec<u8>>, JdkIndexError> {
        if is_non_type_classfile(internal_name) {
            return Ok(None);
        }

        if self
            .missing
            .lock()
            .expect("mutex poisoned")
            .contains(internal_name)
        {
            return Ok(None);
        }

        if let Some(container_idx) = self
            .class_to_container
            .lock()
            .expect("mutex poisoned")
            .get(internal_name)
            .copied()
        {
            if let Some(bytes) =
                self.load_class_bytes_from_container(container_idx, internal_name)?
            {
                return Ok(Some(bytes));
            }
        }

        // Lazily index containers until we locate the class. This avoids opening
        // and scanning every platform container for each lookup.
        let mut found_container = None;
        for container_idx in 0..self.containers.len() {
            self.ensure_container_indexed(container_idx)?;
            let container = self
                .class_to_container
                .lock()
                .expect("mutex poisoned")
                .get(internal_name)
                .copied();

            if container.is_some() {
                found_container = container;
                break;
            }
        }

        if let Some(container_idx) = found_container {
            if let Some(bytes) =
                self.load_class_bytes_from_container(container_idx, internal_name)?
            {
                return Ok(Some(bytes));
            }
        }

        self.missing
            .lock()
            .expect("mutex poisoned")
            .insert(internal_name.to_owned());
        Ok(None)
    }

    /// All types in the implicit `java.lang.*` universe scope.
    pub fn java_lang_symbols(&self) -> Result<Vec<Arc<JdkClassStub>>, JdkIndexError> {
        if let Some(cached) = self.java_lang.get() {
            return Ok(cached.clone());
        }

        // `java.lang` lives in `java.base` (JPMS) / `rt.jar` (legacy). Avoid
        // scanning all containers just to populate the universe.
        let java_lang_container_idx = if self.module_graph.is_some() {
            self.containers
                .iter()
                .position(|c| {
                    c.kind
                        .module_name()
                        .is_some_and(|name| name.as_str() == JAVA_BASE)
                })
                .unwrap_or(0)
        } else {
            0
        };
        self.ensure_container_indexed(java_lang_container_idx)?;

        let internal_names: Vec<String> = self
            .class_to_container
            .lock()
            .expect("mutex poisoned")
            .keys()
            .filter(|internal| {
                internal.starts_with("java/lang/") && is_direct_java_lang_member(internal)
            })
            .cloned()
            .collect();

        let mut out = Vec::new();
        for internal in internal_names {
            if let Some(stub) = self.lookup_type(&internal)? {
                out.push(stub);
            }
        }

        out.sort_by(|a, b| a.binary_name.cmp(&b.binary_name));

        // Best-effort cache set; it's okay if another thread won the race.
        let _ = self.java_lang.set(out.clone());
        Ok(out)
    }

    /// All packages present in the JDK module set.
    pub fn packages(&self) -> Result<Vec<String>, JdkIndexError> {
        Ok(self.packages_sorted()?.clone())
    }

    pub fn packages_with_prefix(&self, prefix: &str) -> Result<Vec<String>, JdkIndexError> {
        let prefix = normalize_binary_prefix(prefix);
        let pkgs = self.packages_sorted()?;

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

    pub fn class_names_with_prefix(&self, prefix: &str) -> Result<Vec<String>, JdkIndexError> {
        let prefix = normalize_binary_prefix(prefix);
        let names = self.binary_names_sorted()?;

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

    fn insert_stub(&self, stub: Arc<JdkClassStub>) {
        self.by_internal
            .lock()
            .expect("mutex poisoned")
            .insert(stub.internal_name.clone(), stub.clone());
        self.by_binary
            .lock()
            .expect("mutex poisoned")
            .insert(stub.binary_name.clone(), stub);
    }

    fn ensure_container_indexed(&self, container_idx: usize) -> Result<(), JdkIndexError> {
        self.containers[container_idx]
            .indexed
            .get_or_try_init(|| self.index_container(container_idx))?;
        Ok(())
    }

    fn index_container(&self, container_idx: usize) -> Result<(), JdkIndexError> {
        let container = &self.containers[container_idx];

        let class_names: Vec<String> = match &container.kind {
            JdkContainerKind::JmodModule { .. } => {
                let archive = jmod::open_archive(&container.path)?;
                archive
                    .file_names()
                    .filter_map(|name| {
                        let internal = jmod::entry_to_internal_name(name)?;
                        if is_non_type_classfile(internal) {
                            None
                        } else {
                            Some(internal.to_owned())
                        }
                    })
                    .collect()
            }
            JdkContainerKind::Jar => {
                let archive = jar::open_archive(&container.path)?;
                archive
                    .file_names()
                    .filter_map(|name| {
                        let internal = jar::entry_to_internal_name(name)?;
                        if is_non_type_classfile(internal) {
                            None
                        } else {
                            Some(internal.to_owned())
                        }
                    })
                    .collect()
            }
        };

        let mut map = self.class_to_container.lock().expect("mutex poisoned");
        for internal in class_names {
            map.entry(internal).or_insert(container_idx);
        }
        Ok(())
    }

    fn load_stub_from_container(
        &self,
        container_idx: usize,
        internal: &str,
    ) -> Result<Option<Arc<JdkClassStub>>, JdkIndexError> {
        self.ensure_container_indexed(container_idx)?;

        let container = &self.containers[container_idx];
        let Some(bytes) = (match &container.kind {
            JdkContainerKind::JmodModule { .. } => {
                jmod::read_class_bytes(&container.path, internal)?
            }
            JdkContainerKind::Jar => jar::read_class_bytes(&container.path, internal)?,
        }) else {
            // Stale mapping (e.g. mutated filesystem). Remove and treat as not found.
            self.class_to_container
                .lock()
                .expect("mutex poisoned")
                .remove(internal);
            return Ok(None);
        };

        let class_file = ClassFile::parse(&bytes)?;
        if is_non_type_classfile(&class_file.this_class) {
            return Ok(None);
        }

        let stub = Arc::new(classfile_to_stub(class_file));
        self.insert_stub(stub.clone());
        Ok(Some(stub))
    }

    fn load_class_bytes_from_container(
        &self,
        container_idx: usize,
        internal: &str,
    ) -> Result<Option<Vec<u8>>, JdkIndexError> {
        self.ensure_container_indexed(container_idx)?;

        let container = &self.containers[container_idx];
        let Some(bytes) = (match &container.kind {
            JdkContainerKind::JmodModule { .. } => {
                jmod::read_class_bytes(&container.path, internal)?
            }
            JdkContainerKind::Jar => jar::read_class_bytes(&container.path, internal)?,
        }) else {
            // Stale mapping (e.g. mutated filesystem). Remove and treat as not found.
            self.class_to_container
                .lock()
                .expect("mutex poisoned")
                .remove(internal);
            return Ok(None);
        };

        Ok(Some(bytes))
    }

    fn packages_sorted(&self) -> Result<&Vec<String>, JdkIndexError> {
        if let Some(pkgs) = self.packages.get() {
            return Ok(pkgs);
        }

        let mut set = BTreeSet::new();
        for container_idx in 0..self.containers.len() {
            self.ensure_container_indexed(container_idx)?;
        }

        for internal in self
            .class_to_container
            .lock()
            .expect("mutex poisoned")
            .keys()
        {
            if let Some((pkg, _)) = internal.rsplit_once('/') {
                set.insert(internal_to_binary(pkg));
            }
        }

        let pkgs: Vec<String> = set.into_iter().collect();
        let _ = self.packages.set(pkgs);
        Ok(self
            .packages
            .get()
            .expect("packages OnceLock should be initialized"))
    }

    fn binary_names_sorted(&self) -> Result<&Vec<String>, JdkIndexError> {
        if let Some(names) = self.binary_names_sorted.get() {
            return Ok(names);
        }

        for container_idx in 0..self.containers.len() {
            self.ensure_container_indexed(container_idx)?;
        }

        let mut names: Vec<String> = self
            .class_to_container
            .lock()
            .expect("mutex poisoned")
            .keys()
            .map(|internal| internal_to_binary(internal))
            .collect();
        names.sort();
        names.dedup();

        let _ = self.binary_names_sorted.set(names);
        Ok(self
            .binary_names_sorted
            .get()
            .expect("binary_names_sorted OnceLock should be initialized"))
    }
}

#[derive(Debug, Error)]
pub enum JdkIndexError {
    #[error(transparent)]
    Discovery(#[from] JdkDiscoveryError),

    #[error("`jmods/` directory not found at `{dir}`")]
    MissingJmodsDir { dir: PathBuf },

    #[error("no `.jmod` modules found under `{dir}`")]
    NoModulesFound { dir: PathBuf },

    #[error("`module-info.class` not found in `{path}`")]
    MissingModuleInfo { path: PathBuf },

    #[error("`rt.jar` not found at `{path}`")]
    MissingRtJar { path: PathBuf },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),

    #[error(transparent)]
    ClassFile(#[from] nova_classfile::Error),

    #[error(transparent)]
    Jmod(#[from] jmod::JmodError),

    #[error(transparent)]
    Jar(#[from] jar::JarError),

    #[error(transparent)]
    CtSym(#[from] ct_sym::CtSymError),

    #[error("ct.sym does not contain release {release}; available releases: {available:?}")]
    CtSymReleaseNotFound { release: u32, available: Vec<u32> },
}

pub(crate) fn classfile_to_stub(class_file: ClassFile) -> JdkClassStub {
    JdkClassStub {
        binary_name: internal_to_binary(&class_file.this_class),
        internal_name: class_file.this_class,
        access_flags: class_file.access_flags,
        super_internal_name: class_file.super_class,
        interfaces_internal_names: class_file.interfaces,
        signature: class_file.signature,
        fields: class_file
            .fields
            .into_iter()
            .map(|f| JdkFieldStub {
                access_flags: f.access_flags,
                name: f.name,
                descriptor: f.descriptor,
                signature: f.signature,
            })
            .collect(),
        methods: class_file
            .methods
            .into_iter()
            .map(|m| JdkMethodStub {
                access_flags: m.access_flags,
                name: m.name,
                descriptor: m.descriptor,
                signature: m.signature,
            })
            .collect(),
    }
}

pub(crate) fn is_non_type_classfile(internal_name: &str) -> bool {
    internal_name == "module-info"
        || internal_name.ends_with("/module-info")
        || internal_name.ends_with("/package-info")
        || internal_name.ends_with("package-info")
}

pub(crate) fn is_direct_java_lang_member(internal_name: &str) -> bool {
    // Universe scope is only `java.lang.*`, not `java.lang.reflect.*`.
    let rest = internal_name
        .strip_prefix("java/lang/")
        .unwrap_or(internal_name);
    // Also exclude nested classes (`$`) because they are not implicitly
    // imported as unqualified names.
    !rest.contains('/') && !rest.contains('$')
}

pub(crate) fn normalize_binary_prefix(prefix: &str) -> Cow<'_, str> {
    if prefix.contains('/') {
        Cow::Owned(prefix.replace('/', "."))
    } else {
        Cow::Borrowed(prefix)
    }
}
