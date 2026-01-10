//! JPMS-aware resolution helpers.
//!
//! Nova's main resolver is built around [`nova_core::TypeIndex`]. That index does
//! not yet encode module membership for individual packages/types, so the
//! JPMS-aware checks implemented here are intentionally best-effort and geared
//! toward validating module visibility semantics using fixture workspaces.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use nova_hir::module_info::{lower_module_info_source, ModuleInfoLowerError};
use nova_modules::{ModuleGraph, ModuleInfo, ModuleName};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDef {
    pub module: ModuleName,
    pub package: String,
    pub name: String,
}

impl TypeDef {
    pub fn fqcn(&self) -> String {
        if self.package.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.package, self.name)
        }
    }
}

#[derive(Debug)]
pub struct Workspace {
    graph: ModuleGraph,
    module_roots: HashMap<ModuleName, PathBuf>,
    types: HashMap<String, TypeDef>,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("failed to read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Lower(#[from] ModuleInfoLowerError),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ResolveError {
    #[error("type `{fqcn}` not found")]
    NotFound { fqcn: String },
    #[error("module `{from}` does not read `{to}`")]
    NotReadable { from: ModuleName, to: ModuleName },
    #[error("module `{exporter}` does not export package `{package}` to `{to}`")]
    NotExported {
        exporter: ModuleName,
        package: String,
        to: ModuleName,
    },
}

impl Workspace {
    pub fn load_from_dir(root: &Path) -> Result<Self, WorkspaceError> {
        let mut graph = ModuleGraph::new();
        let mut module_roots = HashMap::new();

        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.file_name() != "module-info.java" {
                continue;
            }

            let path = entry.path().to_path_buf();
            let src = fs::read_to_string(&path).map_err(|source| WorkspaceError::ReadFile {
                path: path.clone(),
                source,
            })?;

            let info = lower_module_info_source(&src)?;
            let module_root = path
                .parent()
                .ok_or_else(|| WorkspaceError::ReadFile {
                    path: path.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "module-info.java has no parent directory",
                    ),
                })?
                .to_path_buf();

            module_roots.insert(info.name.clone(), module_root);
            graph.insert(info);
        }

        let mut types = HashMap::new();
        for (module, module_root) in &module_roots {
            index_java_sources(module, module_root, &mut types)?;
        }

        Ok(Self {
            graph,
            module_roots,
            types,
        })
    }

    pub fn module(&self, name: &str) -> Option<&ModuleName> {
        self.graph
            .iter()
            .find(|(module, _)| module.as_str() == name)
            .map(|(module, _)| module)
    }

    pub fn module_info(&self, module: &ModuleName) -> Option<&ModuleInfo> {
        self.graph.get(module)
    }

    pub fn module_root(&self, module: &ModuleName) -> Option<&Path> {
        self.module_roots.get(module).map(PathBuf::as_path)
    }

    pub fn resolve_fqcn(&self, from: &ModuleName, fqcn: &str) -> Result<&TypeDef, ResolveError> {
        let Some(def) = self.types.get(fqcn) else {
            return Err(ResolveError::NotFound {
                fqcn: fqcn.to_string(),
            });
        };

        if &def.module == from {
            return Ok(def);
        }

        if !self.graph.can_read(from, &def.module) {
            return Err(ResolveError::NotReadable {
                from: from.clone(),
                to: def.module.clone(),
            });
        }

        let Some(exporter_info) = self.graph.get(&def.module) else {
            return Ok(def);
        };

        if !exporter_info.exports_package_to(&def.package, from) {
            return Err(ResolveError::NotExported {
                exporter: def.module.clone(),
                package: def.package.clone(),
                to: from.clone(),
            });
        }

        Ok(def)
    }
}

fn index_java_sources(
    module: &ModuleName,
    module_root: &Path,
    types: &mut HashMap<String, TypeDef>,
) -> Result<(), WorkspaceError> {
    for entry in WalkDir::new(module_root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name() == "module-info.java" {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }

        let path = entry.path().to_path_buf();
        let src = fs::read_to_string(&path).map_err(|source| WorkspaceError::ReadFile {
            path: path.clone(),
            source,
        })?;

        let package = parse_package(&src).unwrap_or_default();
        let Some(name) = parse_first_type_name(&src) else {
            continue;
        };

        let def = TypeDef {
            module: module.clone(),
            package,
            name,
        };
        types.insert(def.fqcn(), def);
    }

    Ok(())
}

fn parse_package(src: &str) -> Option<String> {
    for line in src.lines() {
        let line = line.trim();
        if line.starts_with("//") || line.starts_with("/*") || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("package ") {
            let name = rest.trim_end_matches(';').trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn parse_first_type_name(src: &str) -> Option<String> {
    for line in src.lines() {
        let line = line.trim();
        if line.starts_with("//") || line.starts_with("/*") || line.is_empty() {
            continue;
        }

        let tokens: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == '{' || c == '(')
            .filter(|t| !t.is_empty())
            .collect();

        for win in tokens.windows(2) {
            if let [kw, name] = win {
                if matches!(*kw, "class" | "interface" | "enum" | "record") {
                    return Some(name.trim_end_matches('{').to_string());
                }
            }
        }
    }
    None
}

