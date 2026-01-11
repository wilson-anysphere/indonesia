use std::collections::{BTreeMap, HashMap};

use tree_sitter::{Node, Parser, TreeCursor};

use crate::edit::{FileId, TextRange};
use crate::java_semantic::{JavaSymbolKind, SymbolId};
use crate::semantic::{RefactorDatabase, Reference, SymbolDefinition};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeKind {
    Root,
    TypeBody,
    MethodBody,
    Block,
    For,
}

struct ScopeData {
    parent: Option<u32>,
    #[allow(dead_code)]
    kind: ScopeKind,
    symbols: HashMap<String, Vec<SymbolId>>,
}

struct SymbolData {
    def: SymbolDefinition,
    kind: JavaSymbolKind,
}

/// A tree-sitter based Java semantic index that powers the prototype semantic refactorings.
///
/// This database intentionally implements *lexical* name resolution only. It understands Java
/// scoping rules for blocks/methods/types and records references for identifier occurrences in the
/// syntax tree (excluding comments/strings by construction).
pub struct TreeSitterJavaDatabase {
    files: BTreeMap<FileId, String>,
    scopes: Vec<ScopeData>,
    symbols: Vec<SymbolData>,
    references: Vec<Vec<Reference>>,
    spans: Vec<(FileId, TextRange, SymbolId)>,
}

impl TreeSitterJavaDatabase {
    pub fn new(files: impl IntoIterator<Item = (FileId, String)>) -> Self {
        let mut db = Self {
            files: BTreeMap::new(),
            scopes: Vec::new(),
            symbols: Vec::new(),
            references: Vec::new(),
            spans: Vec::new(),
        };

        for (file, text) in files {
            db.files.insert(file, text);
        }

        db.rebuild();
        db
    }

    pub fn single_file(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new([(FileId::new(path), text.into())])
    }

    pub fn symbol_at(&self, file: &FileId, offset: usize) -> Option<SymbolId> {
        self.spans.iter().find_map(|(span_file, range, symbol)| {
            if span_file == file && range.start <= offset && offset < range.end {
                Some(*symbol)
            } else {
                None
            }
        })
    }

    pub fn symbol_kind(&self, symbol: SymbolId) -> Option<JavaSymbolKind> {
        self.symbols.get(symbol.as_usize()).map(|s| s.kind)
    }

    fn rebuild(&mut self) {
        self.scopes.clear();
        self.symbols.clear();
        self.references.clear();
        self.spans.clear();

        for (file, text) in self.files.clone() {
            self.index_file(file, &text);
        }
    }

    fn index_file(&mut self, file: FileId, text: &str) {
        let mut parser = Parser::new();
        parser
            .set_language(tree_sitter_java::language())
            .expect("tree-sitter-java language should load");

        let Some(tree) = parser.parse(text, None) else {
            return;
        };

        let root_scope = self.new_scope(None, ScopeKind::Root);
        let mut stack = vec![root_scope];
        self.index_node(&file, text, tree.root_node(), &mut stack);
    }

    fn new_scope(&mut self, parent: Option<u32>, kind: ScopeKind) -> u32 {
        let id = self.scopes.len() as u32;
        self.scopes.push(ScopeData {
            parent,
            kind,
            symbols: HashMap::new(),
        });
        id
    }

    fn add_symbol(
        &mut self,
        file: &FileId,
        name_node: Node<'_>,
        scope: u32,
        kind: JavaSymbolKind,
        text: &str,
    ) -> SymbolId {
        let name = text[name_node.byte_range()].to_string();
        let range = TextRange::new(name_node.start_byte(), name_node.end_byte());

        if let Some(existing) = self.scopes[scope as usize]
            .symbols
            .get(name.as_str())
            .and_then(|candidates| {
                candidates.iter().find(|candidate| {
                    self.symbol_kind(**candidate) == Some(kind)
                        && self
                            .symbols
                            .get(candidate.as_usize())
                            .map(|data| data.def.name_range == range)
                            .unwrap_or(false)
                })
            })
        {
            return *existing;
        }

        let id = SymbolId::new(self.symbols.len() as u32);

        self.symbols.push(SymbolData {
            def: SymbolDefinition {
                file: file.clone(),
                name: name.clone(),
                name_range: range,
                scope,
            },
            kind,
        });
        self.references.push(Vec::new());
        self.spans.push((file.clone(), range, id));

        self.scopes[scope as usize]
            .symbols
            .entry(name)
            .or_default()
            .push(id);

        id
    }

