use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct SymbolLocation {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
#[serde(rename_all = "snake_case")]
pub enum IndexSymbolKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
    Method,
    Field,
    Constructor,
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
pub struct IndexedSymbol {
    pub qualified_name: String,
    pub kind: IndexSymbolKind,
    pub container_name: Option<String>,
    pub location: SymbolLocation,
    pub ast_id: u32,
}

/// Symbol index: name → definitions.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct SymbolIndex {
    pub generation: u64,
    pub symbols: BTreeMap<String, Vec<IndexedSymbol>>,
}

impl SymbolIndex {
    /// Approximate heap memory usage of this index in bytes.
    ///
    /// This is intended for cheap, deterministic memory accounting (e.g. Nova's
    /// [`nova_memory`] budgets). The heuristic is not exact; it intentionally
    /// prioritizes stability over precision.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        let mut bytes = 0u64;

        // Approximate per-entry storage inside the `BTreeMap` nodes.
        bytes =
            bytes
                .saturating_add((self.symbols.len() as u64).saturating_mul(
                    (size_of::<String>() + size_of::<Vec<IndexedSymbol>>()) as u64,
                ));

        for (symbol, symbols) in &self.symbols {
            bytes = bytes.saturating_add(symbol.capacity() as u64);

            bytes = bytes.saturating_add(
                (symbols.capacity() as u64).saturating_mul(size_of::<IndexedSymbol>() as u64),
            );
            for sym in symbols {
                bytes = bytes.saturating_add(sym.qualified_name.capacity() as u64);
                if let Some(container) = &sym.container_name {
                    bytes = bytes.saturating_add(container.capacity() as u64);
                }
                bytes = bytes.saturating_add(sym.location.file.capacity() as u64);
            }
        }

        bytes
    }

    fn sort_symbols(symbols: &mut [IndexedSymbol]) {
        symbols.sort_by(|a, b| {
            a.qualified_name
                .cmp(&b.qualified_name)
                .then_with(|| a.ast_id.cmp(&b.ast_id))
        });
    }

    pub fn insert(&mut self, symbol: impl Into<String>, sym: IndexedSymbol) {
        let symbols = self.symbols.entry(symbol.into()).or_default();
        symbols.push(sym);
        Self::sort_symbols(symbols);
    }

    pub fn merge_from(&mut self, other: SymbolIndex) {
        self.generation = self.generation.max(other.generation);
        for (symbol, mut symbols) in other.symbols {
            let entry = self.symbols.entry(symbol).or_default();
            entry.append(&mut symbols);
            Self::sort_symbols(entry);
        }
    }

    pub fn invalidate_file(&mut self, file: &str) {
        self.symbols.retain(|_, symbols| {
            symbols.retain(|sym| sym.location.file != file);
            !symbols.is_empty()
        });
    }
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ReferenceLocation {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

/// Reference index: symbol → usages.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ReferenceIndex {
    pub generation: u64,
    pub references: BTreeMap<String, Vec<ReferenceLocation>>,
}

impl ReferenceIndex {
    /// Approximate heap memory usage of this index in bytes.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        let mut bytes = 0u64;

        bytes = bytes
            .saturating_add((self.references.len() as u64).saturating_mul(
                (size_of::<String>() + size_of::<Vec<ReferenceLocation>>()) as u64,
            ));

        for (symbol, locations) in &self.references {
            bytes = bytes.saturating_add(symbol.capacity() as u64);
            bytes = bytes.saturating_add((locations.capacity() as u64).saturating_mul(size_of::<
                ReferenceLocation,
            >(
            )
                as u64));
            for loc in locations {
                bytes = bytes.saturating_add(loc.file.capacity() as u64);
            }
        }

        bytes
    }

    pub fn insert(&mut self, symbol: impl Into<String>, location: ReferenceLocation) {
        self.references
            .entry(symbol.into())
            .or_default()
            .push(location);
    }

    pub fn merge_from(&mut self, other: ReferenceIndex) {
        self.generation = self.generation.max(other.generation);
        for (symbol, mut locations) in other.references {
            self.references
                .entry(symbol)
                .or_default()
                .append(&mut locations);
        }
    }

    pub fn invalidate_file(&mut self, file: &str) {
        self.references.retain(|_, locations| {
            locations.retain(|loc| loc.file != file);
            !locations.is_empty()
        });
    }
}

