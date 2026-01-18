use crate::schema::{
    Position, Range, TestDiscoverRequest, TestDiscoverResponse, TestFramework, TestItem, TestKind,
};
use crate::test_id::qualify_test_id;
use crate::util::{collect_module_roots, module_for_path, rel_path_string};
use crate::{NovaTestingError, Result, SCHEMA_VERSION};
use nova_core::{LineIndex, TextSize};
use nova_project::SourceRootKind;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tree_sitter::{Node, Parser};

mod index;
use index::TestDiscoveryIndex;

const SKIP_DIRS: &[&str] = &[".git", "target", "build", "out", "node_modules"];
const MAX_CACHED_WORKSPACES: usize = 8;

thread_local! {
    static JAVA_PARSER: RefCell<std::result::Result<Parser, String>> = RefCell::new({
        let mut parser = Parser::new();
        match parser.set_language(tree_sitter_java::language()) {
            Ok(()) => Ok(parser),
            Err(_) => Err("tree-sitter-java language load failed".to_string()),
        }
    });
}

struct CacheEntry {
    last_used: Instant,
    index: Arc<Mutex<TestDiscoveryIndex>>,
}

static DISCOVERY_CACHE: OnceLock<Mutex<HashMap<PathBuf, CacheEntry>>> = OnceLock::new();

pub fn discover_tests(req: &TestDiscoverRequest) -> Result<TestDiscoverResponse> {
    if req.project_root.trim().is_empty() {
        return Err(NovaTestingError::InvalidRequest(
            "`projectRoot` must not be empty".to_string(),
        ));
    }

    let requested_root = PathBuf::from(&req.project_root);
    let requested_root = match requested_root.canonicalize() {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => requested_root,
        Err(err) => {
            tracing::debug!(
                target = "nova.testing",
                path = %requested_root.display(),
                error = %err,
                "failed to canonicalize requested project root for test discovery"
            );
            requested_root
        }
    };

    // Use nova-project to find source roots. This keeps discovery scoped to test sources
    // in Maven/Gradle projects and provides a reasonable fallback for simple projects.
    let project = nova_project::load_project_with_workspace_config(&requested_root)
        .map_err(|err| NovaTestingError::InvalidRequest(err.to_string()))?;
    let project_root = project.workspace_root;
    let modules = collect_module_roots(&project_root, &project.modules);
    let qualify_ids = modules.len() > 1;

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

    let index = get_or_create_index(&project_root, roots);
    let mut tests = {
        let mut index = lock_mutex(index.as_ref());
        index.refresh()?;
        index.tests()
    };

    if qualify_ids {
        for item in &mut tests {
            let abs_path = project_root.join(Path::new(&item.path));
            let module_rel_path = &module_for_path(&modules, &abs_path).rel_path;
            qualify_test_item_ids(item, module_rel_path);
        }
        tests.sort_by(|a, b| a.id.cmp(&b.id));
    }

    Ok(TestDiscoverResponse {
        schema_version: SCHEMA_VERSION,
        tests,
    })
}

fn qualify_test_item_ids(item: &mut TestItem, module_rel_path: &str) {
    item.id = qualify_test_id(module_rel_path, &item.id);
    for child in &mut item.children {
        qualify_test_item_ids(child, module_rel_path);
    }
}

fn get_or_create_index(
    workspace_root: &Path,
    roots: Vec<PathBuf>,
) -> Arc<Mutex<TestDiscoveryIndex>> {
    let cache = DISCOVERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let mut cache = lock_mutex(cache);
    if let Some(entry) = cache.get_mut(workspace_root) {
        entry.last_used = Instant::now();
        let index = entry.index.clone();
        drop(cache);

        let mut idx = lock_mutex(index.as_ref());
        idx.set_source_roots(roots);
        drop(idx);

        return index;
    }

    let index = Arc::new(Mutex::new(TestDiscoveryIndex::new(
        workspace_root.to_path_buf(),
        roots,
    )));

    cache.insert(
        workspace_root.to_path_buf(),
        CacheEntry {
            last_used: Instant::now(),
            index: index.clone(),
        },
    );
    evict_cache(&mut cache);

    index
}