    fn record_reference(&mut self, file: &FileId, symbol: SymbolId, range: TextRange) {
        self.references[symbol.as_usize()].push(Reference {
            file: file.clone(),
            range,
        });
        self.spans.push((file.clone(), range, symbol));
    }

    fn index_node(&mut self, file: &FileId, text: &str, node: Node<'_>, stack: &mut Vec<u32>) {
        match node.kind() {
            // Type declarations.
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration" => {
                let name_node = find_name_identifier(node);
                if let Some(name) = name_node {
                    let scope = *stack.last().unwrap();
                    self.add_symbol(file, name, scope, JavaSymbolKind::Type, text);
                }

                let name_range = name_node.map(|n| (n.start_byte(), n.end_byte()));
                self.visit_named_children_skipping(file, text, node, stack, |child| {
                    name_range
                        .map(|range| (child.start_byte(), child.end_byte()) == range)
                        .unwrap_or(false)
                });
                return;
            }

            // Type bodies create a new lexical scope for members.
            "class_body" | "interface_body" | "enum_body" | "annotation_type_body" => {
                let parent = *stack.last().unwrap();
                let scope = self.new_scope(Some(parent), ScopeKind::TypeBody);
                stack.push(scope);

                // Java type members are effectively in scope throughout the body. Collect member
                // declarations before traversing into method bodies to improve reference
                // resolution for out-of-order declarations.
                self.predeclare_type_body_members(file, text, node, scope);

                self.visit_named_children(file, text, node, stack);
                stack.pop();
                return;
            }

            // Method declaration.
            "method_declaration" => {
                let current_scope = *stack.last().unwrap();
                if let Some(name) = find_name_identifier(node) {
                    self.add_symbol(file, name, current_scope, JavaSymbolKind::Method, text);
                }

                let Some(body) = node.child_by_field_name("body") else {
                    // abstract/interface methods can omit a body.
                    return;
                };
                if body.kind() != "block" {
                    return;
                }

                let method_scope = self.new_scope(Some(current_scope), ScopeKind::MethodBody);
                stack.push(method_scope);
                self.declare_formal_parameters(file, text, node, method_scope);

                // Traverse inside the body without creating an extra scope for the top-level block.
                self.visit_named_children(file, text, body, stack);

                stack.pop();
                return;
            }

            // Constructor declaration.
            "constructor_declaration" => {
                let current_scope = *stack.last().unwrap();
                if let Some(name) = find_name_identifier(node) {
                    self.add_symbol(file, name, current_scope, JavaSymbolKind::Method, text);
                }

                let Some(body) = node.child_by_field_name("body") else {
                    return;
                };
                if body.kind() != "block" {
                    return;
                }

                let method_scope = self.new_scope(Some(current_scope), ScopeKind::MethodBody);
                stack.push(method_scope);
                self.declare_formal_parameters(file, text, node, method_scope);
                self.visit_named_children(file, text, body, stack);
                stack.pop();
                return;
            }

            // Variable declarations.
            "field_declaration" => {
                let scope = *stack.last().unwrap();
                self.declare_variable_declarators(file, text, node, scope, JavaSymbolKind::Field);

                // Traverse initializers.
                self.visit_named_children(file, text, node, stack);
                return;
            }
            "local_variable_declaration" => {
                let scope = *stack.last().unwrap();
                self.declare_variable_declarators(file, text, node, scope, JavaSymbolKind::Local);

                self.visit_named_children(file, text, node, stack);
                return;
            }

            "variable_declarator" => {
                // Avoid treating the declarator name as a reference while still indexing the
                // initializer/value expression.
                let skip_range = node
                    .child_by_field_name("name")
                    .map(|n| (n.start_byte(), n.end_byte()));
                self.visit_named_children_skipping(file, text, node, stack, |child| {
                    skip_range
                        .map(|range| (child.start_byte(), child.end_byte()) == range)
                        .unwrap_or(false)
                });
                return;
            }

            // Loops that introduce locals in their header.
            "for_statement" => {
                let parent = *stack.last().unwrap();
                let scope = self.new_scope(Some(parent), ScopeKind::For);
                stack.push(scope);
                self.visit_named_children(file, text, node, stack);
                stack.pop();
                return;
            }
            "enhanced_for_statement" => {
                let parent = *stack.last().unwrap();
                let scope = self.new_scope(Some(parent), ScopeKind::For);
                stack.push(scope);

                let name_node = node
                    .child_by_field_name("name")
                    .or_else(|| find_named_child_by_kind(node, "identifier"));
                if let Some(name_node) = name_node {
                    if name_node.kind() == "identifier" {
                        self.add_symbol(file, name_node, scope, JavaSymbolKind::Local, text);
                    }
                }

                let skip_range = name_node.map(|n| (n.start_byte(), n.end_byte()));
                self.visit_named_children_skipping(file, text, node, stack, |child| {
                    skip_range
                        .map(|range| (child.start_byte(), child.end_byte()) == range)
                        .unwrap_or(false)
                });

                stack.pop();
                return;
            }

            // Catch clauses introduce an exception variable scoped to the catch block.
            "catch_clause" => {
                // The exception parameter is scoped to the catch body block. Treat the body as a
                // block scope and inject the parameter into it.
                let Some(body) = node.child_by_field_name("body") else {
                    self.visit_named_children(file, text, node, stack);
                    return;
                };
                if body.kind() != "block" {
                    self.visit_named_children(file, text, node, stack);
                    return;
                }

                let parent = *stack.last().unwrap();
                let catch_body_scope = self.new_scope(Some(parent), ScopeKind::Block);
                stack.push(catch_body_scope);
                self.declare_catch_parameter(file, text, node, catch_body_scope);
                self.visit_named_children(file, text, body, stack);
                stack.pop();
                return;
            }

            // Try-with-resources: resources are in scope of the try body.
            "try_statement" => {
                let resources = node.child_by_field_name("resources");
                let body = node.child_by_field_name("body");

                if let (Some(resources), Some(body)) = (resources, body) {
                    if body.kind() == "block" {
                        let parent = *stack.last().unwrap();
                        let try_body_scope = self.new_scope(Some(parent), ScopeKind::Block);
                        stack.push(try_body_scope);
                        self.declare_resources(file, text, resources, try_body_scope);
                        self.visit_named_children(file, text, body, stack);
                        stack.pop();

                        // Catch/finally blocks: fall back to generic traversal to at least index
                        // their local scopes.
                        let resources_range = (resources.start_byte(), resources.end_byte());
                        let body_range = (body.start_byte(), body.end_byte());
                        self.visit_named_children_skipping(file, text, node, stack, |child| {
                            (child.start_byte(), child.end_byte()) == resources_range
                                || (child.start_byte(), child.end_byte()) == body_range
                        });
                        return;
                    }
                }

                self.visit_named_children(file, text, node, stack);
                return;
            }

            // Blocks introduce lexical scopes (unless handled specially above).
            "block" => {
                let parent = *stack.last().unwrap();
                let scope = self.new_scope(Some(parent), ScopeKind::Block);
                stack.push(scope);
                self.visit_named_children(file, text, node, stack);
                stack.pop();
                return;
            }

            // Member access (`this.foo` / `obj.foo`).
            "field_access" => {
                let field_range = node.child_by_field_name("field").and_then(|field| {
                    if field.kind() != "identifier" {
                        return None;
                    }
                    let name = text[field.byte_range()].trim();
                    if let Some(symbol) =
                        self.resolve_in_stack(stack, name, &[JavaSymbolKind::Field])
                    {
                        self.record_reference(
                            file,
                            symbol,
                            TextRange::new(field.start_byte(), field.end_byte()),
                        );
                    }
                    Some((field.start_byte(), field.end_byte()))
                });

                // Traverse everything except the field identifier (which we handled above).
                self.visit_named_children_skipping(file, text, node, stack, |child| {
                    field_range
                        .map(|range| (child.start_byte(), child.end_byte()) == range)
                        .unwrap_or(false)
                });
                return;
            }

            // Method invocations (`foo()` / `obj.foo()` / `super.foo()`).
            "method_invocation" => {
                let name_range = node.child_by_field_name("name").and_then(|name_node| {
                    if name_node.kind() != "identifier" {
                        return None;
                    }
                    let name = text[name_node.byte_range()].trim();
                    if let Some(symbol) =
                        self.resolve_in_stack(stack, name, &[JavaSymbolKind::Method])
                    {
                        self.record_reference(
                            file,
                            symbol,
                            TextRange::new(name_node.start_byte(), name_node.end_byte()),
                        );
                    }
                    Some((name_node.start_byte(), name_node.end_byte()))
                });

                self.visit_named_children_skipping(file, text, node, stack, |child| {
                    name_range
                        .map(|range| (child.start_byte(), child.end_byte()) == range)
                        .unwrap_or(false)
                });
                return;
            }

            // Annotations reference types.
            "marker_annotation" | "annotation" => {
                let name_range = node.child_by_field_name("name").map(|name| {
                    self.index_type_name(file, text, name, stack);
                    (name.start_byte(), name.end_byte())
                });
                self.visit_named_children_skipping(file, text, node, stack, |child| {
                    name_range
                        .map(|range| (child.start_byte(), child.end_byte()) == range)
                        .unwrap_or(false)
                });
                return;
            }

            // Type identifiers inside type contexts (type arguments, extends/implements, etc).
            "type_identifier" | "scoped_type_identifier" => {
                let (name, range) = if node.kind() == "scoped_type_identifier" {
                    let name_node = node
                        .child_by_field_name("name")
                        .or_else(|| find_descendant_by_kinds(node, &["type_identifier"]));
                    let Some(name_node) = name_node else {
                        return;
                    };
                    (
                        text[name_node.byte_range()].trim(),
                        TextRange::new(name_node.start_byte(), name_node.end_byte()),
                    )
                } else {
                    (
                        text[node.byte_range()].trim(),
                        TextRange::new(node.start_byte(), node.end_byte()),
                    )
                };

                if let Some(symbol) = self.resolve_in_stack(stack, name, &[JavaSymbolKind::Type]) {
                    self.record_reference(file, symbol, range);
                }
                return;
            }

            // General identifiers in expression context.
            "identifier" => {
                let name = text[node.byte_range()].trim();
                if let Some(symbol) = self.resolve_in_stack(
                    stack,
                    name,
                    &[
                        JavaSymbolKind::Local,
                        JavaSymbolKind::Parameter,
                        JavaSymbolKind::Field,
                    ],
                ) {
                    self.record_reference(
                        file,
                        symbol,
                        TextRange::new(node.start_byte(), node.end_byte()),
                    );
                }
                return;
            }

            _ => {}
        }