/// Inheritance index: type → subtypes/supertypes.
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
pub struct InheritanceEdge {
    pub file: String,
    pub subtype: String,
    pub supertype: String,
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct InheritanceIndex {
    pub generation: u64,
    edges: Vec<InheritanceEdge>,
    pub subtypes: BTreeMap<String, Vec<String>>,
    pub supertypes: BTreeMap<String, Vec<String>>,
}

impl InheritanceIndex {
    /// Approximate heap memory usage of this index in bytes.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        let mut bytes = 0u64;

        bytes = bytes.saturating_add(
            (self.edges.capacity() as u64).saturating_mul(size_of::<InheritanceEdge>() as u64),
        );
        for edge in &self.edges {
            bytes = bytes.saturating_add(edge.file.capacity() as u64);
            bytes = bytes.saturating_add(edge.subtype.capacity() as u64);
            bytes = bytes.saturating_add(edge.supertype.capacity() as u64);
        }

        bytes = bytes.saturating_add(estimate_btree_map_string_to_vec_string(&self.subtypes));
        bytes = bytes.saturating_add(estimate_btree_map_string_to_vec_string(&self.supertypes));

        bytes
    }

    pub fn insert(&mut self, edge: InheritanceEdge) {
        self.edges.push(edge);
        self.rebuild_maps();
    }

    pub fn extend(&mut self, edges: impl IntoIterator<Item = InheritanceEdge>) {
        self.edges.extend(edges);
        self.rebuild_maps();
    }

    pub fn merge_from(&mut self, other: InheritanceIndex) {
        self.generation = self.generation.max(other.generation);
        self.edges.extend(other.edges);
        self.rebuild_maps();
    }

    pub fn invalidate_file(&mut self, file: &str) {
        self.edges.retain(|edge| edge.file != file);
        self.rebuild_maps();
    }

    /// Return all known (transitive) subtypes of `base`.
    #[must_use]
    pub fn all_subtypes(&self, base: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = self
            .subtypes
            .get(base)
            .cloned()
            .unwrap_or_else(Vec::new)
            .into_iter()
            .collect();

        while let Some(next) = queue.pop_front() {
            if !seen.insert(next.clone()) {
                continue;
            }
            out.push(next.clone());
            if let Some(children) = self.subtypes.get(&next) {
                for child in children {
                    queue.push_back(child.clone());
                }
            }
        }

        out
    }

    fn rebuild_maps(&mut self) {
        self.subtypes.clear();
        self.supertypes.clear();

        for edge in &self.edges {
            self.subtypes
                .entry(edge.supertype.clone())
                .or_default()
                .push(edge.subtype.clone());
            self.supertypes
                .entry(edge.subtype.clone())
                .or_default()
                .push(edge.supertype.clone());
        }

        // Keep results stable for deterministic tests.
        for children in self.subtypes.values_mut() {
            children.sort();
            children.dedup();
        }
        for parents in self.supertypes.values_mut() {
            parents.sort();
            parents.dedup();
        }
    }
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct AnnotationLocation {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

/// Annotation index: annotation → locations.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct AnnotationIndex {
    pub generation: u64,
    pub annotations: BTreeMap<String, Vec<AnnotationLocation>>,
}

impl AnnotationIndex {
    /// Approximate heap memory usage of this index in bytes.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        let mut bytes = 0u64;

        bytes =
            bytes.saturating_add((self.annotations.len() as u64).saturating_mul(
                (size_of::<String>() + size_of::<Vec<AnnotationLocation>>()) as u64,
            ));

        for (annotation, locations) in &self.annotations {
            bytes = bytes.saturating_add(annotation.capacity() as u64);
            bytes = bytes.saturating_add((locations.capacity() as u64).saturating_mul(size_of::<
                AnnotationLocation,
            >(
            )
                as u64));
            for loc in locations {
                bytes = bytes.saturating_add(loc.file.capacity() as u64);
            }
        }

        bytes
    }

