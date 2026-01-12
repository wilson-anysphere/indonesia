use std::collections::{HashMap, HashSet};

use nova_core::{FileId, Name, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_hir::ids::ItemId;
use nova_modules::ModuleName;

use crate::def_map::DefMap;
use crate::types::TypeDef;

/// A lightweight, workspace-wide definition map.
///
/// This aggregates per-file [`DefMap`] data so resolvers can:
/// - resolve same-package and cross-file type references
/// - prefer workspace types over classpath/JDK types when binary names collide
/// - discover workspace packages for star-import validation and package resolution
///
/// The structure intentionally stays "type namespace only": it tracks top-level
/// types and their nested types, plus a best-effort static-member lookup hook.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceDefMap {
    /// Map of binary type name → representative `ItemId` (first definition wins).
    items_by_type_name: HashMap<TypeName, ItemId>,
    /// Reverse mapping for convenience.
    type_names: HashMap<ItemId, TypeName>,
    /// Full type definitions keyed by `ItemId`.
    types: HashMap<ItemId, TypeDef>,
    /// Top-level type declarations grouped by package.
    ///
    /// We retain a `Vec<ItemId>` for each simple name to keep enough information
    /// around to diagnose duplicates in higher layers.
    top_level_by_package: HashMap<PackageName, HashMap<Name, Vec<ItemId>>>,
    /// Package prefixes present in the workspace.
    packages: HashSet<PackageName>,
    /// Mapping of `FileId` to JPMS module name for files that belong to named modules.
    ///
    /// The absence of an entry is treated as the classpath "unnamed module".
    file_modules: HashMap<FileId, ModuleName>,
}

