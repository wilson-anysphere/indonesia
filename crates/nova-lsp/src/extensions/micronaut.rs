use std::fs;
use std::path::{Path, PathBuf};

use crate::{NovaLspError, Result};
use nova_framework_micronaut::{analyze_sources_with_config, ConfigFile, JavaSource};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicronautRequest {
    pub project_root: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanDto {
    pub start: usize,
    pub end: usize,
}

impl From<nova_framework_micronaut::Span> for SpanDto {
    fn from(span: nova_framework_micronaut::Span) -> Self {
        Self {
            start: span.start,
            end: span.end,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicronautEndpointItem {
    pub method: String,
    pub path: String,
    pub handler: MicronautHandlerLocation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicronautHandlerLocation {
    pub file: String,
    pub span: SpanDto,
    pub class_name: String,
    pub method_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicronautEndpointsResponse {
    pub schema_version: u32,
    pub endpoints: Vec<MicronautEndpointItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicronautBeanItem {
    pub id: String,
    pub name: String,
    pub ty: String,
    pub kind: String,
    pub qualifiers: Vec<String>,
    pub file: String,
    pub span: SpanDto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicronautBeansResponse {
    pub schema_version: u32,
    pub beans: Vec<MicronautBeanItem>,
}

pub fn handle_endpoints(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: MicronautRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;
    let root = PathBuf::from(&req.project_root);

    let analysis = analyze_root(&root)?;
    let endpoints = analysis
        .endpoints
        .into_iter()
        .map(|e| MicronautEndpointItem {
            method: e.method,
            path: e.path,
            handler: MicronautHandlerLocation {
                file: e.handler.file,
                span: e.handler.span.into(),
                class_name: e.handler.class_name,
                method_name: e.handler.method_name,
            },
        })
        .collect();

    let resp = MicronautEndpointsResponse {
        schema_version: SCHEMA_VERSION,
        endpoints,
    };
    serde_json::to_value(resp)
        .map_err(|err| NovaLspError::Internal(crate::sanitize_serde_json_error(&err)))
}

pub fn handle_beans(params: serde_json::Value) -> Result<serde_json::Value> {
    let req: MicronautRequest = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;
    let root = PathBuf::from(&req.project_root);

    let analysis = analyze_root(&root)?;
    let beans = analysis
        .beans
        .into_iter()
        .map(|b| MicronautBeanItem {
            id: b.id,
            name: b.name,
            ty: b.ty,
            kind: match b.kind {
                nova_framework_micronaut::BeanKind::Class => "class".into(),
                nova_framework_micronaut::BeanKind::FactoryMethod => "factoryMethod".into(),
            },
            qualifiers: b
                .qualifiers
                .into_iter()
                .map(|q| match q {
                    nova_framework_micronaut::Qualifier::Named(name) => format!("Named({name})"),
                    nova_framework_micronaut::Qualifier::Annotation(name) => name,
                })
                .collect(),
            file: b.file,
            span: b.span.into(),
        })
        .collect();

    let resp = MicronautBeansResponse {
        schema_version: SCHEMA_VERSION,
        beans,
    };
    serde_json::to_value(resp)
        .map_err(|err| NovaLspError::Internal(crate::sanitize_serde_json_error(&err)))
}

fn analyze_root(root: &Path) -> Result<nova_framework_micronaut::AnalysisResult> {
    let sources = read_java_sources(root)?;
    let config_files = read_config_files(root)?;
    Ok(analyze_sources_with_config(&sources, &config_files))
}

fn read_java_sources(root: &Path) -> Result<Vec<JavaSource>> {
    let mut java_files = Vec::new();
    collect_files(root, &mut java_files, |path| {
        path.extension().and_then(|e| e.to_str()) == Some("java")
    })?;

    let mut sources = Vec::with_capacity(java_files.len());
    for path in java_files {
        let text = fs::read_to_string(&path)
            .map_err(|err| {
                NovaLspError::Internal(format!(
                    "failed to read {path:?}: {}",
                    crate::sanitize_error_message(&err)
                ))
            })?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        sources.push(JavaSource::new(rel, text));
    }
    Ok(sources)
}

fn read_config_files(root: &Path) -> Result<Vec<ConfigFile>> {
    let mut config_paths = Vec::new();
    collect_files(root, &mut config_paths, |path| {
        matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("application.yml") | Some("application.yaml") | Some("application.properties")
        )
    })?;

    let mut out = Vec::new();
    for path in config_paths {
        let text = fs::read_to_string(&path)
            .map_err(|err| {
                NovaLspError::Internal(format!(
                    "failed to read {path:?}: {}",
                    crate::sanitize_error_message(&err)
                ))
            })?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        match path.extension().and_then(|e| e.to_str()) {
            Some("properties") => out.push(ConfigFile::properties(rel, text)),
            Some("yml") | Some("yaml") => out.push(ConfigFile::yaml(rel, text)),
            _ => {}
        }
    }

    Ok(out)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, pred: fn(&Path) -> bool) -> Result<()> {
    let entries = fs::read_dir(dir)
        .map_err(|err| {
            NovaLspError::Internal(format!(
                "failed to read dir {dir:?}: {}",
                crate::sanitize_error_message(&err)
            ))
        })?;

    for entry in entries {
        let entry = entry
            .map_err(|err| {
                NovaLspError::Internal(format!(
                    "failed to read dir entry: {}",
                    crate::sanitize_error_message(&err)
                ))
            })?;
        let path = entry.path();
        if path.is_dir() {
            // Ignore common noise directories.
            let ignore = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| matches!(name, ".git" | "target" | "build" | "out"));
            if ignore {
                continue;
            }
            collect_files(&path, out, pred)?;
        } else if path.is_file() && pred(&path) {
            out.push(path);
        }
    }

    out.sort();
    out.dedup();
    Ok(())
}
