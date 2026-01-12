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
        for entry in nova_properties::parse(file).entries {
            props.insert(entry.key);
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
