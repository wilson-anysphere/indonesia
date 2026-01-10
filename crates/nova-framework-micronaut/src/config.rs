use nova_types::CompletionItem;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigFileKind {
    Properties,
    Yaml,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    pub path: String,
    pub kind: ConfigFileKind,
    pub text: String,
}

impl ConfigFile {
    pub fn properties(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: ConfigFileKind::Properties,
            text: text.into(),
        }
    }

    pub fn yaml(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: ConfigFileKind::Yaml,
            text: text.into(),
        }
    }
}

pub fn collect_config_keys(files: &[ConfigFile]) -> Vec<String> {
    let mut keys = Vec::new();
    for file in files {
        match file.kind {
            ConfigFileKind::Properties => keys.extend(parse_properties_keys(&file.text)),
            ConfigFileKind::Yaml => keys.extend(parse_yaml_keys(&file.text)),
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

pub fn config_completions(prefix: &str, config_keys: &[String]) -> Vec<CompletionItem> {
    let mut items: Vec<_> = config_keys
        .iter()
        .filter(|k| k.starts_with(prefix))
        .map(|k| CompletionItem::new(k.clone()))
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

#[derive(Clone, Debug)]
struct PlaceholderContext {
    /// Currently typed prefix within the placeholder.
    prefix: String,
}

fn placeholder_context_at(text: &str, offset: usize) -> Option<PlaceholderContext> {
    // Find the enclosing @Value string literal.
    let mut search = 0usize;
    while let Some(rel) = text[search..].find("@Value") {
        let start = search + rel;
        let Some(open_paren) = text[start..].find('(').map(|o| start + o) else {
            break;
        };
        let Some(open_quote) = find_next_unescaped_quote(text, open_paren) else {
            search = open_paren + 1;
            continue;
        };
        let Some(close_quote) = find_next_unescaped_quote(text, open_quote + 1) else {
            search = open_quote + 1;
            continue;
        };

        let content_start = open_quote + 1;
        let content_end = close_quote;
        if offset < content_start || offset > content_end {
            search = close_quote + 1;
            continue;
        }

        let content = &text[content_start..content_end];
        let rel_offset = offset - content_start;
        let Some(open_rel) = content[..rel_offset].rfind("${") else {
            return None;
        };
        let key_start_rel = open_rel + 2;
        if rel_offset < key_start_rel {
            return None;
        }

        let after_key = &content[key_start_rel..];
        let key_end_rel = after_key
            .find(|c| c == '}' || c == ':')
            .unwrap_or(after_key.len())
            + key_start_rel;
        if rel_offset > key_end_rel {
            return None;
        }

        let prefix = content[key_start_rel..rel_offset].trim().to_string();
        return Some(PlaceholderContext { prefix });
    }

    None
}

fn find_next_unescaped_quote(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let mut backslashes = 0usize;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Provide Micronaut configuration key completions inside `@Value("${...}")`.
#[must_use]
pub fn completions_for_value_placeholder(
    java_source: &str,
    offset: usize,
    config_keys: &[String],
) -> Vec<CompletionItem> {
    let Some(ctx) = placeholder_context_at(java_source, offset) else {
        return Vec::new();
    };

    config_completions(&ctx.prefix, config_keys)
}

fn parse_properties_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let (key, _) = line
            .split_once('=')
            .or_else(|| line.split_once(':'))
            .unwrap_or((line, ""));
        let key = key.trim();
        if !key.is_empty() {
            out.push(key.to_string());
        }
    }
    out
}

fn parse_yaml_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();

    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.trim_start().starts_with('-') {
            continue;
        }

        let indent = line.chars().take_while(|c| c.is_whitespace()).count();
        let Some((raw_key, _)) = line.trim().split_once(':') else {
            continue;
        };
        let key = raw_key.trim();
        if key.is_empty() {
            continue;
        }

        while let Some((prev, _)) = stack.last() {
            if *prev < indent {
                break;
            }
            stack.pop();
        }
        stack.push((indent, key.to_string()));

        out.push(
            stack
                .iter()
                .map(|(_, k)| k.as_str())
                .collect::<Vec<_>>()
                .join("."),
        );
    }

    out
}
