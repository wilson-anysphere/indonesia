use crate::schema::{
    Position, Range, TestDiscoverRequest, TestDiscoverResponse, TestFramework, TestItem, TestKind,
};
use crate::util::rel_path_string;
use crate::{NovaTestingError, Result, SCHEMA_VERSION};
use nova_core::{LineIndex, TextSize};
use nova_project::SourceRootKind;
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
    let project = nova_project::load_project_with_workspace_config(&requested_root)
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
    let relative_path = rel_path_string(project_root, file_path);

    let tree = parse_java(&content)?;
    let root = tree.root_node();
    let line_index = LineIndex::new(&content);
    let (package, imports) = parse_package_and_imports(root, &content);

    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "class_declaration" {
            continue;
        }

        if let Some(item) = parse_test_class(
            child,
            &content,
            &line_index,
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
    line_index: &LineIndex,
    package: Option<&str>,
    imports: &Imports,
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
    let class_range = range_for_node(line_index, source, name_node);

    let body = node
        .child_by_field_name("body")
        .or_else(|| find_named_child(node, "class_body"));
    let mut children = Vec::new();
    if let Some(body) = body {
        children.extend(discover_test_methods(
            body,
            source,
            line_index,
            imports,
            &class_id,
            relative_path,
        )?);
        children.extend(discover_nested_test_classes(
            body,
            source,
            line_index,
            package,
            imports,
            relative_path,
            &class_id,
        )?);
    }

    if children.is_empty()
        && !looks_like_test_class(&class_name, relative_path, enclosing_class_id.is_none())
    {
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
        range: class_range,
        children,
    }))
}

fn discover_nested_test_classes(
    class_body: Node<'_>,
    source: &str,
    line_index: &LineIndex,
    package: Option<&str>,
    imports: &Imports,
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
            line_index,
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
    line_index: &LineIndex,
    imports: &Imports,
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
        let range = range_for_node(line_index, source, name_node);

        out.push(TestItem {
            id,
            label: method_name,
            kind: TestKind::Test,
            framework,
            path: relative_path.to_string(),
            range,
            children: Vec::new(),
        });
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

#[derive(Clone, Debug, Default)]
struct Imports {
    exact: Vec<String>,
    wildcard: Vec<String>,
    static_exact: Vec<String>,
    static_wildcard: Vec<String>,
}

impl Imports {
    fn non_static_exact(&self) -> impl Iterator<Item = &str> {
        self.exact.iter().map(String::as_str)
    }

    fn non_static_wildcard(&self) -> impl Iterator<Item = &str> {
        self.wildcard.iter().map(String::as_str)
    }

    fn all_import_roots(&self) -> impl Iterator<Item = &str> {
        self.exact
            .iter()
            .chain(self.wildcard.iter())
            .chain(self.static_exact.iter())
            .chain(self.static_wildcard.iter())
            .map(String::as_str)
    }
}

#[derive(Clone, Debug)]
struct ImportDecl {
    path: String,
    is_static: bool,
    is_wildcard: bool,
}

fn parse_package_and_imports(root: Node<'_>, source: &str) -> (Option<String>, Imports) {
    let mut package = None;
    let mut imports = Imports::default();

    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "package_declaration" => {
                if package.is_none() {
                    package = parse_package_declaration(child, source);
                }
            }
            "import_declaration" => {
                if let Some(import) = parse_import_declaration(child, source) {
                    match (import.is_static, import.is_wildcard) {
                        (true, true) => imports.static_wildcard.push(import.path),
                        (true, false) => imports.static_exact.push(import.path),
                        (false, true) => imports.wildcard.push(import.path),
                        (false, false) => imports.exact.push(import.path),
                    }
                }
            }
            _ => {}
        }
    }

    (package, imports)
}

fn parse_package_declaration(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "scoped_identifier" | "identifier"))
        .map(|name| node_text(source, name).to_string());
    result
}

fn parse_import_declaration(node: Node<'_>, source: &str) -> Option<ImportDecl> {
    let is_static = node_contains_child_kind(node, "static")
        || node_text(source, node)
            .trim_start()
            .starts_with("import static");

    let is_wildcard = node_contains_child_kind(node, "*")
        || node_contains_child_kind(node, "asterisk")
        || node_text(source, node).trim_end().ends_with("*;");

    let mut cursor = node.walk();
    let name_node = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "scoped_identifier" | "identifier"))?;

    Some(ImportDecl {
        path: node_text(source, name_node).to_string(),
        is_static,
        is_wildcard,
    })
}

