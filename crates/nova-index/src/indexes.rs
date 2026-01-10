use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    pub symbols: BTreeMap<String, Vec<SymbolLocation>>,
}

impl SymbolIndex {
    pub fn insert(&mut self, symbol: impl Into<String>, location: SymbolLocation) {
        self.symbols.entry(symbol.into()).or_default().push(location);
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
    pub references: BTreeMap<String, Vec<ReferenceLocation>>,
}

impl ReferenceIndex {
    pub fn insert(&mut self, symbol: impl Into<String>, location: ReferenceLocation) {
        self.references.entry(symbol.into()).or_default().push(location);
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
    edges: Vec<InheritanceEdge>,
    pub subtypes: BTreeMap<String, Vec<String>>,
    pub supertypes: BTreeMap<String, Vec<String>>,
}

impl InheritanceIndex {
    pub fn insert(&mut self, edge: InheritanceEdge) {
        self.edges.push(edge);
        self.rebuild_maps();
    }

    pub fn invalidate_file(&mut self, file: &str) {
        self.edges.retain(|edge| edge.file != file);
        self.rebuild_maps();
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
    pub annotations: BTreeMap<String, Vec<AnnotationLocation>>,
}

impl AnnotationIndex {
    pub fn insert(&mut self, annotation: impl Into<String>, location: AnnotationLocation) {
        self.annotations
            .entry(annotation.into())
            .or_default()
            .push(location);
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
    pub fn invalidate_file(&mut self, file: &str) {
        self.symbols.invalidate_file(file);
        self.references.invalidate_file(file);
        self.inheritance.invalidate_file(file);
        self.annotations.invalidate_file(file);
    }
}
