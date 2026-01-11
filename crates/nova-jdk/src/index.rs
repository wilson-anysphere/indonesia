use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use nova_classfile::{parse_module_info_class, ClassFile};
use nova_core::ProjectConfig;
use nova_modules::{ModuleGraph, ModuleInfo, ModuleName, JAVA_BASE};
use once_cell::sync::OnceCell;
use thiserror::Error;

use crate::discovery::{JdkDiscoveryError, JdkInstallation};
use crate::jmod;
use crate::stub::{binary_to_internal, internal_to_binary};
use crate::{JdkClassStub, JdkFieldStub, JdkMethodStub};

#[derive(Debug)]
struct JdkModule {
    name: ModuleName,
    path: PathBuf,
    indexed: OnceCell<()>,
}

#[derive(Debug)]
pub(crate) struct JdkSymbolIndex {
    modules: Vec<JdkModule>,
    module_graph: ModuleGraph,

    by_internal: Mutex<HashMap<String, Arc<JdkClassStub>>>,
    by_binary: Mutex<HashMap<String, Arc<JdkClassStub>>>,

    class_to_module: Mutex<HashMap<String, usize>>,
    missing: Mutex<HashSet<String>>,

    packages: OnceLock<Vec<String>>,
    java_lang: OnceLock<Vec<Arc<JdkClassStub>>>,
    binary_names_sorted: OnceLock<Vec<String>>,
}

impl JdkSymbolIndex {
    pub fn discover(config: Option<&ProjectConfig>) -> Result<Self, JdkIndexError> {
        let install = JdkInstallation::discover(config)?;
        Self::from_jmods_dir(install.jmods_dir())
    }

    pub fn from_jdk_root(root: impl AsRef<Path>) -> Result<Self, JdkIndexError> {
        let install = JdkInstallation::from_root(root)?;
        Self::from_jmods_dir(install.jmods_dir())
    }

    pub fn from_jmods_dir(jmods_dir: impl AsRef<Path>) -> Result<Self, JdkIndexError> {
        let jmods_dir = jmods_dir.as_ref().to_path_buf();
        if !jmods_dir.is_dir() {
            return Err(JdkIndexError::MissingJmodsDir { dir: jmods_dir });
        }

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

        let mut module_graph = ModuleGraph::new();
        let mut modules = Vec::with_capacity(module_paths.len());
        for path in module_paths {
            let Some(bytes) = jmod::read_module_info_class_bytes(&path)? else {
                return Err(JdkIndexError::MissingModuleInfo { path });
            };
            let info = parse_module_info_class(&bytes)?;
            let name = info.name.clone();
            module_graph.insert(info);

            modules.push(JdkModule {
                name,
                path,
                indexed: OnceCell::new(),
            });
        }

        Ok(Self {
            modules,
            module_graph,
            by_internal: Mutex::new(HashMap::new()),
            by_binary: Mutex::new(HashMap::new()),
            class_to_module: Mutex::new(HashMap::new()),
            missing: Mutex::new(HashSet::new()),
            packages: OnceLock::new(),
            java_lang: OnceLock::new(),
            binary_names_sorted: OnceLock::new(),
        })
    }

    pub fn module_graph(&self) -> &ModuleGraph {
        &self.module_graph
    }

    pub fn module_info(&self, name: &ModuleName) -> Option<&ModuleInfo> {
        self.module_graph.get(name)
    }

