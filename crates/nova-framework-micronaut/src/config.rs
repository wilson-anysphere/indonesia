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