fn node_contains_child_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).any(|child| child.kind() == kind);
    result
}

fn infer_framework_from_imports(imports: &Imports) -> TestFramework {
    let any_junit5 = imports.all_import_roots().any(|path| {
        path == "org.junit.jupiter" || path.starts_with("org.junit.jupiter.")
    });
    if any_junit5 {
        return TestFramework::Junit5;
    }

    let any_junit4 = imports
        .all_import_roots()
        .any(|path| path == "org.junit" || path.starts_with("org.junit."));
    if any_junit4 {
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
    imports: &Imports,
    annotations: &[Annotation],
) -> Option<TestFramework> {
    let mut is_test = false;
    let mut saw_junit4 = false;
    let mut saw_junit5 = false;

    for ann in annotations {
        if !is_test_annotation(&ann.simple_name) {
            continue;
        }

        is_test = true;
        match infer_framework_for_annotation(imports, ann) {
            TestFramework::Junit4 => saw_junit4 = true,
            TestFramework::Junit5 => saw_junit5 = true,
            TestFramework::Unknown => {}
        }
    }

    if is_test {
        Some(match (saw_junit4, saw_junit5) {
            (true, true) => TestFramework::Unknown,
            (true, false) => TestFramework::Junit4,
            (false, true) => TestFramework::Junit5,
            (false, false) => infer_framework_from_imports(imports),
        })
    } else {
        None
    }
}

fn is_test_annotation(simple_name: &str) -> bool {
    matches!(
        simple_name,
        "Test"
            | "ParameterizedTest"
            | "RepeatedTest"
            | "TestFactory"
            | "TestTemplate"
            | "Theory"
    )
}

fn infer_framework_for_annotation(imports: &Imports, ann: &Annotation) -> TestFramework {
    let mut saw_junit4 = false;
    let mut saw_junit5 = false;

    for candidate in resolve_annotation_candidates(imports, &ann.name) {
        match framework_from_fqn(&candidate) {
            TestFramework::Junit4 => saw_junit4 = true,
            TestFramework::Junit5 => saw_junit5 = true,
            TestFramework::Unknown => {}
        }
    }

    match (saw_junit4, saw_junit5) {
        (true, true) => return TestFramework::Unknown,
        (true, false) => return TestFramework::Junit4,
        (false, true) => return TestFramework::Junit5,
        (false, false) => {}
    }

    match ann.simple_name.as_str() {
        "ParameterizedTest" | "RepeatedTest" | "TestFactory" | "TestTemplate" => TestFramework::Junit5,
        "Theory" => TestFramework::Junit4,
        "Test" => infer_framework_from_imports(imports),
        _ => TestFramework::Unknown,
    }
}

fn resolve_annotation_candidates(imports: &Imports, annotation: &str) -> Vec<String> {
    if annotation.contains('.') {
        return vec![annotation.to_string()];
    }

    let mut candidates = Vec::new();
    for import in imports.non_static_exact() {
        if import.rsplit('.').next() == Some(annotation) {
            candidates.push(import.to_string());
        }
    }

    for wildcard in imports.non_static_wildcard() {
        candidates.push(format!("{wildcard}.{annotation}"));
    }

    candidates
}

fn framework_from_fqn(fqn: &str) -> TestFramework {
    if fqn.starts_with("org.junit.jupiter.") {
        return TestFramework::Junit5;
    }
    if fqn.starts_with("org.junit.") || fqn == "org.junit" {
        return TestFramework::Junit4;
    }
    TestFramework::Unknown
}

fn looks_like_test_class(
    class_name: &str,
    relative_path: &str,
    allow_path_heuristic: bool,
) -> bool {
    if class_name.ends_with("Test")
        || class_name.ends_with("Tests")
        || class_name.ends_with("TestCase")
        || class_name.ends_with("IT")
        || class_name.starts_with("Test")
    {
        return true;
    }
    allow_path_heuristic
        && (relative_path.ends_with("Test.java") || relative_path.ends_with("Tests.java"))
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

fn position_for_offset(line_index: &LineIndex, source: &str, offset: usize) -> Position {
    let pos = line_index.position(source, TextSize::from(offset as u32));
    Position {
        line: pos.line,
        character: pos.character,
    }
}

fn range_for_node(line_index: &LineIndex, source: &str, node: Node<'_>) -> Range {
    Range {
        start: position_for_offset(line_index, source, node.start_byte()),
        end: position_for_offset(line_index, source, node.end_byte()),
    }
}