    pub fn insert(&mut self, annotation: impl Into<String>, location: AnnotationLocation) {
        self.annotations
            .entry(annotation.into())
            .or_default()
            .push(location);
    }

    pub fn merge_from(&mut self, other: AnnotationIndex) {
        self.generation = self.generation.max(other.generation);
        for (annotation, mut locations) in other.annotations {
            self.annotations
                .entry(annotation)
                .or_default()
                .append(&mut locations);
        }
    }

    pub fn invalidate_file(&mut self, file: &str) {
        self.annotations.retain(|_, locations| {
            locations.retain(|loc| loc.file != file);
            !locations.is_empty()
        });
    }
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ProjectIndexes {
    pub symbols: SymbolIndex,
    pub references: ReferenceIndex,
    pub inheritance: InheritanceIndex,
    pub annotations: AnnotationIndex,
}

impl ProjectIndexes {
    /// Approximate heap memory usage of all project-level indexes in bytes.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        let mut bytes = 0u64;
        bytes = bytes.saturating_add(self.symbols.estimated_bytes());
        bytes = bytes.saturating_add(self.references.estimated_bytes());
        bytes = bytes.saturating_add(self.inheritance.estimated_bytes());
        bytes = bytes.saturating_add(self.annotations.estimated_bytes());
        bytes
    }

    pub fn set_generation(&mut self, generation: u64) {
        self.symbols.generation = generation;
        self.references.generation = generation;
        self.inheritance.generation = generation;
        self.annotations.generation = generation;
    }

    pub fn invalidate_file(&mut self, file: &str) {
        self.symbols.invalidate_file(file);
        self.references.invalidate_file(file);
        self.inheritance.invalidate_file(file);
        self.annotations.invalidate_file(file);
    }

    pub fn merge_from(&mut self, other: ProjectIndexes) {
        self.symbols.merge_from(other.symbols);
        self.references.merge_from(other.references);
        self.inheritance.merge_from(other.inheritance);
        self.annotations.merge_from(other.annotations);
    }
}

fn estimate_btree_map_string_to_vec_string(map: &BTreeMap<String, Vec<String>>) -> u64 {
    use std::mem::size_of;

    let mut bytes = 0u64;

    bytes = bytes.saturating_add(
        (map.len() as u64).saturating_mul((size_of::<String>() + size_of::<Vec<String>>()) as u64),
    );

    for (key, values) in map {
        bytes = bytes.saturating_add(key.capacity() as u64);
        bytes = bytes
            .saturating_add((values.capacity() as u64).saturating_mul(size_of::<String>() as u64));
        for value in values {
            bytes = bytes.saturating_add(value.capacity() as u64);
        }
    }

    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimated_bytes_defaults_to_zero() {
        let indexes = ProjectIndexes::default();
        assert_eq!(indexes.estimated_bytes(), 0);
    }

    #[test]
    fn estimated_bytes_increases_monotonically() {
        let mut indexes = ProjectIndexes::default();

        let mut prev = indexes.estimated_bytes();

        indexes.symbols.insert(
            "Foo".to_string(),
            IndexedSymbol {
                qualified_name: "Foo".to_string(),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "A.java".to_string(),
                    line: 0,
                    column: 0,
                },
                ast_id: 0,
            },
        );
        let next = indexes.estimated_bytes();
        assert!(next > prev);
        prev = next;

        indexes.references.insert(
            "Bar".to_string(),
            ReferenceLocation {
                file: "A.java".to_string(),
                line: 0,
                column: 0,
            },
        );
        let next = indexes.estimated_bytes();
        assert!(next > prev);
        prev = next;

        indexes.inheritance.insert(InheritanceEdge {
            file: "A.java".to_string(),
            subtype: "Foo".to_string(),
            supertype: "Bar".to_string(),
        });
        let next = indexes.estimated_bytes();
        assert!(next > prev);
        prev = next;

        indexes.annotations.insert(
            "@Anno".to_string(),
            AnnotationLocation {
                file: "A.java".to_string(),
                line: 0,
                column: 0,
            },
        );
        let next = indexes.estimated_bytes();
        assert!(next > prev);
    }
}
