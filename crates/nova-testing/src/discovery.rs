use crate::schema::{
    Position, Range, TestDiscoverRequest, TestDiscoverResponse, TestFramework, TestItem, TestKind,
};
use crate::util::rel_path_string;
use crate::{NovaTestingError, Result, SCHEMA_VERSION};
use nova_project::SourceRootKind;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};
use walkdir::WalkDir;

const SKIP_DIRS: &[&str] = &[".git", "target", "build", "out", "node_modules"];

pub fn discover_tests(req: &TestDiscoverRequest) -> Result<TestDiscoverResponse> {
    if req.project_root.trim().is_empty() {
        return Err(NovaTestingError::InvalidRequest(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&req.project_root);
    let requested_root = requested_root.canonicalize().unwrap_or(requested_root);

    // Use nova-project to find source roots. This keeps discovery scoped to test sources
    // in Maven/Gradle projects and provides a reasonable fallback for simple projects.
    let project = nova_project::load_project(&requested_root)
        .map_err(|err| NovaTestingError::InvalidRequest(err.to_string()))?;
    let project_root = project.workspace_root;

    let mut roots: Vec<PathBuf> = project
        .source_roots
        .iter()
        .filter(|root| root.kind == SourceRootKind::Test)
        .map(|root| root.path.clone())
        .collect();

    if roots.is_empty() {
        roots = project
            .source_roots
            .iter()
            .map(|root| root.path.clone())
            .collect();
    }

    let mut tests = Vec::new();
    for root in roots {
        tests.extend(discover_tests_in_root(&project_root, &root)?);
    }

    tests.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(TestDiscoverResponse {
        schema_version: SCHEMA_VERSION,
        tests,
    })
}

fn discover_tests_in_root(project_root: &Path, root: &Path) -> Result<Vec<TestItem>> {
    let mut tests = Vec::new();

    for entry in WalkDir::new(root)
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

        tests.extend(discover_tests_in_file(project_root, path)?);
    }

    Ok(tests)
}

fn discover_tests_in_file(project_root: &Path, file_path: &Path) -> Result<Vec<TestItem>> {
    let content = fs::read_to_string(file_path)?;
    let package = parse_package(&content)?;
    let imports = parse_imports(&content)?;
    let relative_path = rel_path_string(project_root, file_path);

    let tree = parse_java(&content)?;
    let root = tree.root_node();

    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "class_declaration" {
            continue;
        }

        if let Some(item) = parse_test_class(
            child,
            &content,
            package.as_deref(),
            &imports,
            &relative_path,
            None,
        )? {
            out.push(item);
        }
    }

    Ok(out)
}

fn parse_test_class(
    node: Node<'_>,
    source: &str,
    package: Option<&str>,
    imports: &[String],
    relative_path: &str,
    enclosing_class_id: Option<&str>,
) -> Result<Option<TestItem>> {
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"));
    let Some(name_node) = name_node else {
        return Ok(None);
    };
    let class_name = node_text(source, name_node).to_string();

    let class_id = match enclosing_class_id {
        Some(parent) => format!("{parent}${class_name}"),
        None => match package {
            Some(pkg) => format!("{pkg}.{class_name}"),
            None => class_name.clone(),
        },
    };

    let class_framework = infer_framework_from_imports(imports);
    let class_pos = ts_point_to_position(name_node.start_position());

    let body = node
        .child_by_field_name("body")
        .or_else(|| find_named_child(node, "class_body"));
    let mut children = Vec::new();
    if let Some(body) = body {
        children.extend(discover_test_methods(
            body,
            source,
            imports,
            &class_id,
            relative_path,
        )?);
        children.extend(discover_nested_test_classes(
            body,
            source,
            package,
            imports,
            relative_path,
            &class_id,
        )?);
    }

    if children.is_empty() && !looks_like_test_class(&class_name, relative_path, enclosing_class_id.is_none()) {
        return Ok(None);
    }

    let class_framework = children
        .iter()
        .map(|m| m.framework)
        .find(|f| *f != TestFramework::Unknown)
        .unwrap_or(class_framework);

    Ok(Some(TestItem {
        id: class_id,
        label: class_name,
        kind: TestKind::Class,
        framework: class_framework,
        path: relative_path.to_string(),
        range: Range {
            start: class_pos,
            end: class_pos,
        },
        children,
    }))
}

