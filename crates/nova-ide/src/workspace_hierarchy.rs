use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::str::FromStr;

use lsp_types::Uri;
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{Database, FileId};
use nova_index::{InheritanceEdge, InheritanceIndex};
use nova_types::Span;

use crate::parse::{parse_file, MethodDef, ParsedFile, TypeDef};

#[derive(Clone, Debug)]
pub(crate) struct TypeInfo {
    pub(crate) file_id: FileId,
    pub(crate) uri: Uri,
    pub(crate) def: TypeDef,
}

#[derive(Clone, Debug)]
pub(crate) struct MethodInfo {
    pub(crate) file_id: FileId,
    pub(crate) uri: Uri,
    pub(crate) type_name: String,
    pub(crate) name: String,
    pub(crate) name_span: Span,
    pub(crate) body_span: Option<Span>,
}

#[derive(Debug, Default)]
pub(crate) struct WorkspaceHierarchyIndex {
    file_ids: Vec<FileId>,
    files: HashMap<FileId, ParsedFile>,
    types: HashMap<String, TypeInfo>,
    inheritance: InheritanceIndex,
    methods: HashMap<(String, String), MethodInfo>,
}

impl WorkspaceHierarchyIndex {
    pub(crate) fn new(db: &dyn Database) -> Self {
        let mut file_ids: Vec<FileId> = db
            .all_file_ids()
            .into_iter()
            .filter(|id| is_java_file(db, *id))
            .collect();
        // Keep iteration deterministic for tests.
        file_ids.sort_by_key(|id| id.to_raw());

        let mut files = HashMap::new();
        for file_id in &file_ids {
            let uri = uri_for_file(db, *file_id);
            let text = db.file_content(*file_id).to_string();
            files.insert(*file_id, parse_file(uri, text));
        }

        let mut types: HashMap<String, TypeInfo> = HashMap::new();
        let mut methods: HashMap<(String, String), MethodInfo> = HashMap::new();

        for file_id in &file_ids {
            let Some(parsed) = files.get(file_id) else {
                continue;
            };

            for ty in &parsed.types {
                types.entry(ty.name.clone()).or_insert_with(|| TypeInfo {
                    file_id: *file_id,
                    uri: parsed.uri.clone(),
                    def: ty.clone(),
                });

                for m in &ty.methods {
                    methods
                        .entry((ty.name.clone(), m.name.clone()))
                        .or_insert_with(|| method_info_from_def(*file_id, &parsed.uri, &ty.name, m));
                }
            }
        }

        let mut inheritance = InheritanceIndex::default();
        let mut edges: Vec<InheritanceEdge> = Vec::new();
        for file_id in &file_ids {
            let Some(parsed) = files.get(file_id) else {
                continue;
            };
            for ty in &parsed.types {
                if let Some(super_class) = &ty.super_class {
                    edges.push(InheritanceEdge {
                        file: parsed.uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: super_class.clone(),
                    });
                }
                for iface in &ty.interfaces {
                    edges.push(InheritanceEdge {
                        file: parsed.uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: iface.clone(),
                    });
                }
            }
        }
        inheritance.extend(edges);

        Self {
            file_ids,
            files,
            types,
            inheritance,
            methods,
        }
    }

    pub(crate) fn file_ids(&self) -> &[FileId] {
        &self.file_ids
    }

    pub(crate) fn file(&self, file_id: FileId) -> Option<&ParsedFile> {
        self.files.get(&file_id)
    }

    pub(crate) fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }

    pub(crate) fn inheritance(&self) -> &InheritanceIndex {
        &self.inheritance
    }

    #[allow(dead_code)]
    pub(crate) fn method_info(&self, type_name: &str, method_name: &str) -> Option<&MethodInfo> {
        self.methods
            .get(&(type_name.to_string(), method_name.to_string()))
    }

    pub(crate) fn resolve_method_definition(
        &self,
        type_name: &str,
        method_name: &str,
    ) -> Option<MethodInfo> {
        let mut visited = BTreeSet::new();
        self.resolve_method_definition_inner(type_name, method_name, &mut visited)
    }

    fn resolve_method_definition_inner(
        &self,
        type_name: &str,
        method_name: &str,
        visited: &mut BTreeSet<String>,
    ) -> Option<MethodInfo> {
        if !visited.insert(type_name.to_string()) {
            return None;
        }

        let type_info = self.type_info(type_name)?;
        if let Some(method) = type_info
            .def
            .methods
            .iter()
            .find(|m| m.name == method_name)
        {
            return Some(method_info_from_def(
                type_info.file_id,
                &type_info.uri,
                &type_info.def.name,
                method,
            ));
        }

        let super_name = type_info.def.super_class.as_deref()?;
        self.resolve_method_definition_inner(super_name, method_name, visited)
    }

    pub(crate) fn resolve_super_types(&self, type_name: &str) -> Vec<String> {
        self.inheritance
            .supertypes
            .get(type_name)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn resolve_sub_types(&self, type_name: &str) -> Vec<String> {
        self.inheritance
            .subtypes
            .get(type_name)
            .cloned()
            .unwrap_or_default()
    }
}

fn method_info_from_def(
    file_id: FileId,
    uri: &Uri,
    type_name: &str,
    method: &MethodDef,
) -> MethodInfo {
    MethodInfo {
        file_id,
        uri: uri.clone(),
        type_name: type_name.to_string(),
        name: method.name.clone(),
        name_span: method.name_span,
        body_span: method.body_span,
    }
}

fn is_java_file(db: &dyn Database, file_id: FileId) -> bool {
    db.file_path(file_id)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
}

fn uri_for_file(db: &dyn Database, file_id: FileId) -> Uri {
    if let Some(path) = db.file_path(file_id) {
        if let Some(uri) = uri_for_path(path) {
            return uri;
        }
    }

    Uri::from_str(&format!("file:///unknown/{}.java", file_id.to_raw()))
        .expect("fallback URI is valid")
}

fn uri_for_path(path: &Path) -> Option<Uri> {
    let abs = AbsPathBuf::new(path.to_path_buf()).ok()?;
    let uri = path_to_file_uri(&abs).ok()?;
    Uri::from_str(&uri).ok()
}
