use std::collections::{HashMap, HashSet};

use nova_core::{FileId, Name, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_hir::ids::ItemId;

use crate::DefMap;

/// Project source type index built from per-file [`DefMap`]s.
///
/// This lets `nova-resolve` consult types declared in other source files using the same
/// `TypeIndex` abstraction as the JDK/classpath indices.
///
/// Note: static member lookup is currently unsupported for source types because
/// `nova_hir::item_tree` does not preserve `static` modifiers.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SourceTypeIndex {
    types: HashMap<String, TypeName>,
    package_to_types: HashMap<String, HashMap<String, TypeName>>,
    packages: HashSet<String>,

    /// First-seen (deterministic) winner for each fully-qualified top-level type.
    top_level_owners: HashMap<TypeName, FileId>,
    /// All files that declared the same fully-qualified top-level type.
    top_level_conflicts: HashMap<TypeName, Vec<FileId>>,
}

impl SourceTypeIndex {
    pub fn top_level_conflicts(&self) -> &HashMap<TypeName, Vec<FileId>> {
        &self.top_level_conflicts
    }

    pub fn extend_from_def_map(&mut self, def_map: &DefMap) {
        let file = def_map.file();
        let pkg = def_map
            .package()
            .map(|p| p.to_dotted())
            .unwrap_or_else(String::new);
        self.packages.insert(pkg.clone());

        let mut top_levels: Vec<(&Name, ItemId)> = def_map.top_level_types().collect();
        top_levels.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

        let mut accepted_top_levels = HashSet::<ItemId>::new();

        for (name, id) in top_levels {
            let Some(binary_name) = def_map.binary_name(id) else {
                continue;
            };

            if let Some(&winner) = self.top_level_owners.get(binary_name) {
                let files = self
                    .top_level_conflicts
                    .entry(binary_name.clone())
                    .or_insert_with(|| vec![winner]);
                if !files.contains(&file) {
                    files.push(file);
                }
                continue;
            }

            self.top_level_owners.insert(binary_name.clone(), file);
            self.package_to_types
                .entry(pkg.clone())
                .or_default()
                .insert(name.as_str().to_string(), binary_name.clone());
            accepted_top_levels.insert(id);
        }

        let mut accepted_types: Vec<TypeName> = def_map
            .types()
            .filter_map(|(id, def)| {
                let top = outermost_enclosing(def_map, id);
                accepted_top_levels
                    .contains(&top)
                    .then(|| def.binary_name.clone())
            })
            .collect();
        accepted_types.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        for binary_name in accepted_types {
            self.types
                .entry(binary_name.as_str().to_string())
                .or_insert(binary_name);
        }
    }
}

impl TypeIndex for SourceTypeIndex {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        self.types.get(&name.to_dotted()).cloned()
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        self.package_to_types
            .get(&package.to_dotted())
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.packages.contains(&package.to_dotted())
    }

    fn resolve_static_member(&self, _owner: &TypeName, _name: &Name) -> Option<StaticMemberId> {
        None
    }
}

fn outermost_enclosing(def_map: &DefMap, mut id: ItemId) -> ItemId {
    while let Some(def) = def_map.type_def(id) {
        match def.enclosing {
            Some(parent) => id = parent,
            None => break,
        }
    }
    id
}
