use crate::schema::{
    Position, Range, TestDiscoverRequest, TestDiscoverResponse, TestFramework, TestItem, TestKind,
};
use crate::util::rel_path_string;
use crate::{NovaTestingError, Result, SCHEMA_VERSION};
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const SKIP_DIRS: &[&str] = &[".git", "target", "build", "out", "node_modules"];

pub fn discover_tests(req: &TestDiscoverRequest) -> Result<TestDiscoverResponse> {
    if req.project_root.trim().is_empty() {
        return Err(NovaTestingError::InvalidRequest(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let project_root = PathBuf::from(&req.project_root);
    let project_root = project_root.canonicalize().unwrap_or(project_root);

    let mut tests = Vec::new();
    for entry in WalkDir::new(&project_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }

            let name = entry.file_name().to_string_lossy();
            !SKIP_DIRS.iter().any(|skip| skip == &name.as_ref())
        })
    {
        let entry = entry.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("java") {
            continue;
        }

        if let Some(item) = discover_tests_in_file(&project_root, path)? {
            tests.push(item);
        }
    }

    tests.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(TestDiscoverResponse {
        schema_version: SCHEMA_VERSION,
        tests,
    })
}

fn discover_tests_in_file(project_root: &Path, file_path: &Path) -> Result<Option<TestItem>> {
    let content = fs::read_to_string(file_path)?;
    let package = parse_package(&content)?;
    let imports = parse_imports(&content)?;
    let class_info = parse_first_class(&content)?;
    let Some((class_name, class_line)) = class_info else {
        return Ok(None);
    };

    let class_framework = infer_framework_from_imports(&imports);
    let class_id = match &package {
        Some(pkg) => format!("{pkg}.{class_name}"),
        None => class_name.clone(),
    };

    let relative_path = rel_path_string(project_root, file_path);

    let methods = discover_test_methods(&content, &imports, &class_id, &relative_path)?;

    if methods.is_empty() && !looks_like_test_class(&class_name, &relative_path) {
        return Ok(None);
    }

    Ok(Some(TestItem {
        id: class_id,
        label: class_name,
        kind: TestKind::Class,
        framework: class_framework,
        path: relative_path,
        range: Range {
            start: Position {
                line: class_line,
                character: 0,
            },
            end: Position {
                line: class_line,
                character: 0,
            },
        },
        children: methods,
    }))
}

fn parse_package(content: &str) -> Result<Option<String>> {
    let re = Regex::new(r"(?m)^\s*package\s+([A-Za-z0-9_.]+)\s*;")?;
    Ok(re
        .captures(content)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string())))
}

fn parse_imports(content: &str) -> Result<Vec<String>> {
    let re = Regex::new(r"(?m)^\s*import\s+([A-Za-z0-9_.]+)\s*;")?;
    Ok(re
        .captures_iter(content)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .collect())
}

fn parse_first_class(content: &str) -> Result<Option<(String, u32)>> {
    // Best-effort, matches `class Foo` with optional modifiers.
    let re = Regex::new(
        r"(?m)^\s*(?:public|protected|private|abstract|final|static|\s)*\s*class\s+([A-Za-z_][A-Za-z0-9_]*)\b",
    )?;
    if let Some(caps) = re.captures(content) {
        let name = caps.get(1).unwrap().as_str().to_string();
        let m = caps.get(0).unwrap();
        let line = content[..m.start()].matches('\n').count() as u32;
        return Ok(Some((name, line)));
    }
    Ok(None)
}

fn infer_framework_from_imports(imports: &[String]) -> TestFramework {
    if imports.iter().any(|i| i.starts_with("org.junit.jupiter.")) {
        return TestFramework::Junit5;
    }
    if imports
        .iter()
        .any(|i| i == "org.junit.Test" || i.starts_with("org.junit."))
    {
        return TestFramework::Junit4;
    }
    TestFramework::Unknown
}

