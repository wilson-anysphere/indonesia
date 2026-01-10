mod persist;

use std::borrow::Cow;
use std::collections::{hash_map::DefaultHasher, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use nova_classfile::{parse_module_info_class, ClassFile};
use nova_core::{Name, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_modules::ModuleInfo;
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
            nova_project::ClasspathEntryKind::Directory => ClasspathEntry::ClassDir(value.path.clone()),
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
    pub fn build(entries: &[ClasspathEntry], cache_dir: Option<&Path>) -> Result<Self, ClasspathError> {
        let mut stubs_by_binary = HashMap::new();
        let mut internal_to_binary = HashMap::new();

        for entry in entries {
            let entry = entry.normalize()?;
            let fingerprint = entry.fingerprint()?;

            let stubs = if let Some(cache_dir) = cache_dir {
                persist::load_or_build_entry(cache_dir, &entry, fingerprint, || index_entry(&entry))?
            } else {
                index_entry(&entry)?
            };

            for stub in stubs {
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

fn index_entry(entry: &ClasspathEntry) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
    match entry {
        ClasspathEntry::ClassDir(dir) => index_class_dir(dir),
        ClasspathEntry::Jar(path) => index_zip(path, ZipKind::Jar),
        ClasspathEntry::Jmod(path) => index_zip(path, ZipKind::Jmod),
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

fn read_module_info_from_zip(path: &Path, kind: ZipKind) -> Result<Option<ModuleInfo>, ClasspathError> {
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

fn index_zip(path: &Path, kind: ZipKind) -> Result<Vec<ClasspathClassStub>, ClasspathError> {
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

            let mut out: Vec<ClasspathClassStub> = best.into_values().map(|(_, stub)| stub).collect();
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

#[cfg(test)]
mod tests {
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

    #[test]
    fn lookup_type_from_jar() {
        let tmp = TempDir::new().unwrap();
        let index = ClasspathIndex::build(
            &[ClasspathEntry::Jar(test_jar())],
            Some(tmp.path()),
        )
        .unwrap();

        let foo = index.lookup_binary("com.example.dep.Foo").unwrap();
        let strings = foo
            .methods
            .iter()
            .find(|m| m.name == "strings")
            .unwrap();

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
        let index =
            ClasspathIndex::build(&[ClasspathEntry::ClassDir(test_class_dir())], None).unwrap();
        assert!(index.lookup_binary("com.example.dep.Bar").is_some());
    }

    #[test]
    fn package_prefix_search() {
        let index = ClasspathIndex::build(&[ClasspathEntry::Jar(test_jar())], None).unwrap();
        let pkgs = index.packages_with_prefix("com.example");
        assert!(pkgs.contains(&"com.example.dep".to_string()));
    }

    #[test]
    fn lookup_type_from_jmod() {
        let index = ClasspathIndex::build(&[ClasspathEntry::Jmod(test_jmod())], None).unwrap();
        assert!(index.lookup_binary("java.lang.String").is_some());
        assert!(index.lookup_internal("java/lang/String").is_some());
        assert!(index.packages_with_prefix("java").contains(&"java.lang".to_string()));
    }

    #[test]
    fn prefix_search_accepts_internal_separators() {
        let index = ClasspathIndex::build(&[ClasspathEntry::Jar(test_jar())], None).unwrap();
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
            persist::load_or_build_entry(tmp.path(), &entry, fingerprint, || index_entry(&entry))
                .unwrap();
        assert!(stubs_first.iter().any(|s| s.binary_name == "com.example.dep.Foo"));

        let stubs_cached = persist::load_or_build_entry(tmp.path(), &entry, fingerprint, || {
            panic!("expected cache hit, but builder was invoked")
        })
        .unwrap();

        assert_eq!(stubs_first.len(), stubs_cached.len());
    }

    #[test]
    fn resolve_type_returns_typename() {
        let index = ClasspathIndex::build(&[ClasspathEntry::Jar(test_jar())], None).unwrap();
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
        let index =
            ClasspathIndex::build(&[ClasspathEntry::Jar(test_multirelease_jar())], None).unwrap();
        assert!(index.lookup_binary("com.example.mr.MultiReleaseOnly").is_some());
    }

    #[test]
    fn ignores_versions_directory_without_multi_release_manifest() {
        let index =
            ClasspathIndex::build(&[ClasspathEntry::Jar(test_not_multirelease_jar())], None)
                .unwrap();
        assert!(index.lookup_binary("com.example.mr.MultiReleaseOnly").is_none());
    }
}