    pub fn module_of_type(&self, binary_or_internal: &str) -> Result<Option<ModuleName>, JdkIndexError> {
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

        if let Some(module_idx) = self
            .class_to_module
            .lock()
            .expect("mutex poisoned")
            .get(&internal)
            .copied()
        {
            return Ok(Some(self.modules[module_idx].name.clone()));
        }

        // Lazily index modules until we locate the class. This mirrors
        // `lookup_type` but avoids parsing the classfile itself.
        let mut found_module = None;
        for module_idx in 0..self.modules.len() {
            self.ensure_module_indexed(module_idx)?;
            let module = self
                .class_to_module
                .lock()
                .expect("mutex poisoned")
                .get(&internal)
                .copied();
            if module.is_some() {
                found_module = module;
                break;
            }
        }

        if let Some(module_idx) = found_module {
            return Ok(Some(self.modules[module_idx].name.clone()));
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

        if let Some(module_idx) = self
            .class_to_module
            .lock()
            .expect("mutex poisoned")
            .get(&internal)
            .copied()
        {
            if let Some(stub) = self.load_stub_from_module(module_idx, &internal)? {
                return Ok(Some(stub));
            }
        }

        // Lazily index modules until we locate the class. This avoids opening
        // and scanning every `.jmod` for each lookup.
        let mut found_module = None;
        for module_idx in 0..self.modules.len() {
            self.ensure_module_indexed(module_idx)?;
            let module = self
                .class_to_module
                .lock()
                .expect("mutex poisoned")
                .get(&internal)
                .copied();

            if module.is_some() {
                found_module = module;
                break;
            }
        }

        if let Some(module_idx) = found_module {
            if let Some(stub) = self.load_stub_from_module(module_idx, &internal)? {
                return Ok(Some(stub));
            }
        }

        self.missing
            .lock()
            .expect("mutex poisoned")
            .insert(internal);
        Ok(None)
    }

    /// All types in the implicit `java.lang.*` universe scope.
    pub fn java_lang_symbols(&self) -> Result<Vec<Arc<JdkClassStub>>, JdkIndexError> {
        if let Some(cached) = self.java_lang.get() {
            return Ok(cached.clone());
        }

        // `java.lang` lives in `java.base`; avoid scanning all modules just to
        // populate the universe.
        let java_base_idx = self
            .modules
            .iter()
            .position(|m| m.name.as_str() == JAVA_BASE)
            .unwrap_or(0);
        self.ensure_module_indexed(java_base_idx)?;

        let internal_names: Vec<String> = self
            .class_to_module
            .lock()
            .expect("mutex poisoned")
            .keys()
            .filter(|internal| internal.starts_with("java/lang/") && is_direct_java_lang_member(internal))
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

    fn ensure_module_indexed(&self, module_idx: usize) -> Result<(), JdkIndexError> {
        self.modules[module_idx]
            .indexed
            .get_or_try_init(|| self.index_module(module_idx))?;
        Ok(())
    }

    fn index_module(&self, module_idx: usize) -> Result<(), JdkIndexError> {
        let module_path = &self.modules[module_idx].path;
        let archive = jmod::open_archive(module_path)?;

        let class_names: Vec<String> = archive
            .file_names()
            .filter_map(|name| {
                let internal = jmod::entry_to_internal_name(name)?;
                if is_non_type_classfile(internal) {
                    None
                } else {
                    Some(internal.to_owned())
                }
            })
            .collect();

        let mut map = self.class_to_module.lock().expect("mutex poisoned");
        for internal in class_names {
            map.entry(internal).or_insert(module_idx);
        }
        Ok(())
    }

    fn load_stub_from_module(
        &self,
        module_idx: usize,
        internal: &str,
    ) -> Result<Option<Arc<JdkClassStub>>, JdkIndexError> {
        self.ensure_module_indexed(module_idx)?;

        let module_path = &self.modules[module_idx].path;
        let Some(bytes) = jmod::read_class_bytes(module_path, internal)? else {
            // Stale mapping (e.g. mutated filesystem). Remove and treat as not found.
            self.class_to_module
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

    fn packages_sorted(&self) -> Result<&Vec<String>, JdkIndexError> {
        if let Some(pkgs) = self.packages.get() {
            return Ok(pkgs);
        }

        let mut set = BTreeSet::new();
        for module_idx in 0..self.modules.len() {
            self.ensure_module_indexed(module_idx)?;
        }

        for internal in self
            .class_to_module
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

        for module_idx in 0..self.modules.len() {
            self.ensure_module_indexed(module_idx)?;
        }

        let mut names: Vec<String> = self
            .class_to_module
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

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),

    #[error(transparent)]
    ClassFile(#[from] nova_classfile::Error),

    #[error(transparent)]
    Jmod(#[from] jmod::JmodError),
}

fn classfile_to_stub(class_file: ClassFile) -> JdkClassStub {
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

fn is_non_type_classfile(internal_name: &str) -> bool {
    internal_name == "module-info" || internal_name.ends_with("/module-info")
        || internal_name.ends_with("/package-info")
        || internal_name.ends_with("package-info")
}

fn is_direct_java_lang_member(internal_name: &str) -> bool {
    // Universe scope is only `java.lang.*`, not `java.lang.reflect.*`.
    let rest = internal_name.strip_prefix("java/lang/").unwrap_or(internal_name);
    // Also exclude nested classes (`$`) because they are not implicitly
    // imported as unqualified names.
    !rest.contains('/') && !rest.contains('$')
}

fn normalize_binary_prefix(prefix: &str) -> Cow<'_, str> {
    if prefix.contains('/') {
        Cow::Owned(prefix.replace('/', "."))
    } else {
        Cow::Borrowed(prefix)
    }
}