        self.visit_named_children(file, text, node, stack);
    }

    fn visit_named_children(
        &mut self,
        file: &FileId,
        text: &str,
        node: Node<'_>,
        stack: &mut Vec<u32>,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.index_node(file, text, child, stack);
        }
    }

    fn visit_named_children_skipping(
        &mut self,
        file: &FileId,
        text: &str,
        node: Node<'_>,
        stack: &mut Vec<u32>,
        mut skip: impl FnMut(Node<'_>) -> bool,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if skip(child) {
                continue;
            }
            self.index_node(file, text, child, stack);
        }
    }

    fn predeclare_type_body_members(
        &mut self,
        file: &FileId,
        text: &str,
        body: Node<'_>,
        scope: u32,
    ) {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "field_declaration" => {
                    self.declare_variable_declarators(
                        file,
                        text,
                        child,
                        scope,
                        JavaSymbolKind::Field,
                    );
                }
                "method_declaration" | "constructor_declaration" => {
                    if let Some(name) = find_name_identifier(child) {
                        self.add_symbol(file, name, scope, JavaSymbolKind::Method, text);
                    }
                }
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration" => {
                    if let Some(name) = find_name_identifier(child) {
                        self.add_symbol(file, name, scope, JavaSymbolKind::Type, text);
                    }
                }
                _ => {}
            }
        }
    }

    fn declare_formal_parameters(
        &mut self,
        file: &FileId,
        text: &str,
        method: Node<'_>,
        scope: u32,
    ) {
        let params_node = method
            .child_by_field_name("parameters")
            .or_else(|| find_named_child_by_kind(method, "formal_parameters"));

        let Some(params_node) = params_node else {
            return;
        };

        let mut cursor = params_node.walk();
        for param in params_node.named_children(&mut cursor) {
            if !matches!(
                param.kind(),
                "formal_parameter" | "spread_parameter" | "receiver_parameter"
            ) {
                continue;
            }
            if let Some(name) = find_name_identifier(param) {
                self.add_symbol(file, name, scope, JavaSymbolKind::Parameter, text);
            }
        }
    }

    fn declare_catch_parameter(
        &mut self,
        file: &FileId,
        text: &str,
        catch_clause: Node<'_>,
        scope: u32,
    ) {
        let param = catch_clause
            .child_by_field_name("parameter")
            .or_else(|| find_named_child_by_kind(catch_clause, "catch_formal_parameter"));
        let Some(param) = param else { return };
        if let Some(name) = find_name_identifier(param) {
            self.add_symbol(file, name, scope, JavaSymbolKind::Local, text);
        }
    }

    fn declare_resources(&mut self, file: &FileId, text: &str, resources: Node<'_>, scope: u32) {
        // Resources are expressed as variable declarators (e.g. `var r = ...`).
        // Walk resources for any variable_declarator nodes and declare their names.
        let mut cursor = resources.walk();
        for child in resources.named_children(&mut cursor) {
            if child.kind() == "resource" {
                self.declare_variable_declarators(file, text, child, scope, JavaSymbolKind::Local);
            }
        }
    }

    fn declare_variable_declarators(
        &mut self,
        file: &FileId,
        text: &str,
        decl: Node<'_>,
        scope: u32,
        kind: JavaSymbolKind,
    ) {
        let mut stack = vec![decl];
        while let Some(node) = stack.pop() {
            if node.kind() == "variable_declarator" {
                if let Some(name) = find_variable_declarator_name(node) {
                    self.add_symbol(file, name, scope, kind, text);
                }
                continue;
            }

            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                // Skip nested type declarations inside initializers (rare) to avoid treating their
                // declarators as part of this declaration.
                if matches!(
                    child.kind(),
                    "class_declaration"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "record_declaration"
                        | "annotation_type_declaration"
                ) {
                    continue;
                }
                stack.push(child);
            }
        }
    }

    fn resolve_in_stack(
        &self,
        stack: &[u32],
        name: &str,
        kinds: &[JavaSymbolKind],
    ) -> Option<SymbolId> {
        for scope in stack.iter().rev() {
            if let Some(candidates) = self
                .scopes
                .get(*scope as usize)
                .and_then(|s| s.symbols.get(name))
            {
                for candidate in candidates.iter().rev() {
                    if let Some(kind) = self.symbol_kind(*candidate) {
                        if kinds.contains(&kind) {
                            return Some(*candidate);
                        }
                    }
                }
            }
        }
        None
    }

    fn index_type_name(&mut self, file: &FileId, text: &str, node: Node<'_>, stack: &[u32]) {
        match node.kind() {
            "type_identifier" | "scoped_type_identifier" => {
                let (name, range) = if node.kind() == "scoped_type_identifier" {
                    let name_node = node
                        .child_by_field_name("name")
                        .or_else(|| find_descendant_by_kinds(node, &["type_identifier"]));
                    let Some(name_node) = name_node else {
                        return;
                    };
                    (
                        text[name_node.byte_range()].trim(),
                        TextRange::new(name_node.start_byte(), name_node.end_byte()),
                    )
                } else {
                    (
                        text[node.byte_range()].trim(),
                        TextRange::new(node.start_byte(), node.end_byte()),
                    )
                };

                if let Some(symbol) = self.resolve_in_stack(stack, name, &[JavaSymbolKind::Type]) {
                    self.record_reference(file, symbol, range);
                }
            }
            "identifier" => {
                // Some syntactic positions (annotations) use `identifier` for a type name.
                let name = text[node.byte_range()].trim();
                if let Some(symbol) = self.resolve_in_stack(stack, name, &[JavaSymbolKind::Type]) {
                    self.record_reference(
                        file,
                        symbol,
                        TextRange::new(node.start_byte(), node.end_byte()),
                    );
                }
            }
            "scoped_identifier" => {
                // Qualified annotation names (e.g. `@com.foo.Bar`).
                // Record a reference for the final component only.
                if let Some(last) = last_identifier_in_qualified_name(node) {
                    let name = text[last.byte_range()].trim();
                    if let Some(symbol) =
                        self.resolve_in_stack(stack, name, &[JavaSymbolKind::Type])
                    {
                        self.record_reference(
                            file,
                            symbol,
                            TextRange::new(last.start_byte(), last.end_byte()),
                        );
                    }
                }
            }
            _ => {
                // Fallback: look for a descendant type identifier.
                if let Some(desc) =
                    find_descendant_by_kinds(node, &["type_identifier", "scoped_type_identifier"])
                {
                    self.index_type_name(file, text, desc, stack);
                }
            }
        }
    }
}

