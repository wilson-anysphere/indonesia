use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PackageNameError {
    #[error("package name must not be empty")]
    Empty,
    #[error("invalid package segment `{segment}`")]
    InvalidSegment { segment: String },
    #[error("reserved Java keyword `{keyword}` cannot be used as a package segment")]
    ReservedKeyword { keyword: String },
}

/// Validates a Java-style package name (`com.example.foo`).
///
/// This is intentionally conservative: it accepts ASCII identifiers only.
pub fn validate_package_name(name: &str) -> Result<(), PackageNameError> {
    if name.is_empty() {
        return Err(PackageNameError::Empty);
    }

    for segment in name.split('.') {
        if segment.is_empty() {
            return Err(PackageNameError::InvalidSegment {
                segment: segment.to_string(),
            });
        }

        if is_java_keyword(segment) {
            return Err(PackageNameError::ReservedKeyword {
                keyword: segment.to_string(),
            });
        }

        let mut chars = segment.chars();
        match chars.next() {
            Some(first) if is_ident_start(first) => {}
            _ => {
                return Err(PackageNameError::InvalidSegment {
                    segment: segment.to_string(),
                })
            }
        }
        if !chars.all(is_ident_part) {
            return Err(PackageNameError::InvalidSegment {
                segment: segment.to_string(),
            });
        }
    }

    Ok(())
}

pub fn is_valid_package_name(name: &str) -> bool {
    validate_package_name(name).is_ok()
}

pub fn package_to_path(package: &str) -> PathBuf {
    if package.is_empty() {
        return PathBuf::new();
    }
    package.split('.').collect()
}

pub fn class_to_file_name(class_name: &str) -> String {
    format!("{class_name}.java")
}

/// Attempts to infer the source root for a Java file using its `package` declaration.
///
/// Example:
/// - file path: `src/main/java/com/example/Foo.java`
/// - package: `com.example`
/// - source root: `src/main/java`
pub fn infer_source_root(file_path: &Path, package: &str) -> Option<PathBuf> {
    let pkg_path = package_to_path(package);
    let parent = file_path.parent()?;

    if !path_ends_with(parent, &pkg_path) {
        return None;
    }

    let mut root = parent.to_path_buf();
    for _ in pkg_path.components() {
        root = root.parent()?.to_path_buf();
    }
    Some(root)
}

pub fn path_ends_with(path: &Path, suffix: &Path) -> bool {
    let mut path_iter = path.components().rev();
    for suffix_component in suffix.components().rev() {
        match path_iter.next() {
            Some(path_component) if same_component(path_component, suffix_component) => {}
            _ => return false,
        }
    }
    true
}

fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    match (a, b) {
        (Component::Normal(a), Component::Normal(b)) => a == b,
        _ => false,
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_part(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

fn is_java_keyword(word: &str) -> bool {
    // https://docs.oracle.com/javase/tutorial/java/nutsandbolts/_keywords.html
    matches!(
        word,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_source_root_happy_path() {
        let file = Path::new("src/main/java/com/example/Foo.java");
        let root = infer_source_root(file, "com.example").unwrap();
        assert_eq!(root, Path::new("src/main/java"));
    }

    #[test]
    fn infer_source_root_none_when_path_mismatch() {
        let file = Path::new("src/main/java/com/example/Foo.java");
        assert!(infer_source_root(file, "com.wrong").is_none());
    }
}