impl WorkspaceDefMap {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items_by_type_name.is_empty()
    }

    #[must_use]
    pub fn item_by_type_name(&self, name: &TypeName) -> Option<ItemId> {
        self.items_by_type_name.get(name).copied()
    }

    #[must_use]
    pub fn item_by_type_name_str(&self, name: &str) -> Option<ItemId> {
        self.items_by_type_name.get(name).copied()
    }

    #[must_use]
    pub fn type_name(&self, item: ItemId) -> Option<&TypeName> {
        self.type_names.get(&item)
    }

    #[must_use]
    pub fn type_def(&self, item: ItemId) -> Option<&TypeDef> {
        self.types.get(&item)
    }

    #[must_use]
    pub fn module_for_item(&self, item: ItemId) -> Option<&ModuleName> {
        self.file_modules.get(&item.file())
    }

    /// Iterate all unique binary type names known to this workspace map.
    ///
    /// The iteration order is unspecified; callers that need deterministic
    /// ordering should sort by `TypeName::as_str()` (or call [`Self::iter_type_names`]).
    pub fn all_type_names(&self) -> impl Iterator<Item = &TypeName> + '_ {
        self.items_by_type_name.keys()
    }

    /// Iterate all unique workspace type binary names in deterministic order.
    ///
    /// The returned iterator yields binary names (`java.lang.String`,
    /// `com.example.Outer$Inner`, etc) sorted lexicographically by
    /// [`TypeName::as_str`].
    ///
    /// This is intended for callers that want to deterministically pre-intern all
    /// workspace types into a project-level type environment without re-parsing
    /// every file.
    pub fn iter_type_names(&self) -> impl Iterator<Item = &TypeName> + '_ {
        let mut names: Vec<&TypeName> = self.all_type_names().collect();
        names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        names.into_iter()
    }

    /// Return all JPMS modules that define at least one top-level type in `package`.
    ///
    /// The result is de-duplicated and treats missing module metadata as the
    /// classpath "unnamed module".
    #[must_use]
    pub fn modules_defining_package(&self, package: &PackageName) -> Vec<ModuleName> {
        let Some(entries) = self.top_level_by_package.get(package) else {
            return Vec::new();
        };
        let mut out = std::collections::BTreeSet::<ModuleName>::new();
        for items in entries.values() {
            for item in items {
                out.insert(
                    self.module_for_item(*item)
                        .cloned()
                        .unwrap_or_else(ModuleName::unnamed),
                );
            }
        }
        out.into_iter().collect()
    }

    /// Insert definitions from a single-file [`DefMap`].
    pub fn extend_from_def_map(&mut self, def_map: &DefMap) {
        self.extend_from_def_map_with_module(def_map, ModuleName::unnamed());
    }

    /// Insert definitions from a single-file [`DefMap`] and record its JPMS module.
    ///
    /// `module` should be the named module containing the file, or the sentinel
    /// unnamed module (`ModuleName::unnamed()`) if the file is on the classpath.
    pub fn extend_from_def_map_with_module(&mut self, def_map: &DefMap, module: ModuleName) {
        // Avoid storing the unnamed module for every file; callers can treat a
        // missing entry as "unnamed".
        if !module.is_unnamed() {
            self.file_modules.insert(def_map.file(), module);
        }

        let package = def_map.package().cloned().unwrap_or_else(PackageName::root);
        self.register_package_prefixes(&package);

        for (item, def) in def_map.iter_type_defs() {
            let ty_name = def.binary_name.clone();
            self.type_names.insert(item, ty_name.clone());
            self.types.insert(item, def.clone());

            // Prefer the first definition encountered (callers should iterate
            // project files in stable order).
            self.items_by_type_name.entry(ty_name).or_insert(item);

            if def.enclosing.is_none() {
                self.top_level_by_package
                    .entry(package.clone())
                    .or_default()
                    .entry(def.name.clone())
                    .or_default()
                    .push(item);
            }
        }
    }

    fn register_package_prefixes(&mut self, package: &PackageName) {
        // Include the root package.
        self.packages.insert(PackageName::root());

        let mut current = PackageName::root();
        for seg in package.segments() {
            current.push(seg.clone());
            self.packages.insert(current.clone());
        }
    }

    /// Best-effort estimate of heap memory usage of this workspace definition map in bytes.
    ///
    /// This is intended for cheap, deterministic memory accounting (e.g. Nova's
    /// `nova-memory` budgets). The heuristic is not exact; it intentionally
    /// prioritizes stability over precision.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        fn name_bytes(name: &Name) -> u64 {
            name.as_str().len() as u64
        }

        fn package_bytes(pkg: &PackageName) -> u64 {
            let mut bytes = size_of::<PackageName>() as u64;
            bytes = bytes.saturating_add(
                (pkg.segments().len() as u64).saturating_mul(size_of::<Name>() as u64),
            );
            for seg in pkg.segments() {
                bytes = bytes.saturating_add(name_bytes(seg));
            }
            bytes
        }

        let mut bytes = 0u64;

        // items_by_type_name: HashMap<TypeName, ItemId>
        bytes = bytes.saturating_add(
            (self.items_by_type_name.capacity() as u64)
                .saturating_mul(size_of::<(TypeName, ItemId)>() as u64),
        );
        bytes = bytes.saturating_add(self.items_by_type_name.capacity() as u64);
        for (name, _) in &self.items_by_type_name {
            bytes = bytes.saturating_add(name.as_str().len() as u64);
        }

        // type_names: HashMap<ItemId, TypeName>
        bytes = bytes.saturating_add(
            (self.type_names.capacity() as u64)
                .saturating_mul(size_of::<(ItemId, TypeName)>() as u64),
        );
        bytes = bytes.saturating_add(self.type_names.capacity() as u64);
        for (_, name) in &self.type_names {
            bytes = bytes.saturating_add(name.as_str().len() as u64);
        }

        // types: HashMap<ItemId, TypeDef>
        bytes = bytes.saturating_add((self.types.capacity() as u64).saturating_mul(size_of::<(
            ItemId,
            TypeDef,
        )>()
            as u64));
        bytes = bytes.saturating_add(self.types.capacity() as u64);
        for (_, def) in &self.types {
            bytes = bytes.saturating_add(def.estimated_bytes());
        }

        // top_level_by_package: HashMap<PackageName, HashMap<Name, Vec<ItemId>>>
        bytes = bytes.saturating_add(
            (self.top_level_by_package.capacity() as u64)
                .saturating_mul(size_of::<(PackageName, HashMap<Name, Vec<ItemId>>)>() as u64),
        );
        bytes = bytes.saturating_add(self.top_level_by_package.capacity() as u64);
        for (pkg, entries) in &self.top_level_by_package {
            bytes = bytes.saturating_add(package_bytes(pkg));
            bytes = bytes.saturating_add((entries.capacity() as u64).saturating_mul(size_of::<(
                Name,
                Vec<ItemId>,
            )>()
                as u64));
            bytes = bytes.saturating_add(entries.capacity() as u64);
            for (name, items) in entries {
                bytes = bytes.saturating_add(name_bytes(name));
                bytes =
                    bytes.saturating_add(
                        (items.capacity() as u64).saturating_mul(size_of::<ItemId>() as u64),
                    );
            }
        }

        // packages: HashSet<PackageName>
        bytes = bytes.saturating_add(
            (self.packages.capacity() as u64).saturating_mul(size_of::<PackageName>() as u64),
        );
        bytes = bytes.saturating_add(self.packages.capacity() as u64);
        for pkg in &self.packages {
            bytes = bytes.saturating_add(package_bytes(pkg));
        }

        // file_modules: HashMap<FileId, ModuleName>
        bytes = bytes.saturating_add(
            (self.file_modules.capacity() as u64)
                .saturating_mul(size_of::<(FileId, ModuleName)>() as u64),
        );
        bytes = bytes.saturating_add(self.file_modules.capacity() as u64);
        for (_, module) in &self.file_modules {
            bytes = bytes.saturating_add(module.as_str().len() as u64);
        }

        bytes
    }
}

impl TypeIndex for WorkspaceDefMap {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        let key = TypeName::new(name.to_dotted());
        self.items_by_type_name.contains_key(&key).then_some(key)
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        let candidates = self
            .top_level_by_package
            .get(package)
            .and_then(|m| m.get(name))?;
        let first = candidates.first().copied()?;
        self.type_names.get(&first).cloned()
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.packages.contains(package)
    }

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        let item = self.items_by_type_name.get(owner).copied()?;
        let ty = self.types.get(&item)?;
        let has_static_field = ty.fields.get(name).is_some_and(|f| f.is_static);
        let has_static_method = ty
            .methods
            .get(name)
            .is_some_and(|methods| methods.iter().any(|m| m.is_static));
        if has_static_field || has_static_method {
            Some(StaticMemberId::new(format!(
                "{}::{}",
                owner.as_str(),
                name.as_str()
            )))
        } else {
            None
        }
    }
}
