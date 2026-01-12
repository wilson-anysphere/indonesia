use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::Uri;

use nova_db::FileId;
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
    uri_to_file_id: HashMap<Uri, FileId>,
    file_id_to_uri: HashMap<FileId, Uri>,
    file_id_to_path: HashMap<FileId, PathBuf>,
    path_to_file_id: HashMap<PathBuf, FileId>,
    file_ids: Vec<FileId>,
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

        // Best-effort stable FileId mapping for callers that want to reuse the richer
        // `nova_ide::code_intelligence` APIs (which operate on `nova_db::Database` + FileId).
        //
        // This is primarily used by `nova-lsp` handler helpers and in-memory fixtures.
        let mut uris: Vec<_> = files.keys().cloned().collect();
        uris.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        let mut uri_to_file_id = HashMap::new();
        let mut file_id_to_uri = HashMap::new();
        let mut file_id_to_path = HashMap::new();
        let mut path_to_file_id = HashMap::new();
        let mut file_ids = Vec::new();
        for (idx, uri) in uris.into_iter().enumerate() {
            let file_id = FileId::from_raw(idx as u32);
            file_ids.push(file_id);
            uri_to_file_id.insert(uri.clone(), file_id);
            file_id_to_uri.insert(file_id, uri.clone());
            if let Ok(url) = url::Url::parse(uri.as_str()) {
                if let Ok(path) = url.to_file_path() {
                    path_to_file_id.insert(path.clone(), file_id);
                    file_id_to_path.insert(file_id, path);
                }
            }
        }

        self.data = Arc::new(DatabaseData {
            files,
            types,
            inheritance,
            uri_to_file_id,
            file_id_to_uri,
            file_id_to_path,
            path_to_file_id,
            file_ids,
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

    /// Best-effort `FileId` lookup for a file URI managed by this database.
    ///
    /// This enables bridging URI-based fixtures to FileId-based IDE helpers.
    #[must_use]
    pub fn file_id_for_uri(&self, uri: &Uri) -> Option<FileId> {
        self.data.uri_to_file_id.get(uri).copied()
    }
}

impl nova_db::Database for DatabaseSnapshot {
    fn file_content(&self, file_id: FileId) -> &str {
        let Some(uri) = self.data.file_id_to_uri.get(&file_id) else {
            return "";
        };
        self.data
            .files
            .get(uri)
            .map(|parsed| parsed.text.as_str())
            .unwrap_or("")
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        self.data
            .file_id_to_path
            .get(&file_id)
            .map(PathBuf::as_path)
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        self.data.file_ids.clone()
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.data.path_to_file_id.get(path).copied()
    }
}
