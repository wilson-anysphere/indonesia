use std::collections::BTreeSet;
use std::sync::OnceLock;

use nova_types::CompletionItem;
use regex::Regex;

/// Collect config property names from:
/// - `@ConfigProperty(name = "...")` usages in Java sources
/// - `application.properties`-style key/value sources (passed as strings)
pub fn collect_config_property_names(
    java_sources: &[&str],
    property_files: &[&str],
) -> Vec<String> {
    let mut props = BTreeSet::<String>::new();

    static CONFIG_RE: OnceLock<Regex> = OnceLock::new();
    let config_re = CONFIG_RE.get_or_init(|| {
        Regex::new(r#"@(?:[\w$]+\.)*ConfigProperty\s*\([^)]*\bname\s*=\s*"([^"]+)""#)
            .expect("ConfigProperty regex must compile")
    });
    for src in java_sources {
        for cap in config_re.captures_iter(src) {
            props.insert(cap[1].to_string());
        }
    }

    for file in property_files {
        for raw_line in file.lines() {
            if let Some(key) = parse_properties_key(raw_line) {
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

fn parse_properties_key(raw_line: &str) -> Option<&str> {
    let line = raw_line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
        return None;
    }

    // Java `.properties` files accept `=`, `:`, or whitespace as separators.
    // We keep this best-effort and only parse the key portion.
    let mut split_at = None;
    for (idx, ch) in line.char_indices() {
        if ch == '=' || ch == ':' || ch.is_whitespace() {
            split_at = Some(idx);
            break;
        }
    }

    let key = match split_at {
        Some(idx) => &line[..idx],
        None => line,
    };
    let key = key.trim();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}
