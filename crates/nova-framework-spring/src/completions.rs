use std::collections::BTreeSet;

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
        // Extract the file name in a way that works regardless of whether `path` uses
        // POSIX (`/`) or Windows (`\`) separators (or a mix).
        let file_name = path.rsplit(&['/', '\\'][..]).next().unwrap_or(path);
        if !starts_with_ignore_ascii_case(file_name, "application") {
            continue;
        }

        let ext = file_name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
        if ext.eq_ignore_ascii_case("properties") {
            keys.extend(parse_properties(text));
        } else if ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml") {
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

fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
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
