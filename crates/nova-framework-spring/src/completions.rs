use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::Path;

use nova_types::CompletionItem;

use crate::BeanModel;

/// Completion items for `@Qualifier("...")` bean names.
pub fn qualifier_completions(model: &BeanModel) -> Vec<CompletionItem> {
    let mut items: Vec<_> = model
        .beans
        .iter()
        .flat_map(|b| {
            std::iter::once(CompletionItem {
                label: b.name.clone(),
                detail: Some(b.ty.clone()),
                replace_span: None,
            })
            .chain(b.qualifiers.iter().map(|q| CompletionItem {
                label: q.clone(),
                detail: Some(b.ty.clone()),
                replace_span: None,
            }))
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    items
}

/// Completion items for `@Profile("...")` names.
///
/// Profile discovery is configuration-dependent; for now we return a small stub list.
pub fn profile_completions() -> Vec<CompletionItem> {
    ["dev", "test", "prod"]
        .into_iter()
        .map(|p| CompletionItem {
            label: p.to_string(),
            detail: None,
            replace_span: None,
        })
        .collect()
}

/// Extract property keys from Spring config files (`application*.properties|yml|yaml`).
///
/// This is a best-effort extractor intended for completions in `@Value("${...}")`.
///
/// `files` is a list of `(path, contents)` tuples.
pub fn property_keys_from_configs(files: &[(&str, &str)]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();

    for (path, text) in files {
        // Use `Path::file_name` instead of string splitting to support both POSIX and Windows paths.
        //
        // Note: `std::path::Path` uses host OS semantics, so on non-Windows platforms a Windows
        // path like `C:\foo\bar\application.properties` is treated as a single component. In
        // that case, fall back to normalizing backslashes to forward slashes and try again.
        let file_name = Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path);
        let file_name: Cow<'_, str> = if file_name == *path && path.contains('\\') && !path.contains('/') {
            let normalized = path.replace('\\', "/");
            Cow::Owned(
                Path::new(&normalized)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(file_name)
                    .to_string(),
            )
        } else {
            Cow::Borrowed(file_name)
        };
        let file_name = file_name.as_ref();
        if !file_name.starts_with("application") {
            continue;
        }

        if file_name.ends_with(".properties") {
            keys.extend(parse_properties(text));
        } else if file_name.ends_with(".yml") || file_name.ends_with(".yaml") {
            keys.extend(parse_yaml_keys(text));
        }
    }

    if keys.is_empty() {
        keys.extend([
            "spring.application.name".to_string(),
            "server.port".to_string(),
            "logging.level.root".to_string(),
        ]);
    }

    keys
}

/// Completion items for `@Value("${...}")` property keys.
pub fn value_completions(keys: &BTreeSet<String>) -> Vec<CompletionItem> {
    keys.iter()
        .map(|k| CompletionItem {
            label: k.clone(),
            detail: None,
            replace_span: None,
        })
        .collect()
}

fn parse_properties(text: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let Some((k, _)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if !k.is_empty() {
            keys.insert(k.to_string());
        }
    }
    keys
}

fn parse_yaml_keys(text: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let mut stack: Vec<(usize, String)> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let Some((k, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }

        while let Some((prev_indent, _)) = stack.last() {
            if *prev_indent < indent {
                break;
            }
            stack.pop();
        }
        stack.push((indent, k.to_string()));

        // Only emit concrete key when it has a scalar value.
        if !rest.trim().is_empty() {
            keys.insert(
                stack
                    .iter()
                    .map(|(_, p)| p.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
            );
        }
    }

    keys
}
