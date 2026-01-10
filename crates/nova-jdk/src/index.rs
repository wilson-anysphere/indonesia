use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use nova_classfile::ClassFile;
use nova_core::ProjectConfig;
use once_cell::sync::OnceCell;
use thiserror::Error;

use crate::discovery::{JdkDiscoveryError, JdkInstallation};
use crate::jmod;
use crate::stub::{binary_to_internal, internal_to_binary};
use crate::{JdkClassStub, JdkFieldStub, JdkMethodStub};

#[derive(Debug)]
struct JdkModule {
    path: PathBuf,
    indexed: OnceCell<()>,
}

#[derive(Debug)]
pub(crate) struct JdkSymbolIndex {
    modules: Vec<JdkModule>,

    by_internal: Mutex<HashMap<String, Arc<JdkClassStub>>>,
    by_binary: Mutex<HashMap<String, Arc<JdkClassStub>>>,

    class_to_module: Mutex<HashMap<String, usize>>,
    missing: Mutex<HashSet<String>>,

    packages: OnceLock<Vec<String>>,
    java_lang: OnceLock<Vec<Arc<JdkClassStub>>>,
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

        let modules = module_paths
            .into_iter()
            .map(|path| JdkModule {
                path,
                indexed: OnceCell::new(),
            })
            .collect();

        Ok(Self {
            modules,
            by_internal: Mutex::new(HashMap::new()),
            by_binary: Mutex::new(HashMap::new()),
            class_to_module: Mutex::new(HashMap::new()),
            missing: Mutex::new(HashSet::new()),
            packages: OnceLock::new(),
            java_lang: OnceLock::new(),
        })
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
            .position(|m| {
                m.path
                    .file_name()
                    .is_some_and(|n| n == std::ffi::OsStr::new("java.base.jmod"))
            })
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
        if let Some(pkgs) = self.packages.get() {
            return Ok(pkgs.clone());
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
        let _ = self.packages.set(pkgs.clone());
        Ok(pkgs)
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
}

#[derive(Debug, Error)]
pub enum JdkIndexError {
    #[error(transparent)]
    Discovery(#[from] JdkDiscoveryError),

    #[error("`jmods/` directory not found at `{dir}`")]
    MissingJmodsDir { dir: PathBuf },

    #[error("no `.jmod` modules found under `{dir}`")]
    NoModulesFound { dir: PathBuf },

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
        fields: class_file
            .fields
            .into_iter()
            .map(|f| JdkFieldStub {
                access_flags: f.access_flags,
                name: f.name,
                descriptor: f.descriptor,
            })
            .collect(),
        methods: class_file
            .methods
            .into_iter()
            .map(|m| JdkMethodStub {
                access_flags: m.access_flags,
                name: m.name,
                descriptor: m.descriptor,
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
