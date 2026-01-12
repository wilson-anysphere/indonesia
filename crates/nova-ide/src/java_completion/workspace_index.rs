use std::collections::{HashMap, HashSet};

use nova_db::Database;

/// Best-effort workspace Java type index built purely from the in-memory `Database`.
///
/// This is intentionally lightweight:
/// - It does not depend on the filesystem.
/// - It does not build full syntax trees; parsing is token/line based.
/// - It only indexes package declarations and top-level type names.
#[derive(Debug, Default, Clone)]
pub(crate) struct WorkspaceJavaIndex {
    packages: HashSet<String>,
    package_to_types: HashMap<String, HashSet<String>>,
}

impl WorkspaceJavaIndex {
    pub(crate) fn build(db: &dyn Database) -> Self {
        let mut index = Self::default();

        for file_id in db.all_file_ids() {
            let Some(path) = db.file_path(file_id) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }

            let text = db.file_content(file_id);
            let package = parse_package_name(text).unwrap_or_default();
            index.add_package_hierarchy(&package);

            for type_name in parse_top_level_type_names(text) {
                index
                    .package_to_types
                    .entry(package.clone())
                    .or_default()
                    .insert(type_name);
            }
        }

        index
    }

    pub(crate) fn packages(&self) -> impl Iterator<Item = &String> {
        self.packages.iter()
    }

    pub(crate) fn types_in_package(&self, package: &str) -> impl Iterator<Item = &String> {
        self.package_to_types
            .get(package)
            .into_iter()
            .flat_map(|set| set.iter())
    }

    pub(crate) fn all_types(&self) -> impl Iterator<Item = (&String, &String)> {
        self.package_to_types
            .iter()
            .flat_map(|(pkg, types)| types.iter().map(move |ty| (pkg, ty)))
    }

    pub(crate) fn contains_fqn(&self, fqn: &str) -> bool {
        let Some((pkg, name)) = split_fqn(fqn) else {
            return false;
        };
        self.package_to_types
            .get(pkg)
            .is_some_and(|types| types.contains(name))
    }

    fn add_package_hierarchy(&mut self, package: &str) {
        self.packages.insert(String::new()); // default package/root

        let package = package.trim();
        if package.is_empty() {
            return;
        }

        let mut current = String::new();
        for (i, seg) in package.split('.').enumerate() {
            if seg.is_empty() {
                continue;
            }
            if i > 0 {
                current.push('.');
            }
            current.push_str(seg);
            self.packages.insert(current.clone());
        }
    }
}

pub(crate) fn split_fqn(fqn: &str) -> Option<(&str, &str)> {
    match fqn.rsplit_once('.') {
        Some((pkg, name)) => Some((pkg, name)),
        None => Some(("", fqn)),
    }
}

/// Best-effort parse of the `package ...;` declaration.
pub(crate) fn parse_package_name(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("package") {
            let rest = rest.trim_start();
            if rest.is_empty() {
                continue;
            }
            // The `package` declaration can share a line with other tokens in
            // fixtures/minified sources. Only take the segment up to `;`.
            let end = rest.find(';').unwrap_or(rest.len());
            let pkg = rest[..end].trim();
            if pkg.is_empty() {
                continue;
            }
            return Some(pkg.to_string());
        }
        // Package declarations must come before imports/types; bail once we see those.
        if line.starts_with("import") || line.contains("class") || line.contains("interface") {
            break;
        }
    }
    None
}

/// Parse top-level `class`/`interface`/`enum`/`record` type names.
///
/// This is a best-effort parser that tracks brace depth and skips comments and
/// string literals.
pub(crate) fn parse_top_level_type_names(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut names = Vec::new();

    let mut depth: i32 = 0;
    let mut current = String::new();
    let mut expecting_name = false;

    #[derive(Clone, Copy, Debug)]
    enum State {
        Normal,
        LineComment,
        BlockComment,
        String { escaped: bool },
        Char { escaped: bool },
    }

    let mut state = State::Normal;
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i] as char;

        match state {
            State::Normal => {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                    current.push(ch);
                    i += 1;
                    continue;
                }

                if !current.is_empty() {
                    let tok = std::mem::take(&mut current);
                    if expecting_name && depth == 0 && is_java_identifier(&tok) {
                        names.push(tok);
                        expecting_name = false;
                    } else {
                        expecting_name = depth == 0
                            && matches!(tok.as_str(), "class" | "interface" | "enum" | "record");
                    }
                }

                match ch {
                    '{' => {
                        depth += 1;
                        expecting_name = false;
                        i += 1;
                    }
                    '}' => {
                        depth = depth.saturating_sub(1);
                        expecting_name = false;
                        i += 1;
                    }
                    '"' => {
                        state = State::String { escaped: false };
                        i += 1;
                    }
                    '\'' => {
                        state = State::Char { escaped: false };
                        i += 1;
                    }
                    '/' => {
                        if i + 1 < bytes.len() {
                            let next = bytes[i + 1] as char;
                            if next == '/' {
                                state = State::LineComment;
                                i += 2;
                            } else if next == '*' {
                                state = State::BlockComment;
                                i += 2;
                            } else {
                                i += 1;
                            }
                        } else {
                            i += 1;
                        }
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            State::LineComment => {
                if ch == '\n' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::BlockComment => {
                if ch == '*' && i + 1 < bytes.len() && bytes[i + 1] as char == '/' {
                    state = State::Normal;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            State::String { escaped } => {
                if escaped {
                    state = State::String { escaped: false };
                    i += 1;
                    continue;
                }
                match ch {
                    '\\' => {
                        state = State::String { escaped: true };
                        i += 1;
                    }
                    '"' => {
                        state = State::Normal;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            State::Char { escaped } => {
                if escaped {
                    state = State::Char { escaped: false };
                    i += 1;
                    continue;
                }
                match ch {
                    '\\' => {
                        state = State::Char { escaped: true };
                        i += 1;
                    }
                    '\'' => {
                        state = State::Normal;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
        }
    }

    if !current.is_empty() && expecting_name && depth == 0 && is_java_identifier(&current) {
        names.push(current);
    }

    names
}

fn is_java_identifier(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}
