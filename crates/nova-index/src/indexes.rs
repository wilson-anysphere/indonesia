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

/// Symbol index: name → locations.
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
    pub symbols: BTreeMap<String, Vec<SymbolLocation>>,
}

impl SymbolIndex {
    pub fn insert(&mut self, symbol: impl Into<String>, location: SymbolLocation) {
        self.symbols
            .entry(symbol.into())
            .or_default()
            .push(location);
    }

    pub fn merge_from(&mut self, other: SymbolIndex) {
        self.generation = self.generation.max(other.generation);
        for (symbol, mut locations) in other.symbols {
            self.symbols
                .entry(symbol)
                .or_default()
                .append(&mut locations);
        }
    }

    pub fn invalidate_file(&mut self, file: &str) {
        self.symbols.retain(|_, locations| {
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
            .unwrap_or_default()
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
