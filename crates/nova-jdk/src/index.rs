use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use nova_classfile::ClassFile;
use nova_core::ProjectConfig;
use thiserror::Error;

use crate::discovery::{JdkDiscoveryError, JdkInstallation};
use crate::jmod;
use crate::stub::{binary_to_internal, internal_to_binary};
use crate::{JdkClassStub, JdkFieldStub, JdkMethodStub};

#[derive(Debug)]
pub(crate) struct JdkSymbolIndex {
    modules: Vec<PathBuf>,

    by_internal: Mutex<HashMap<String, Arc<JdkClassStub>>>,
    by_binary: Mutex<HashMap<String, Arc<JdkClassStub>>>,

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

        let mut modules: Vec<PathBuf> = std::fs::read_dir(&jmods_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "jmod"))
            .collect();

        // Put `java.base.jmod` first since it's where most core types live.
        modules.sort_by_key(|p| {
            let file_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            (file_name != "java.base.jmod", file_name.to_owned())
        });

        if modules.is_empty() {
            return Err(JdkIndexError::NoModulesFound { dir: jmods_dir });
        }

        Ok(Self {
            modules,
            by_internal: Mutex::new(HashMap::new()),
            by_binary: Mutex::new(HashMap::new()),
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

        for module_path in &self.modules {
            let Some(bytes) = jmod::read_class_bytes(module_path, &internal)? else {
                continue;
            };

            let class_file = ClassFile::parse(&bytes)?;
            if is_non_type_classfile(&class_file.this_class) {
                return Ok(None);
            }

            let stub = Arc::new(classfile_to_stub(class_file));
            self.insert_stub(stub.clone());
            return Ok(Some(stub));
        }

        Ok(None)
    }

    /// All types in the implicit `java.lang.*` universe scope.
    pub fn java_lang_symbols(&self) -> Result<Vec<Arc<JdkClassStub>>, JdkIndexError> {
        if let Some(cached) = self.java_lang.get() {
            return Ok(cached.clone());
        }

        let mut out = Vec::new();
        for module_path in &self.modules {
            let mut archive = jmod::open_archive(module_path)?;

            // We avoid `ZipFile` borrow issues by collecting the relevant entry
            // names first.
            let entry_names: Vec<String> = archive
                .file_names()
                .filter_map(|name| {
                    let internal = jmod::entry_to_internal_name(name)?;
                    if internal.starts_with("java/lang/") && is_direct_java_lang_member(internal) {
                        Some(name.to_owned())
                    } else {
                        None
                    }
                })
                .collect();

            for entry_name in entry_names {
                let internal = jmod::entry_to_internal_name(&entry_name)
                    .expect("entry name came from entry_to_internal_name")
                    .to_owned();

                if let Some(stub) = self
                    .by_internal
                    .lock()
                    .expect("mutex poisoned")
                    .get(&internal)
                    .cloned()
                {
                    out.push(stub);
                    continue;
                }

                let mut zf = archive.by_name(&entry_name)?;
                let mut bytes = Vec::with_capacity(zf.size() as usize);
                std::io::Read::read_to_end(&mut zf, &mut bytes)?;

                let class_file = ClassFile::parse(&bytes)?;
                if is_non_type_classfile(&class_file.this_class) {
                    continue;
                }

                let stub = Arc::new(classfile_to_stub(class_file));
                self.insert_stub(stub.clone());
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
        for module_path in &self.modules {
            let archive = jmod::open_archive(module_path)?;
            for entry_name in archive.file_names() {
                let internal = match jmod::entry_to_internal_name(entry_name) {
                    Some(v) => v,
                    None => continue,
                };
                if is_non_type_classfile(internal) {
                    continue;
                }

                if let Some((pkg, _)) = internal.rsplit_once('/') {
                    set.insert(internal_to_binary(pkg));
                }
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
