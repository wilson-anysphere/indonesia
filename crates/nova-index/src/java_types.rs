use nova_build_model::{SourceRoot, SourceRootKind, SourceRootOrigin};
use nova_core::{fs as corefs, Name, PackageName, QualifiedName, TypeIndex, TypeName};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassLocation {
    pub file: PathBuf,
    pub source_root: PathBuf,
    pub kind: SourceRootKind,
    pub origin: SourceRootOrigin,
}

#[derive(Default, Debug)]
pub struct ClassIndex {
    classes: HashMap<String, Vec<ClassLocation>>,
    package_to_types: HashMap<String, HashMap<String, TypeName>>,
    packages: HashSet<String>,
}

impl ClassIndex {
    pub fn build(roots: &[SourceRoot]) -> io::Result<Self> {
        let mut index = Self::default();

        for root in roots {
            for file in corefs::collect_java_files(&root.path)? {
                let text = fs::read_to_string(&file)?;
                let package = parse_package_name(&text);
                for class_name in parse_top_level_type_names(&text) {
                    let fqn = match package.as_deref() {
                        Some(package) if !package.is_empty() => format!("{package}.{class_name}"),
                        _ => class_name.clone(),
                    };
                    let type_name = TypeName::new(fqn.clone());

                    index
                        .classes
                        .entry(fqn.clone())
                        .or_default()
                        .push(ClassLocation {
                            file: file.clone(),
                            source_root: root.path.clone(),
                            kind: root.kind,
                            origin: root.origin,
                        });

                    let pkg = package.clone().unwrap_or_else(String::new);
                    index.packages.insert(pkg.clone());
                    index
                        .package_to_types
                        .entry(pkg)
                        .or_default()
                        .insert(class_name, type_name);
                }
            }
        }

        Ok(index)
    }

    pub fn contains(&self, fqn: &str) -> bool {
        self.classes.contains_key(fqn)
    }

    pub fn all_locations(&self, fqn: &str) -> Option<&[ClassLocation]> {
        self.classes.get(fqn).map(Vec::as_slice)
    }

    /// Return the best location for a class based on stable precedence rules.
    ///
    /// Precedence:
    /// 1. User sources
    /// 2. Generated sources (annotation processor output)
    ///
    /// When multiple candidates have the same precedence, the lexicographically-smallest path
    /// wins to make results deterministic.
    pub fn lookup(&self, fqn: &str) -> Option<&ClassLocation> {
        let locations = self.classes.get(fqn)?;
        locations.iter().min_by(|a, b| compare_locations(a, b))
    }
}

fn compare_locations(a: &ClassLocation, b: &ClassLocation) -> std::cmp::Ordering {
    a.origin
        .cmp(&b.origin)
        .then_with(|| a.kind.cmp(&b.kind))
        .then_with(|| path_as_sort_key(&a.file).cmp(&path_as_sort_key(&b.file)))
}

fn path_as_sort_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn parse_package_name(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("package") {
            let rest = rest.trim_start();
            if rest.is_empty() {
                continue;
            }
            return Some(rest.trim_end_matches(';').trim().to_string());
        }
    }
    None
}

fn parse_top_level_type_names(text: &str) -> Vec<String> {
    let tokens = tokenize(text);
    let mut names = Vec::new();

    for window in tokens.windows(2) {
        let (keyword, name) = (&window[0], &window[1]);
        if matches!(keyword.as_str(), "class" | "interface" | "enum" | "record")
            && is_java_identifier(name)
        {
            names.push(name.clone());
        }
    }

    names
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn is_java_identifier(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

impl TypeIndex for ClassIndex {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        let dotted = name.to_dotted();
        self.classes
            .contains_key(&dotted)
            .then(|| TypeName::new(dotted))
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        let pkg = package.to_dotted();
        self.package_to_types
            .get(&pkg)
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.packages.contains(&package.to_dotted())
    }
}
