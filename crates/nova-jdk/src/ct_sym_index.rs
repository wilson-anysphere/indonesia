#![allow(dead_code)]

use std::collections::{hash_map::Entry, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use nova_classfile::{parse_module_info_class, ClassFile};
use nova_modules::{ModuleGraph, ModuleInfo, ModuleName, JAVA_BASE};
use zip::ZipArchive;

use crate::ct_sym;
use crate::index::{
    classfile_to_stub, is_direct_java_lang_member, is_non_type_classfile, normalize_binary_prefix,
    record_cache_hit, record_cache_write, IndexingStats, JdkIndexError,
};
use crate::persist;
use crate::stub::{binary_to_internal, internal_to_binary};
use crate::{JdkClassStub, JdkFieldStub, JdkMethodStub};

#[derive(Debug, Clone)]
struct CtSymSelectedEntry {
    zip_path: String,
    ext: ct_sym::CtSymExt,
}

/// A `ct.sym`-backed symbol index scoped to a single `--release` value.
///
/// This is the core building block for JDK `--release` support: it can resolve
/// type/member stubs for a specific Java platform release from a JDK9+ `lib/ct.sym`.
#[derive(Debug)]
pub(crate) struct CtSymReleaseIndex {
    #[allow(dead_code)]
    release: u32,
    ct_sym_path: PathBuf,
    archive: Mutex<ZipArchive<std::fs::File>>,

    modules: Vec<ModuleName>,
    module_graph: Option<ModuleGraph>,

    /// Map internal type names (`java/lang/String`) to the module index in
    /// `modules`.
    class_to_module: HashMap<String, usize>,

    /// Map internal type names (`java/lang/String`) to their zip entry path in
    /// `ct.sym`. This avoids assuming any hard-coded prefix/layout.
    internal_to_zip_path: HashMap<String, String>,

    by_internal: Mutex<HashMap<String, Arc<JdkClassStub>>>,
    by_binary: Mutex<HashMap<String, Arc<JdkClassStub>>>,

    missing: Mutex<HashSet<String>>,

    packages: OnceLock<Vec<String>>,
    java_lang: OnceLock<Vec<Arc<JdkClassStub>>>,
    binary_names_sorted: OnceLock<Vec<String>>,
}

impl CtSymReleaseIndex {
    pub(crate) fn from_ct_sym_path_with_cache(
        ct_sym_path: impl AsRef<Path>,
        release: u32,
        cache_dir: Option<&Path>,
        allow_write: bool,
        stats: Option<&IndexingStats>,
    ) -> Result<Self, JdkIndexError> {
        let ct_sym_path = ct_sym_path.as_ref().to_path_buf();

        let fingerprint = if cache_dir.is_some() {
            Some(persist::ContainerFingerprint::for_path(&ct_sym_path)?)
        } else {
            None
        };

        if let (Some(cache_dir), Some(fingerprint)) = (cache_dir, fingerprint.as_ref()) {
            if let Some(loaded) =
                persist::load_ct_sym_index(cache_dir, &ct_sym_path, release, fingerprint)
            {
                record_cache_hit(stats);
                return from_loaded_ct_sym_index(ct_sym_path, release, loaded);
            }
        }

        let mut archive = ct_sym::open_archive(&ct_sym_path)?;
        let file_names: Vec<String> = archive.file_names().map(|name| name.to_owned()).collect();
        let built = build_release_mapping(&mut archive, &file_names, release)?;

        let module_graph = build_module_graph(&mut archive, &built.module_info_zip_paths)?;

        let mut class_to_module = built.class_to_module;
        let mut internal_to_zip_path = built.internal_to_zip_path;

        if allow_write {
            if let (Some(cache_dir), Some(fingerprint)) = (cache_dir, fingerprint) {
                let modules: Vec<String> = built
                    .modules
                    .iter()
                    .map(|m| m.as_str().to_owned())
                    .collect();

                // Clone the module-info paths (small, one entry per module) so we can still build
                // the in-memory module graph without re-reading the archive.
                let write_ok = persist::store_ct_sym_index(
                    cache_dir,
                    &ct_sym_path,
                    release,
                    fingerprint,
                    modules,
                    &mut class_to_module,
                    &mut internal_to_zip_path,
                    built.module_info_zip_paths.clone(),
                );
                if write_ok {
                    record_cache_write(stats);
                }
            }
        }

        Ok(Self {
            release,
            ct_sym_path,
            archive: Mutex::new(archive),
            modules: built.modules,
            module_graph,
            class_to_module,
            internal_to_zip_path,
            by_internal: Mutex::new(HashMap::new()),
            by_binary: Mutex::new(HashMap::new()),
            missing: Mutex::new(HashSet::new()),
            packages: OnceLock::new(),
            java_lang: OnceLock::new(),
            binary_names_sorted: OnceLock::new(),
        })
    }

    pub(crate) fn from_ct_sym_path(
        ct_sym_path: impl AsRef<Path>,
        release: u32,
    ) -> Result<Self, JdkIndexError> {
        let ct_sym_path = ct_sym_path.as_ref().to_path_buf();
        let mut archive = ct_sym::open_archive(&ct_sym_path)?;

        // Collect file names up-front so we can iterate without holding a borrow
        // on the archive. We also re-use this archive for module-info reads.
        let file_names: Vec<String> = archive.file_names().map(|name| name.to_owned()).collect();
        let built = build_release_mapping(&mut archive, &file_names, release)?;
        let module_graph = build_module_graph(&mut archive, &built.module_info_zip_paths)?;

        Ok(Self {
            release,
            ct_sym_path,
            archive: Mutex::new(archive),
            modules: built.modules,
            module_graph,
            class_to_module: built.class_to_module,
            internal_to_zip_path: built.internal_to_zip_path,
            by_internal: Mutex::new(HashMap::new()),
            by_binary: Mutex::new(HashMap::new()),
            missing: Mutex::new(HashSet::new()),
            packages: OnceLock::new(),
            java_lang: OnceLock::new(),
            binary_names_sorted: OnceLock::new(),
        })
    }
    /// Approximate heap memory usage of this index in bytes.
    ///
    /// This is intended for best-effort integration with `nova-memory`.
    pub(crate) fn estimated_bytes(&self) -> u64 {
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

        fn add_field_stub(bytes: &mut u64, stub: &JdkFieldStub) {
            add_string(bytes, &stub.name);
            add_string(bytes, &stub.descriptor);
            add_opt_string(bytes, &stub.signature);
        }

        fn add_method_stub(bytes: &mut u64, stub: &JdkMethodStub) {
            add_string(bytes, &stub.name);
            add_string(bytes, &stub.descriptor);
            add_opt_string(bytes, &stub.signature);
        }

        fn class_stub_bytes(stub: &JdkClassStub) -> u64 {
            let mut bytes = 0u64;
            add_string(&mut bytes, &stub.internal_name);
            add_string(&mut bytes, &stub.binary_name);
            add_opt_string(&mut bytes, &stub.super_internal_name);
            add_vec_string(&mut bytes, &stub.interfaces_internal_names);
            add_opt_string(&mut bytes, &stub.signature);

            bytes =
                bytes.saturating_add((stub.fields.capacity() * size_of::<JdkFieldStub>()) as u64);
            for field in &stub.fields {
                add_field_stub(&mut bytes, field);
            }

            bytes =
                bytes.saturating_add((stub.methods.capacity() * size_of::<JdkMethodStub>()) as u64);
            for method in &stub.methods {
                add_method_stub(&mut bytes, method);
            }

            bytes
        }

        fn module_info_bytes(info: &ModuleInfo) -> u64 {
            use std::mem::size_of;

            let mut bytes = 0u64;

            // `ModuleName` wraps a `String` but does not expose capacity, so we
            // fall back to `len()` for a best-effort approximation.
            bytes = bytes.saturating_add(info.name.as_str().len() as u64);

            bytes = bytes.saturating_add(
                (info.requires.capacity() * size_of::<nova_modules::Requires>()) as u64,
            );
            for req in &info.requires {
                bytes = bytes.saturating_add(req.module.as_str().len() as u64);
            }

            bytes = bytes.saturating_add(
                (info.exports.capacity() * size_of::<nova_modules::Exports>()) as u64,
            );
            for exports in &info.exports {
                bytes = bytes.saturating_add(exports.package.capacity() as u64);
                bytes =
                    bytes.saturating_add((exports.to.capacity() * size_of::<ModuleName>()) as u64);
                for m in &exports.to {
                    bytes = bytes.saturating_add(m.as_str().len() as u64);
                }
            }

            bytes = bytes
                .saturating_add((info.opens.capacity() * size_of::<nova_modules::Opens>()) as u64);
            for opens in &info.opens {
                bytes = bytes.saturating_add(opens.package.capacity() as u64);
                bytes =
                    bytes.saturating_add((opens.to.capacity() * size_of::<ModuleName>()) as u64);
                for m in &opens.to {
                    bytes = bytes.saturating_add(m.as_str().len() as u64);
                }
            }

            bytes = bytes
                .saturating_add((info.uses.capacity() * size_of::<nova_modules::Uses>()) as u64);
            for uses in &info.uses {
                bytes = bytes.saturating_add(uses.service.capacity() as u64);
            }

            bytes = bytes.saturating_add(
                (info.provides.capacity() * size_of::<nova_modules::Provides>()) as u64,
            );
            for provides in &info.provides {
                bytes = bytes.saturating_add(provides.service.capacity() as u64);
                bytes = bytes.saturating_add(
                    (provides.implementations.capacity() * size_of::<String>()) as u64,
                );
                for imp in &provides.implementations {
                    bytes = bytes.saturating_add(imp.capacity() as u64);
                }
            }

            bytes
        }

        fn lock_best_effort<T>(mutex: &Mutex<T>) -> Option<std::sync::MutexGuard<'_, T>> {
            match mutex.lock() {
                Ok(guard) => Some(guard),
                Err(poisoned) => Some(poisoned.into_inner()),
            }
        }

        let mut bytes = 0u64;

        bytes = bytes.saturating_add(self.ct_sym_path.as_os_str().len() as u64);

        bytes = bytes.saturating_add((self.modules.capacity() * size_of::<ModuleName>()) as u64);
        for module in &self.modules {
            // `ModuleName` wraps a `String`, but doesn't expose capacity; use len() best-effort.
            bytes = bytes.saturating_add(module.as_str().len() as u64);
        }

        if let Some(graph) = &self.module_graph {
            // `ModuleGraph` hides its backing map. Estimate by iterating and
            // counting nested allocations.
            bytes = bytes.saturating_add(size_of::<ModuleGraph>() as u64);
            for (name, info) in graph.iter() {
                bytes = bytes.saturating_add(name.as_str().len() as u64);
                bytes = bytes.saturating_add(module_info_bytes(info));
            }
        }

        bytes = bytes.saturating_add(
            (self.class_to_module.capacity() * size_of::<(String, usize)>()) as u64,
        );
        for k in self.class_to_module.keys() {
            add_string(&mut bytes, k);
        }

        bytes = bytes.saturating_add(
            (self.internal_to_zip_path.capacity() * size_of::<(String, String)>()) as u64,
        );
        for (k, v) in &self.internal_to_zip_path {
            add_string(&mut bytes, k);
            add_string(&mut bytes, v);
        }

        // Cached stubs / misses. `by_internal` and `by_binary` share the same
        // `Arc<JdkClassStub>` allocations, so avoid double-counting stubs by tracking pointer
        // identity.
        let mut seen_stubs: HashSet<*const JdkClassStub> = HashSet::new();
        let mut add_stub = |bytes: &mut u64, stub: &Arc<JdkClassStub>| {
            let ptr = Arc::as_ptr(stub);
            if seen_stubs.insert(ptr) {
                *bytes = bytes.saturating_add(class_stub_bytes(stub.as_ref()));
            }
        };

        if let Some(map) = lock_best_effort(&self.by_internal) {
            bytes = bytes
                .saturating_add((map.capacity() * size_of::<(String, Arc<JdkClassStub>)>()) as u64);
            for (k, stub) in map.iter() {
                add_string(&mut bytes, k);
                add_stub(&mut bytes, stub);
            }
        }

        if let Some(map) = lock_best_effort(&self.by_binary) {
            bytes = bytes
                .saturating_add((map.capacity() * size_of::<(String, Arc<JdkClassStub>)>()) as u64);
            for (k, stub) in map.iter() {
                add_string(&mut bytes, k);
                add_stub(&mut bytes, stub);
            }
        }

        if let Some(set) = lock_best_effort(&self.missing) {
            bytes = bytes.saturating_add((set.capacity() * size_of::<String>()) as u64);
            for s in set.iter() {
                add_string(&mut bytes, s);
            }
        }

        if let Some(packages) = self.packages.get() {
            bytes = bytes.saturating_add((packages.capacity() * size_of::<String>()) as u64);
            for pkg in packages {
                add_string(&mut bytes, pkg);
            }
        }

        if let Some(java_lang) = self.java_lang.get() {
            bytes = bytes
                .saturating_add((java_lang.capacity() * size_of::<Arc<JdkClassStub>>()) as u64);
            for stub in java_lang {
                add_stub(&mut bytes, stub);
            }
        }

        if let Some(names) = self.binary_names_sorted.get() {
            bytes = bytes.saturating_add((names.capacity() * size_of::<String>()) as u64);
            for name in names {
                add_string(&mut bytes, name);
            }
        }

        bytes
    }

    pub(crate) fn evict_caches(&self) {
        use std::mem;
        use std::sync::TryLockError;

        fn try_lock_best_effort<T>(mutex: &Mutex<T>) -> Option<std::sync::MutexGuard<'_, T>> {
            match mutex.try_lock() {
                Ok(guard) => Some(guard),
                Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => None,
            }
        }

        if let Some(mut map) = try_lock_best_effort(&self.by_internal) {
            let _ = mem::take(&mut *map);
        }

        if let Some(mut map) = try_lock_best_effort(&self.by_binary) {
            let _ = mem::take(&mut *map);
        }

        if let Some(mut set) = try_lock_best_effort(&self.missing) {
            let _ = mem::take(&mut *set);
        }
    }
    pub(crate) fn modules(&self) -> &[ModuleName] {
        &self.modules
    }

    pub(crate) fn module_graph(&self) -> Option<&ModuleGraph> {
        self.module_graph.as_ref()
    }
    pub(crate) fn module_info(&self, name: &ModuleName) -> Option<&ModuleInfo> {
        self.module_graph.as_ref()?.get(name)
    }
    pub(crate) fn module_of_type(
        &self,
        binary_or_internal: &str,
    ) -> Result<Option<ModuleName>, JdkIndexError> {
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

        if let Some(module_idx) = self.class_to_module.get(&internal).copied() {
            return Ok(Some(self.modules[module_idx].clone()));
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
    pub(crate) fn lookup_type(
        &self,
        name: &str,
    ) -> Result<Option<Arc<JdkClassStub>>, JdkIndexError> {
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

        let Some(zip_path) = self.internal_to_zip_path.get(&internal) else {
            self.missing
                .lock()
                .expect("mutex poisoned")
                .insert(internal);
            return Ok(None);
        };

        let Some(bytes) = self.read_zip_entry(zip_path)? else {
            self.missing
                .lock()
                .expect("mutex poisoned")
                .insert(internal);
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

    pub(crate) fn read_class_bytes(
        &self,
        internal_name: &str,
    ) -> Result<Option<Vec<u8>>, JdkIndexError> {
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

        let Some(zip_path) = self.internal_to_zip_path.get(internal_name) else {
            self.missing
                .lock()
                .expect("mutex poisoned")
                .insert(internal_name.to_owned());
            return Ok(None);
        };

        let Some(bytes) = self.read_zip_entry(zip_path)? else {
            self.missing
                .lock()
                .expect("mutex poisoned")
                .insert(internal_name.to_owned());
            return Ok(None);
        };

        Ok(Some(bytes))
    }

    pub(crate) fn java_lang_symbols(&self) -> Result<Vec<Arc<JdkClassStub>>, JdkIndexError> {
        if let Some(cached) = self.java_lang.get() {
            return Ok(cached.clone());
        }

        let internal_names: Vec<String> = self
            .class_to_module
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

        let _ = self.java_lang.set(out.clone());
        Ok(out)
    }

    pub(crate) fn packages(&self) -> Result<Vec<String>, JdkIndexError> {
        Ok(self.packages_sorted()?.clone())
    }

    pub(crate) fn binary_package_names(&self) -> Result<&[String], JdkIndexError> {
        Ok(self.packages_sorted()?.as_slice())
    }

    pub(crate) fn packages_with_prefix(&self, prefix: &str) -> Result<Vec<String>, JdkIndexError> {
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

    pub(crate) fn class_names_with_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, JdkIndexError> {
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

    fn read_zip_entry(&self, zip_path: &str) -> Result<Option<Vec<u8>>, JdkIndexError> {
        let mut archive = self.archive.lock().expect("mutex poisoned");
        Ok(ct_sym::read_entry_bytes_from_archive(
            &mut archive,
            zip_path,
        )?)
    }

    fn packages_sorted(&self) -> Result<&Vec<String>, JdkIndexError> {
        if let Some(pkgs) = self.packages.get() {
            return Ok(pkgs);
        }

        let mut set = BTreeSet::new();
        for internal in self.class_to_module.keys() {
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

    pub(crate) fn binary_names_sorted(&self) -> Result<&Vec<String>, JdkIndexError> {
        if let Some(names) = self.binary_names_sorted.get() {
            return Ok(names);
        }

        let mut names: Vec<String> = self
            .class_to_module
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

struct BuiltCtSymReleaseMapping {
    modules: Vec<ModuleName>,
    class_to_module: HashMap<String, usize>,
    internal_to_zip_path: HashMap<String, String>,
    module_info_zip_paths: Vec<String>,
}

fn build_release_mapping(
    _archive: &mut ZipArchive<std::fs::File>,
    file_names: &[String],
    release: u32,
) -> Result<BuiltCtSymReleaseMapping, JdkIndexError> {
    let mut available_releases = BTreeSet::new();
    let mut release_found = false;

    let mut module_entries: HashMap<String, HashMap<String, CtSymSelectedEntry>> = HashMap::new();

    for entry_name in file_names {
        let Some(parsed) = ct_sym::parse_entry_name(entry_name) else {
            continue;
        };

        let ct_sym::CtSymEntry {
            release: entry_release,
            module,
            internal_name,
            zip_path,
            ext,
        } = parsed;

        available_releases.insert(entry_release);
        if entry_release != release {
            continue;
        }

        release_found = true;

        let by_internal = module_entries.entry(module).or_default();
        let selected = CtSymSelectedEntry { zip_path, ext };
        match by_internal.entry(internal_name) {
            Entry::Vacant(v) => {
                v.insert(selected);
            }
            Entry::Occupied(mut o) => {
                // Prefer `.sig` over `.class` if both are present.
                if o.get().ext == ct_sym::CtSymExt::Class && ext == ct_sym::CtSymExt::Sig {
                    o.insert(selected);
                }
            }
        }
    }

    if !release_found {
        return Err(JdkIndexError::CtSymReleaseNotFound {
            release,
            available: available_releases.into_iter().collect(),
        });
    }

    let mut modules: Vec<ModuleName> = module_entries
        .keys()
        .map(|name| ModuleName::new(name.clone()))
        .collect();
    // Stable ordering with `java.base` first (mirrors `.jmod` indexing).
    modules.sort_by(
        |a, b| match (a.as_str() == JAVA_BASE, b.as_str() == JAVA_BASE) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.as_str().cmp(b.as_str()),
        },
    );

    let mut class_to_module = HashMap::new();
    let mut internal_to_zip_path = HashMap::new();
    let mut module_info_zip_paths: Vec<String> = Vec::new();

    for (module_idx, module) in modules.iter().enumerate() {
        let Some(entries) = module_entries.get(module.as_str()) else {
            continue;
        };

        for (internal, selected) in entries {
            if internal == "module-info" {
                module_info_zip_paths.push(selected.zip_path.clone());
                continue;
            }

            if is_non_type_classfile(internal) {
                continue;
            }

            let inserted = match class_to_module.entry(internal.clone()) {
                Entry::Vacant(v) => {
                    v.insert(module_idx);
                    true
                }
                Entry::Occupied(_) => false,
            };

            if inserted {
                internal_to_zip_path.insert(internal.clone(), selected.zip_path.clone());
            }
        }
    }

    Ok(BuiltCtSymReleaseMapping {
        modules,
        class_to_module,
        internal_to_zip_path,
        module_info_zip_paths,
    })
}

fn build_module_graph(
    archive: &mut ZipArchive<std::fs::File>,
    module_info_zip_paths: &[String],
) -> Result<Option<ModuleGraph>, JdkIndexError> {
    if module_info_zip_paths.is_empty() {
        return Ok(None);
    }

    let mut graph = ModuleGraph::new();
    for zip_path in module_info_zip_paths {
        let Some(bytes) = ct_sym::read_entry_bytes_from_archive(archive, zip_path)? else {
            continue;
        };

        if let Ok(info) = parse_module_info_class(&bytes) {
            graph.insert(info);
        }
    }

    Ok(Some(graph))
}

fn from_loaded_ct_sym_index(
    ct_sym_path: PathBuf,
    release: u32,
    loaded: persist::LoadedCtSymIndex,
) -> Result<CtSymReleaseIndex, JdkIndexError> {
    let mut archive = ct_sym::open_archive(&ct_sym_path)?;

    let module_graph = build_module_graph(&mut archive, &loaded.module_info_zip_paths)?;

    let modules: Vec<ModuleName> = loaded.modules.into_iter().map(ModuleName::new).collect();

    Ok(CtSymReleaseIndex {
        release,
        ct_sym_path,
        archive: Mutex::new(archive),
        modules,
        module_graph,
        class_to_module: loaded.class_to_module,
        internal_to_zip_path: loaded.internal_to_zip_path,
        by_internal: Mutex::new(HashMap::new()),
        by_binary: Mutex::new(HashMap::new()),
        missing: Mutex::new(HashSet::new()),
        packages: OnceLock::new(),
        java_lang: OnceLock::new(),
        binary_names_sorted: OnceLock::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;

    use nova_modules::ModuleName;
    use tempfile::tempdir;
    use zip::write::FileOptions;

    use super::CtSymReleaseIndex;
    use crate::index::JdkIndexError;

    fn fake_jdk_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/fake-jdk")
    }

    #[test]
    fn loads_symbols_from_ct_sym_release() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let ct_sym_path = temp.path().join("ct.sym");

        let java_base_jmod = fake_jdk_root().join("jmods/java.base.jmod");
        let string_bytes = crate::jmod::read_class_bytes(&java_base_jmod, "java/lang/String")?
            .expect("fixture should contain java/lang/String");
        let module_info_bytes = crate::jmod::read_module_info_class_bytes(&java_base_jmod)?
            .expect("fixture should contain module-info.class");

        let file = File::create(&ct_sym_path)?;
        let mut zip = zip::ZipWriter::new(file);
        let opts = FileOptions::default().compression_method(zip::CompressionMethod::Stored);

        // Write an invalid `.class` first to ensure we prefer `.sig` stubs.
        zip.start_file("META-INF/sym/8/java.base/java/lang/String.class", opts)?;
        zip.write_all(&[0x00, 0x01, 0x02])?;

        zip.start_file("META-INF/sym/8/java.base/java/lang/String.sig", opts)?;
        zip.write_all(&string_bytes)?;

        zip.start_file("META-INF/sym/8/java.base/module-info.sig", opts)?;
        zip.write_all(&module_info_bytes)?;

        // Also include another release so we can validate filtering + error messages.
        zip.start_file("META-INF/sym/11/java.base/java/lang/String.sig", opts)?;
        zip.write_all(&string_bytes)?;

        zip.finish()?;

        let index = CtSymReleaseIndex::from_ct_sym_path(&ct_sym_path, 8)?;
        assert_eq!(
            index.modules()[0].as_str(),
            "java.base",
            "java.base should be first when present"
        );

        let string = index
            .lookup_type("java.lang.String")?
            .expect("java.lang.String should be present for release 8");
        assert_eq!(string.internal_name, "java/lang/String");
        assert_eq!(string.binary_name, "java.lang.String");
        assert!(index.lookup_type("java/lang/String")?.is_some());
        assert!(
            index.lookup_type("String")?.is_some(),
            "universe-scope lookup"
        );

        let bytes = index
            .read_class_bytes("java/lang/String")?
            .expect("java/lang/String bytes should be present");
        assert!(
            bytes.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]),
            "class files should start with CAFEBABE"
        );

        let graph = index
            .module_graph()
            .expect("module graph should be present when module-info is available");
        assert!(
            graph.get(&ModuleName::new("java.base")).is_some(),
            "module graph should include java.base"
        );

        let module = index
            .module_of_type("java.lang.String")?
            .expect("module_of_type should resolve java.lang.String");
        assert_eq!(module.as_str(), "java.base");

        let pkgs = index.packages()?;
        assert!(pkgs.contains(&"java.lang".to_owned()));
        assert!(index
            .packages_with_prefix("java.l")?
            .contains(&"java.lang".to_owned()));
        assert!(index
            .class_names_with_prefix("java.lang.S")?
            .contains(&"java.lang.String".to_owned()));

        assert!(index.estimated_bytes() > 0);

        Ok(())
    }

    #[test]
    fn errors_when_release_is_missing() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let ct_sym_path = temp.path().join("ct.sym");

        let java_base_jmod = fake_jdk_root().join("jmods/java.base.jmod");
        let string_bytes = crate::jmod::read_class_bytes(&java_base_jmod, "java/lang/String")?
            .expect("fixture should contain java/lang/String");

        let file = File::create(&ct_sym_path)?;
        let mut zip = zip::ZipWriter::new(file);
        let opts = FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/sym/11/java.base/java/lang/String.sig", opts)?;
        zip.write_all(&string_bytes)?;
        zip.finish()?;

        let err = CtSymReleaseIndex::from_ct_sym_path(&ct_sym_path, 8).unwrap_err();
        match err {
            JdkIndexError::CtSymReleaseNotFound { release, available } => {
                assert_eq!(release, 8);
                assert_eq!(available, vec![11]);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        Ok(())
    }
}