fn evict_cache(cache: &mut HashMap<PathBuf, CacheEntry>) {
    if cache.len() <= MAX_CACHED_WORKSPACES {
        return;
    }

    let mut entries: Vec<_> = cache
        .iter()
        .map(|(path, entry)| (path.clone(), entry.last_used))
        .collect();
    entries.sort_by_key(|(_, used_at)| *used_at);

    let excess = cache.len().saturating_sub(MAX_CACHED_WORKSPACES);
    for (path, _) in entries.into_iter().take(excess) {
        cache.remove(&path);
    }
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
        if !is_type_declaration_kind(child.kind()) {
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

    let body = type_declaration_body(node);
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
    type_body: Node<'_>,
    source: &str,
    line_index: &LineIndex,
    package: Option<&str>,
    imports: &Imports,
    relative_path: &str,
    enclosing_class_id: &str,
) -> Result<Vec<TestItem>> {
    let mut out = Vec::new();
    for child in type_body_member_nodes(type_body) {
        if !is_type_declaration_kind(child.kind()) {
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
    type_body: Node<'_>,
    source: &str,
    line_index: &LineIndex,
    imports: &Imports,
    class_id: &str,
    relative_path: &str,
) -> Result<Vec<TestItem>> {
    let mut out = Vec::new();
    for child in type_body_member_nodes(type_body) {
        if child.kind() != "method_declaration" {
            continue;
        }

        let modifiers = child
            .child_by_field_name("modifiers")
            .or_else(|| find_named_child(child, "modifiers"));
        let annotations = modifiers.map_or_else(Vec::new, |m| collect_annotations(m, source));
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

fn is_type_declaration_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "record_declaration" | "enum_declaration"
    )
}

fn type_declaration_body(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body")
        .or_else(|| find_named_child(node, "class_body"))
        .or_else(|| find_named_child(node, "enum_body"))
}

fn type_body_member_nodes<'a>(type_body: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut cursor = type_body.walk();
    for child in type_body.named_children(&mut cursor) {
        if child.kind() == "enum_body_declarations" {
            let mut inner_cursor = child.walk();
            out.extend(child.named_children(&mut inner_cursor));
            continue;
        }

        out.push(child);
    }
    out
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
    let any_junit5 = imports
        .all_import_roots()
        .any(|path| path == "org.junit.jupiter" || path.starts_with("org.junit.jupiter."));
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
        "Test" | "ParameterizedTest" | "RepeatedTest" | "TestFactory" | "TestTemplate" | "Theory"
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
        "ParameterizedTest" | "RepeatedTest" | "TestFactory" | "TestTemplate" => {
            TestFramework::Junit5
        }
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
    JAVA_PARSER.with(|parser_cell| {
        let mut parser = parser_cell.try_borrow_mut().map_err(|_| {
            NovaTestingError::InvalidRequest("tree-sitter parser is already in use".to_string())
        })?;
        let parser = parser
            .as_mut()
            .map_err(|err| NovaTestingError::InvalidRequest(err.to_string()))?;

        parser.parse(source, None).ok_or_else(|| {
            NovaTestingError::InvalidRequest("tree-sitter failed to parse Java".into())
        })
    })
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

#[track_caller]
fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = std::panic::Location::caller();
            tracing::error!(
                target = "nova.testing",
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "mutex poisoned; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

    fn clear_discovery_cache() {
        let cache = DISCOVERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        lock_mutex(cache).clear();
    }

    fn with_isolated_cache<T>(f: impl FnOnce() -> T) -> T {
        let test_mutex = TEST_MUTEX.get_or_init(|| Mutex::new(()));
        let _guard = lock_mutex(test_mutex);

        clear_discovery_cache();
        let out = f();
        clear_discovery_cache();
        out
    }

    #[test]
    fn evict_cache_removes_least_recently_used_workspaces() {
        with_isolated_cache(|| {
            let extra = 3usize;
            let total = MAX_CACHED_WORKSPACES + extra;

            let mut cache: HashMap<PathBuf, CacheEntry> = HashMap::new();
            let base = Instant::now();

            for i in 0..total {
                let path = PathBuf::from(format!("workspace-{i}"));
                cache.insert(
                    path,
                    CacheEntry {
                        last_used: base + Duration::from_secs(i as u64),
                        index: Arc::new(Mutex::new(TestDiscoveryIndex::new(
                            PathBuf::from(format!("workspace-root-{i}")),
                            Vec::new(),
                        ))),
                    },
                );
            }

            evict_cache(&mut cache);
            assert_eq!(cache.len(), MAX_CACHED_WORKSPACES);

            for i in 0..extra {
                assert!(
                    !cache.contains_key(&PathBuf::from(format!("workspace-{i}"))),
                    "expected workspace-{i} to be evicted"
                );
            }

            for i in extra..total {
                assert!(
                    cache.contains_key(&PathBuf::from(format!("workspace-{i}"))),
                    "expected workspace-{i} to remain in cache"
                );
            }
        });
    }

    #[test]
    fn get_or_create_index_updates_cached_workspace_source_roots() -> Result<()> {
        with_isolated_cache(|| {
            let temp_dir = tempfile::tempdir()?;
            let workspace_root = temp_dir.path().to_path_buf();

            let root_a = workspace_root.join("root_a");
            let root_b = workspace_root.join("root_b");
            fs::create_dir_all(&root_a)?;
            fs::create_dir_all(&root_b)?;

            fs::write(
                root_a.join("RootATest.java"),
                r#"
                package com.example;

                import org.junit.jupiter.api.Test;

                public class RootATest {
                    @Test
                    public void testA() {}
                }
                "#,
            )?;

            fs::write(
                root_b.join("RootBTest.java"),
                r#"
                package com.example;

                import org.junit.jupiter.api.Test;

                public class RootBTest {
                    @Test
                    public void testB() {}
                }
                "#,
            )?;

            let index = get_or_create_index(&workspace_root, vec![root_a]);
            {
                let mut idx = lock_mutex(index.as_ref());
                idx.refresh()?;
                let tests = idx.tests();

                assert_eq!(tests.len(), 1);
                assert_eq!(tests[0].id, "com.example.RootATest");
                assert_eq!(tests[0].children.len(), 1);
                assert_eq!(tests[0].children[0].id, "com.example.RootATest#testA");
            }

            let index = get_or_create_index(&workspace_root, vec![root_b]);
            {
                let mut idx = lock_mutex(index.as_ref());
                idx.refresh()?;
                let tests = idx.tests();

                assert_eq!(tests.len(), 1);
                assert_eq!(tests[0].id, "com.example.RootBTest");
                assert_eq!(tests[0].children.len(), 1);
                assert_eq!(tests[0].children[0].id, "com.example.RootBTest#testB");
            }

            Ok(())
        })
    }
}
