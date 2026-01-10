use std::collections::BTreeSet;

use nova_types::CompletionItem;
use regex::Regex;

/// Collect config property names from:
/// - `@ConfigProperty(name = "...")` usages in Java sources
/// - `application.properties`-style key/value sources (passed as strings)
pub fn collect_config_property_names(java_sources: &[&str], property_files: &[&str]) -> Vec<String> {
    let mut props = BTreeSet::<String>::new();

    let config_re = Regex::new(r#"@ConfigProperty\s*\(\s*name\s*=\s*"([^"]+)""#).unwrap();
    for src in java_sources {
        for cap in config_re.captures_iter(src) {
            props.insert(cap[1].to_string());
        }
    }

    for file in property_files {
        for raw_line in file.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let key = line.split('=').next().unwrap_or(line).trim();
            if !key.is_empty() {
                props.insert(key.to_string());
            }
        }
    }

    props.into_iter().collect()
}

/// Completion helper for config property names.
pub fn config_property_completions(
    prefix: &str,
    java_sources: &[&str],
    property_files: &[&str],
) -> Vec<CompletionItem> {
    let names = collect_config_property_names(java_sources, property_files);
    names
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .map(CompletionItem::new)
        .collect()
}