impl RefactorDatabase for TreeSitterJavaDatabase {
    fn file_text(&self, file: &FileId) -> Option<&str> {
        self.files.get(file).map(|s| s.as_str())
    }

    fn symbol_definition(&self, symbol: SymbolId) -> Option<SymbolDefinition> {
        self.symbols.get(symbol.as_usize()).map(|s| s.def.clone())
    }

    fn symbol_scope(&self, symbol: SymbolId) -> Option<u32> {
        self.symbols.get(symbol.as_usize()).map(|s| s.def.scope)
    }

    fn resolve_name_in_scope(&self, scope: u32, name: &str) -> Option<SymbolId> {
        self.scopes
            .get(scope as usize)
            .and_then(|s| s.symbols.get(name))
            .and_then(|v| v.last())
            .copied()
    }

    fn would_shadow(&self, scope: u32, name: &str) -> Option<SymbolId> {
        let mut current = self.scopes.get(scope as usize).and_then(|s| s.parent);
        while let Some(scope_id) = current {
            if let Some(symbol) = self.resolve_name_in_scope(scope_id, name) {
                return Some(symbol);
            }
            current = self.scopes.get(scope_id as usize).and_then(|s| s.parent);
        }
        None
    }

    fn find_references(&self, symbol: SymbolId) -> Vec<Reference> {
        self.references
            .get(symbol.as_usize())
            .cloned()
            .unwrap_or_default()
    }
}

fn find_named_child_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

fn find_name_identifier<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let name = node
        .child_by_field_name("name")
        .or_else(|| find_named_child_by_kind(node, "identifier"))?;
    if name.kind() == "identifier" || name.kind() == "type_identifier" {
        return Some(name);
    }
    find_descendant_by_kinds(name, &["identifier", "type_identifier"])
}

fn find_variable_declarator_name<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let name = node.child_by_field_name("name")?;
    if name.kind() == "identifier" {
        return Some(name);
    }
    find_descendant_by_kinds(name, &["identifier"])
}

fn find_descendant_by_kinds<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let mut cursor: TreeCursor<'tree> = node.walk();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if kinds.contains(&n.kind()) {
            return Some(n);
        }
        cursor.reset(n);
        for child in n.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn last_identifier_in_qualified_name<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    // `scoped_identifier` typically has `scope` + `name`.
    node.child_by_field_name("name")
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).last()
        })
        .and_then(|n| {
            if n.kind() == "identifier" {
                Some(n)
            } else {
                find_descendant_by_kinds(n, &["identifier"])
            }
        })
}