fn discover_test_methods(
    content: &str,
    imports: &[String],
    class_id: &str,
    relative_path: &str,
) -> Result<Vec<TestItem>> {
    let mut methods = Vec::new();
    let mut pending_annotations: Vec<(String, u32)> = Vec::new();

    let mut in_block_comment = false;

    for (idx, original_line) in content.lines().enumerate() {
        let line_no = idx as u32;
        let line = strip_comments_line(original_line, &mut in_block_comment);
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('@') {
            if let Some(name) = parse_annotation_name(trimmed) {
                pending_annotations.push((name, line_no));
            }
            continue;
        }

        if pending_annotations.is_empty() {
            continue;
        }

        let Some((framework, is_test)) = classify_test_annotations(imports, &pending_annotations)
        else {
            // Annotations belong to some other declaration.
            pending_annotations.clear();
            continue;
        };
        if !is_test {
            pending_annotations.clear();
            continue;
        }

        if let Some(method_name) = extract_method_name(trimmed) {
            let id = format!("{class_id}#{method_name}");
            methods.push(TestItem {
                id,
                label: method_name,
                kind: TestKind::Test,
                framework,
                path: relative_path.to_string(),
                range: Range {
                    start: Position {
                        line: line_no,
                        character: 0,
                    },
                    end: Position {
                        line: line_no,
                        character: 0,
                    },
                },
                children: Vec::new(),
            });
            pending_annotations.clear();
        }
    }

    methods.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(methods)
}

fn strip_comments_line(mut line: &str, in_block_comment: &mut bool) -> String {
    let mut out = String::new();
    while !line.is_empty() {
        if *in_block_comment {
            if let Some(end) = line.find("*/") {
                line = &line[end + 2..];
                *in_block_comment = false;
                continue;
            }
            return out;
        }

        let line_comment = line.find("//");
        let block_comment = line.find("/*");

        match (line_comment, block_comment) {
            (None, None) => {
                out.push_str(line);
                break;
            }
            (Some(lc), None) => {
                out.push_str(&line[..lc]);
                break;
            }
            (None, Some(bc)) => {
                out.push_str(&line[..bc]);
                line = &line[bc + 2..];
                *in_block_comment = true;
            }
            (Some(lc), Some(bc)) => {
                if lc < bc {
                    out.push_str(&line[..lc]);
                    break;
                }
                out.push_str(&line[..bc]);
                line = &line[bc + 2..];
                *in_block_comment = true;
            }
        }
    }
    out
}

fn parse_annotation_name(trimmed_line: &str) -> Option<String> {
    let after_at = trimmed_line.strip_prefix('@')?;
    let name: String = after_at
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    if name.is_empty() {
        return None;
    }
    Some(name)
}

fn classify_test_annotations(
    imports: &[String],
    annotations: &[(String, u32)],
) -> Option<(TestFramework, bool)> {
    let mut is_test = false;
    let mut framework = TestFramework::Unknown;

    for (name, _) in annotations {
        if name == "ParameterizedTest" || name.ends_with(".ParameterizedTest") {
            is_test = true;
            framework = TestFramework::Junit5;
        }

        if name == "Test" || name.ends_with(".Test") {
            is_test = true;
            framework = match infer_framework_for_test_annotation(imports, name) {
                Some(f) => f,
                None => framework,
            };
        }
    }

    if !is_test {
        return None;
    }

    if framework == TestFramework::Unknown {
        framework = infer_framework_from_imports(imports);
    }

    Some((framework, true))
}

fn infer_framework_for_test_annotation(
    imports: &[String],
    annotation: &str,
) -> Option<TestFramework> {
    if annotation.ends_with("org.junit.jupiter.api.Test") {
        return Some(TestFramework::Junit5);
    }
    if annotation.ends_with("org.junit.Test") {
        return Some(TestFramework::Junit4);
    }

    if imports.iter().any(|i| i == "org.junit.jupiter.api.Test") {
        return Some(TestFramework::Junit5);
    }
    if imports.iter().any(|i| i == "org.junit.Test") {
        return Some(TestFramework::Junit4);
    }

    None
}

fn extract_method_name(line: &str) -> Option<String> {
    let paren = line.find('(')?;
    if line[..paren].contains(" class ") || line.starts_with("class ") {
        return None;
    }
    let before = line[..paren].trim();
    let last = before.split_whitespace().last()?;
    if last.is_empty() {
        return None;
    }
    Some(
        last.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .to_string(),
    )
}

fn looks_like_test_class(class_name: &str, relative_path: &str) -> bool {
    if class_name.ends_with("Test")
        || class_name.ends_with("Tests")
        || class_name.ends_with("TestCase")
        || class_name.ends_with("IT")
        || class_name.starts_with("Test")
    {
        return true;
    }
    // Fallback for files following the common `*Test.java` pattern, even if the class name does
    // not match (e.g. when the file contains a single top-level class but the class name differs).
    relative_path.ends_with("Test.java") || relative_path.ends_with("Tests.java")
}