fn discover_nested_test_classes(
    class_body: Node<'_>,
    source: &str,
    package: Option<&str>,
    imports: &[String],
    relative_path: &str,
    enclosing_class_id: &str,
) -> Result<Vec<TestItem>> {
    let mut out = Vec::new();
    let mut cursor = class_body.walk();
    for child in class_body.named_children(&mut cursor) {
        if child.kind() != "class_declaration" {
            continue;
        }

        if let Some(item) = parse_test_class(
            child,
            source,
            package,
            imports,
            relative_path,
            Some(enclosing_class_id),
        )? {
            out.push(item);
        }
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn discover_test_methods(
    class_body: Node<'_>,
    source: &str,
    imports: &[String],
    class_id: &str,
    relative_path: &str,
) -> Result<Vec<TestItem>> {
    let mut out = Vec::new();
    let mut cursor = class_body.walk();
    for child in class_body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }

        let modifiers = child
            .child_by_field_name("modifiers")
            .or_else(|| find_named_child(child, "modifiers"));
        let annotations = modifiers
            .map(|m| collect_annotations(m, source))
            .unwrap_or_default();
        let Some(framework) = classify_test_annotations(imports, &annotations) else {
            continue;
        };

        let name_node = child
            .child_by_field_name("name")
            .or_else(|| find_named_child(child, "identifier"));
        let Some(name_node) = name_node else {
            continue;
        };

        let method_name = node_text(source, name_node).to_string();
        let id = format!("{class_id}#{method_name}");
        let pos = ts_point_to_position(name_node.start_position());

        out.push(TestItem {
            id,
            label: method_name,
            kind: TestKind::Test,
            framework,
            path: relative_path.to_string(),
            range: Range {
                start: pos,
                end: pos,
            },
            children: Vec::new(),
        });
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
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

#[derive(Clone, Debug)]
struct Annotation {
    name: String,
    simple_name: String,
}

fn classify_test_annotations(
    imports: &[String],
    annotations: &[Annotation],
) -> Option<TestFramework> {
    let mut framework = TestFramework::Unknown;
    let mut is_test = false;

    for ann in annotations {
        if ann.simple_name == "ParameterizedTest" {
            return Some(TestFramework::Junit5);
        }

        if ann.simple_name == "Test" {
            is_test = true;
            framework = infer_framework_for_test_annotation(imports, &ann.name)
                .unwrap_or_else(|| infer_framework_from_imports(imports));
        }
    }

    if is_test {
        Some(framework)
    } else {
        None
    }
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

fn looks_like_test_class(class_name: &str, relative_path: &str, allow_path_heuristic: bool) -> bool {
    if class_name.ends_with("Test")
        || class_name.ends_with("Tests")
        || class_name.ends_with("TestCase")
        || class_name.ends_with("IT")
        || class_name.starts_with("Test")
    {
        return true;
    }
    allow_path_heuristic && (relative_path.ends_with("Test.java") || relative_path.ends_with("Tests.java"))
}

fn parse_java(source: &str) -> Result<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(tree_sitter_java::language())
        .map_err(|_| {
            NovaTestingError::InvalidRequest("tree-sitter-java language load failed".to_string())
        })?;
    parser
        .parse(source, None)
        .ok_or_else(|| NovaTestingError::InvalidRequest("tree-sitter failed to parse Java".into()))
}

fn find_named_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    result
}

fn collect_annotations(modifiers: Node<'_>, source: &str) -> Vec<Annotation> {
    let mut anns = Vec::new();
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if !child.kind().ends_with("annotation") {
            continue;
        }
        if let Some(ann) = parse_annotation(child, source) {
            anns.push(ann);
        }
    }
    anns
}

fn parse_annotation(node: Node<'_>, source: &str) -> Option<Annotation> {
    parse_annotation_text(node_text(source, node))
}

fn parse_annotation_text(text: &str) -> Option<Annotation> {
    let text = text.trim();
    let rest = text.strip_prefix('@')?;
    let (name_part, _) = rest.split_once('(').unwrap_or((rest, ""));
    let name = name_part.trim().to_string();
    let simple_name = name
        .rsplit('.')
        .next()
        .unwrap_or(name_part)
        .trim()
        .to_string();
    if simple_name.is_empty() {
        return None;
    }

    Some(Annotation { name, simple_name })
}

fn node_text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

fn ts_point_to_position(point: tree_sitter::Point) -> Position {
    Position {
        line: point.row as u32,
        character: point.column as u32,
    }
}
