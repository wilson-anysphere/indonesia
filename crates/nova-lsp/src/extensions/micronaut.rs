use std::fs;
use std::path::{Path, PathBuf};

use crate::{NovaLspError, Result};
use nova_framework_micronaut::{analyze_sources_with_config, ConfigFile, JavaSource};
use serde_json::Value;

pub const SCHEMA_VERSION: u32 = 1;

fn span_value(span: nova_framework_micronaut::Span) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("start".to_string(), Value::from(span.start as u64));
        value.insert("end".to_string(), Value::from(span.end as u64));
        value
    })
}

pub fn handle_endpoints(params: serde_json::Value) -> Result<serde_json::Value> {
    let project_root = super::decode_project_root(params)?;
    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }
    let root = PathBuf::from(&project_root);

    let analysis = analyze_root(&root)?;
    let endpoints = analysis
        .endpoints
        .into_iter()
        .map(|e| {
            Value::Object({
                let mut endpoint = serde_json::Map::new();
                endpoint.insert("method".to_string(), Value::String(e.method));
                endpoint.insert("path".to_string(), Value::String(e.path));
                endpoint.insert(
                    "handler".to_string(),
                    Value::Object({
                        let mut handler = serde_json::Map::new();
                        handler.insert("file".to_string(), Value::String(e.handler.file));
                        handler.insert("span".to_string(), span_value(e.handler.span));
                        handler
                            .insert("className".to_string(), Value::String(e.handler.class_name));
                        handler.insert(
                            "methodName".to_string(),
                            Value::String(e.handler.method_name),
                        );
                        handler
                    }),
                );
                endpoint
            })
        })
        .collect::<Vec<_>>();

    Ok(Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "schemaVersion".to_string(),
            Value::from(u64::from(SCHEMA_VERSION)),
        );
        resp.insert("endpoints".to_string(), Value::Array(endpoints));
        resp
    }))
}

pub fn handle_beans(params: serde_json::Value) -> Result<serde_json::Value> {
    let project_root = super::decode_project_root(params)?;
    if project_root.trim().is_empty() {
        return Err(NovaLspError::InvalidParams(
            "`projectRoot` must not be empty".to_string(),
        ));
    }
    let root = PathBuf::from(&project_root);

    let analysis = analyze_root(&root)?;
    let beans = analysis
        .beans
        .into_iter()
        .map(|b| {
            let kind = match b.kind {
                nova_framework_micronaut::BeanKind::Class => "class",
                nova_framework_micronaut::BeanKind::FactoryMethod => "factoryMethod",
            };
            let qualifiers = b
                .qualifiers
                .into_iter()
                .map(|q| match q {
                    nova_framework_micronaut::Qualifier::Named(name) => format!("Named({name})"),
                    nova_framework_micronaut::Qualifier::Annotation(name) => name,
                })
                .map(Value::String)
                .collect::<Vec<_>>();

            Value::Object({
                let mut bean = serde_json::Map::new();
                bean.insert("id".to_string(), Value::String(b.id));
                bean.insert("name".to_string(), Value::String(b.name));
                bean.insert("ty".to_string(), Value::String(b.ty));
                bean.insert("kind".to_string(), Value::String(kind.to_string()));
                bean.insert("qualifiers".to_string(), Value::Array(qualifiers));
                bean.insert("file".to_string(), Value::String(b.file));
                bean.insert("span".to_string(), span_value(b.span));
                bean
            })
        })
        .collect::<Vec<_>>();

    Ok(Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "schemaVersion".to_string(),
            Value::from(u64::from(SCHEMA_VERSION)),
        );
        resp.insert("beans".to_string(), Value::Array(beans));
        resp
    }))
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
            .map_err(|err| NovaLspError::Internal(format!("failed to read {path:?}: {err}")))?;
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
            .map_err(|err| NovaLspError::Internal(format!("failed to read {path:?}: {err}")))?;
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
        .map_err(|err| NovaLspError::Internal(format!("failed to read dir {dir:?}: {err}")))?;

    for entry in entries {
        let entry = entry
            .map_err(|err| NovaLspError::Internal(format!("failed to read dir entry: {err}")))?;
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
