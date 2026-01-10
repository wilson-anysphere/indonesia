use std::collections::HashMap;
use std::sync::Arc;

use lsp_types::Uri;

use nova_index::{InheritanceEdge, InheritanceIndex};

use crate::parse::parse_file;
use crate::parse::{ParsedFile, TypeDef};

#[derive(Debug, Clone)]
pub struct TypeInfo {
    pub uri: Uri,
    pub def: TypeDef,
}

#[derive(Debug, Default)]
struct DatabaseData {
    files: HashMap<Uri, ParsedFile>,
    types: HashMap<String, TypeInfo>,
    inheritance: InheritanceIndex,
}

/// A mutable semantic database. Mutations create a new internal snapshot.
#[derive(Debug, Default)]
pub struct Database {
    data: Arc<DatabaseData>,
}

/// An immutable point-in-time view of the database.
#[derive(Debug, Clone)]
pub struct DatabaseSnapshot {
    data: Arc<DatabaseData>,
}

impl Database {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_file_content(&mut self, uri: Uri, text: impl Into<String>) {
        let text = text.into();

        let mut files = self.data.files.clone();
        let parsed = parse_file(uri.clone(), text);
        files.insert(uri, parsed);

        let mut types: HashMap<String, TypeInfo> = HashMap::new();
        for (file_uri, parsed_file) in &files {
            for ty in &parsed_file.types {
                // Best-effort: keep the first definition if duplicates exist.
                types.entry(ty.name.clone()).or_insert_with(|| TypeInfo {
                    uri: file_uri.clone(),
                    def: ty.clone(),
                });
            }
        }

        let mut inheritance = InheritanceIndex::default();
        let mut edges: Vec<InheritanceEdge> = Vec::new();
        for (file_uri, parsed_file) in &files {
            for ty in &parsed_file.types {
                if let Some(super_class) = &ty.super_class {
                    edges.push(InheritanceEdge {
                        file: file_uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: super_class.clone(),
                    });
                }
                for iface in &ty.interfaces {
                    edges.push(InheritanceEdge {
                        file: file_uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: iface.clone(),
                    });
                }
            }
        }
        inheritance.extend(edges);

        self.data = Arc::new(DatabaseData {
            files,
            types,
            inheritance,
        });
    }

    #[must_use]
    pub fn snapshot(&self) -> DatabaseSnapshot {
        DatabaseSnapshot {
            data: self.data.clone(),
        }
    }
}

impl DatabaseSnapshot {
    pub(crate) fn file(&self, uri: &Uri) -> Option<&ParsedFile> {
        self.data.files.get(uri)
    }

    pub(crate) fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.data.types.get(name)
    }

    pub(crate) fn inheritance(&self) -> &InheritanceIndex {
        &self.data.inheritance
    }
}
