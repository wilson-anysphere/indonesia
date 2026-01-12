use std::collections::BTreeSet;

use nova_hir::item_tree::{Item as HirItem, ItemTree as HirItemTree, Member as HirMember};
use nova_syntax::ast::{self, AstNode};

use crate::indexes::{
    AnnotationLocation, IndexSymbolKind, IndexedSymbol, InheritanceEdge, ProjectIndexes,
    ReferenceLocation, SymbolLocation,
};

/// Stable, range-insensitive semantic information extracted from a Java syntax tree.
///
/// This is intentionally whitespace/trivia-insensitive so Salsa callers can use it
/// for early-cutoff indexing: edits that only change trivia should generally
/// produce the same [`JavaFileIndexExtras`], avoiding downstream recomputation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JavaFileIndexExtras {
    /// (subtype, supertype) edges from `extends` / `implements` clauses.
    pub inheritance: Vec<(String, String)>,
    /// Annotation names (including leading `@`) found on declarations.
    pub annotations: Vec<String>,
}

pub fn extract_java_file_index_extras(parse: &nova_syntax::JavaParseResult) -> JavaFileIndexExtras {
    let root = parse.syntax();
    let Some(unit) = ast::CompilationUnit::cast(root) else {
        return JavaFileIndexExtras::default();
    };

    let mut collector = ExtrasCollector::default();

    if let Some(pkg) = unit.package() {
        for ann in pkg.annotations() {
            collector.collect_annotation(&ann);
        }
    }

    for decl in unit.type_declarations() {
        collector.collect_type_declaration(&decl);
    }

    JavaFileIndexExtras {
        inheritance: collector.inheritance.into_iter().collect(),
        annotations: collector.annotations.into_iter().collect(),
    }
}

#[derive(Debug, Default)]
struct ExtrasCollector {
    inheritance: BTreeSet<(String, String)>,
    annotations: BTreeSet<String>,
}

impl ExtrasCollector {
    fn collect_type_declaration(&mut self, decl: &ast::TypeDeclaration) {
        match decl {
            ast::TypeDeclaration::ClassDeclaration(it) => self.collect_class_declaration(it),
            ast::TypeDeclaration::InterfaceDeclaration(it) => {
                self.collect_interface_declaration(it)
            }
            ast::TypeDeclaration::EnumDeclaration(it) => self.collect_enum_declaration(it),
            ast::TypeDeclaration::RecordDeclaration(it) => self.collect_record_declaration(it),
            ast::TypeDeclaration::AnnotationTypeDeclaration(it) => {
                self.collect_annotation_type_declaration(it)
            }
            ast::TypeDeclaration::EmptyDeclaration(_) => {}
            // `nova_syntax` keeps these enums `#[non_exhaustive]` to allow grammar growth.
            // Unknown variants should not break indexing; just ignore them for now.
            _ => {}
        }
    }

    fn collect_class_declaration(&mut self, decl: &ast::ClassDeclaration) {
        let Some(subtype) = decl.name_token().map(|tok| tok.text().to_string()) else {
            return;
        };

        self.collect_modifiers(decl.modifiers());

        self.collect_extends_clause(subtype.as_str(), decl.extends_clause());
        self.collect_implements_clause(subtype.as_str(), decl.implements_clause());

        if let Some(body) = decl.body() {
            for member in body.members() {
                self.collect_class_member(&member);
            }
        }
    }

    fn collect_interface_declaration(&mut self, decl: &ast::InterfaceDeclaration) {
        let Some(subtype) = decl.name_token().map(|tok| tok.text().to_string()) else {
            return;
        };

        self.collect_modifiers(decl.modifiers());
        self.collect_extends_clause(subtype.as_str(), decl.extends_clause());
        self.collect_implements_clause(subtype.as_str(), decl.implements_clause());

        if let Some(body) = decl.body() {
            for member in body.members() {
                self.collect_class_member(&member);
            }
        }
    }

    fn collect_enum_declaration(&mut self, decl: &ast::EnumDeclaration) {
        let Some(subtype) = decl.name_token().map(|tok| tok.text().to_string()) else {
            return;
        };

        self.collect_modifiers(decl.modifiers());
        self.collect_implements_clause(subtype.as_str(), decl.implements_clause());

        if let Some(body) = decl.body() {
            for member in body.members() {
                self.collect_class_member(&member);
            }
        }
    }

    fn collect_record_declaration(&mut self, decl: &ast::RecordDeclaration) {
        let Some(subtype) = decl.name_token().map(|tok| tok.text().to_string()) else {
            return;
        };

        self.collect_modifiers(decl.modifiers());
        self.collect_implements_clause(subtype.as_str(), decl.implements_clause());

        // Record header parameters can also carry annotations.
        if let Some(params) = decl.parameter_list() {
            for param in params.parameters() {
                self.collect_modifiers(param.modifiers());
            }
        }

        if let Some(body) = decl.body() {
            for member in body.members() {
                self.collect_class_member(&member);
            }
        }
    }

    fn collect_annotation_type_declaration(&mut self, decl: &ast::AnnotationTypeDeclaration) {
        self.collect_modifiers(decl.modifiers());

        if let Some(body) = decl.body() {
            for member in body.members() {
                self.collect_class_member(&member);
            }
        }
    }

    fn collect_class_member(&mut self, member: &ast::ClassMember) {
        match member {
            ast::ClassMember::FieldDeclaration(it) => self.collect_modifiers(it.modifiers()),
            ast::ClassMember::MethodDeclaration(it) => {
                self.collect_modifiers(it.modifiers());
                for param in it.parameters() {
                    self.collect_modifiers(param.modifiers());
                }
            }
            ast::ClassMember::ConstructorDeclaration(it) => {
                self.collect_modifiers(it.modifiers());
                if let Some(params) = it.parameter_list() {
                    for param in params.parameters() {
                        self.collect_modifiers(param.modifiers());
                    }
                }
            }
            ast::ClassMember::CompactConstructorDeclaration(it) => {
                self.collect_modifiers(it.modifiers());
            }
            ast::ClassMember::InitializerBlock(it) => self.collect_modifiers(it.modifiers()),
            ast::ClassMember::EmptyDeclaration(_) => {}

            // Nested type declarations.
            ast::ClassMember::ClassDeclaration(it) => self.collect_class_declaration(it),
            ast::ClassMember::InterfaceDeclaration(it) => self.collect_interface_declaration(it),
            ast::ClassMember::EnumDeclaration(it) => self.collect_enum_declaration(it),
            ast::ClassMember::RecordDeclaration(it) => self.collect_record_declaration(it),
            ast::ClassMember::AnnotationTypeDeclaration(it) => {
                self.collect_annotation_type_declaration(it)
            }
            // `nova_syntax` keeps these enums `#[non_exhaustive]` to allow grammar growth.
            _ => {}
        }
    }

    fn collect_modifiers(&mut self, modifiers: Option<ast::Modifiers>) {
        let Some(modifiers) = modifiers else {
            return;
        };

        for ann in modifiers.annotations() {
            self.collect_annotation(&ann);
        }
    }

    fn collect_annotation(&mut self, ann: &ast::Annotation) {
        let Some(name) = ann.name() else {
            return;
        };

        let text = name.text();
        let simple = simple_name_from_qualified_name(text.as_str());
        if simple.is_empty() {
            return;
        }
        self.annotations.insert(format!("@{simple}"));
    }

    fn collect_extends_clause(&mut self, subtype: &str, clause: Option<ast::ExtendsClause>) {
        let Some(clause) = clause else {
            return;
        };
        for ty in clause.types() {
            if let Some(supertype) = type_to_simple_name(&ty) {
                self.inheritance
                    .insert((subtype.to_string(), supertype.to_string()));
            }
        }
    }

    fn collect_implements_clause(&mut self, subtype: &str, clause: Option<ast::ImplementsClause>) {
        let Some(clause) = clause else {
            return;
        };
        for ty in clause.types() {
            if let Some(supertype) = type_to_simple_name(&ty) {
                self.inheritance
                    .insert((subtype.to_string(), supertype.to_string()));
            }
        }
    }
}

fn simple_name_from_qualified_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn syntax_text_no_trivia(node: &nova_syntax::SyntaxNode) -> String {
    let mut out = String::new();
    // `children_with_tokens` only yields tokens that are direct children of the
    // node, but many syntax nodes (like `Type`) nest the identifier tokens
    // under multiple layers. Use a full descendant walk so we always see the
    // actual token stream, then strip trivia for stability across whitespace-only edits.
    for el in node.descendants_with_tokens() {
        let Some(tok) = el.into_token() else { continue };
        if tok.kind().is_trivia() {
            continue;
        }
        out.push_str(tok.text());
    }
    out
}

fn type_to_simple_name(ty: &ast::Type) -> Option<String> {
    let text = syntax_text_no_trivia(ty.syntax());
    if text.is_empty() {
        return None;
    }

    // Strip generic arguments and array dimensions.
    let base = text
        .split_once('<')
        .map(|(head, _)| head)
        .unwrap_or(text.as_str());
    let base = base.split_once('[').map(|(head, _)| head).unwrap_or(base);

    let base = simple_name_from_qualified_name(base);
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// Build a [`ProjectIndexes`] fragment for a single file.
///
/// The resulting index is **range-insensitive** and intentionally records
/// `(line, column) = (0, 0)` locations. This keeps the output stable across
/// whitespace-only edits and makes it suitable for Salsa early-cutoff.
pub fn build_file_indexes(
    rel_path: &str,
    hir: &HirItemTree,
    extras: &JavaFileIndexExtras,
) -> ProjectIndexes {
    let mut out = ProjectIndexes::default();
    let file = rel_path.to_string();

    let mut reference_names = BTreeSet::<String>::new();
    let package = hir.package.as_ref().map(|pkg| pkg.name.as_str());

    let mut type_stack: Vec<String> = Vec::new();
    for item in &hir.items {
        collect_hir_item(
            hir,
            *item,
            package,
            &mut type_stack,
            &file,
            &mut out,
            &mut reference_names,
        );
    }

    // Treat supertypes in inheritance edges as type references.
    for (_, supertype) in &extras.inheritance {
        reference_names.insert(supertype.clone());
    }

    for name in reference_names {
        out.references.insert(
            name,
            ReferenceLocation {
                file: file.clone(),
                line: 0,
                column: 0,
            },
        );
    }

    for ann in extras.annotations.iter().cloned().collect::<BTreeSet<_>>() {
        out.annotations.insert(
            ann,
            AnnotationLocation {
                file: file.clone(),
                line: 0,
                column: 0,
            },
        );
    }

    let edges = extras
        .inheritance
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|(subtype, supertype)| InheritanceEdge {
            file: file.clone(),
            subtype,
            supertype,
        });
    out.inheritance.extend(edges);

    // Keep results stable for deterministic tests.
    for symbols in out.symbols.symbols.values_mut() {
        symbols.sort_by(|a, b| {
            a.qualified_name
                .cmp(&b.qualified_name)
                .then_with(|| a.ast_id.cmp(&b.ast_id))
        });
    }

    out
}

fn collect_hir_item(
    tree: &HirItemTree,
    item: HirItem,
    package: Option<&str>,
    type_stack: &mut Vec<String>,
    file: &str,
    out: &mut ProjectIndexes,
    references: &mut BTreeSet<String>,
) {
    match item {
        HirItem::Class(id) => {
            let data = tree.class(id);
            collect_hir_type(
                tree,
                IndexSymbolKind::Class,
                &data.name,
                id.ast_id.to_raw(),
                &data.members,
                package,
                type_stack,
                file,
                out,
                references,
            );
        }
        HirItem::Interface(id) => {
            let data = tree.interface(id);
            collect_hir_type(
                tree,
                IndexSymbolKind::Interface,
                &data.name,
                id.ast_id.to_raw(),
                &data.members,
                package,
                type_stack,
                file,
                out,
                references,
            );
        }
        HirItem::Enum(id) => {
            let data = tree.enum_(id);
            collect_hir_type(
                tree,
                IndexSymbolKind::Enum,
                &data.name,
                id.ast_id.to_raw(),
                &data.members,
                package,
                type_stack,
                file,
                out,
                references,
            );
        }
        HirItem::Record(id) => {
            let data = tree.record(id);
            collect_hir_type(
                tree,
                IndexSymbolKind::Record,
                &data.name,
                id.ast_id.to_raw(),
                &data.members,
                package,
                type_stack,
                file,
                out,
                references,
            );
        }
        HirItem::Annotation(id) => {
            let data = tree.annotation(id);
            collect_hir_type(
                tree,
                IndexSymbolKind::Annotation,
                &data.name,
                id.ast_id.to_raw(),
                &data.members,
                package,
                type_stack,
                file,
                out,
                references,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_hir_type(
    tree: &HirItemTree,
    kind: IndexSymbolKind,
    name: &str,
    ast_id: u32,
    members: &[HirMember],
    package: Option<&str>,
    type_stack: &mut Vec<String>,
    file: &str,
    out: &mut ProjectIndexes,
    references: &mut BTreeSet<String>,
) {
    let container_name = if type_stack.is_empty() {
        package
            .map(|pkg| pkg.to_string())
            .filter(|pkg| !pkg.is_empty())
    } else {
        Some(fqn_for_type(package, type_stack))
    };

    type_stack.push(name.to_string());
    let qualified_name = fqn_for_type(package, type_stack);

    out.symbols.insert(
        name,
        IndexedSymbol {
            qualified_name,
            kind,
            container_name,
            location: SymbolLocation {
                file: file.to_string(),
                line: 0,
                column: 0,
            },
            ast_id,
        },
    );

    collect_hir_members(tree, members, package, type_stack, file, out, references);
    type_stack.pop();
}

fn collect_hir_members(
    tree: &HirItemTree,
    members: &[HirMember],
    package: Option<&str>,
    type_stack: &mut Vec<String>,
    file: &str,
    out: &mut ProjectIndexes,
    references: &mut BTreeSet<String>,
) {
    let container_type_fqn = fqn_for_type(package, type_stack);
    for member in members {
        match member {
            HirMember::Field(id) => {
                let field = tree.field(*id);
                out.symbols.insert(
                    &field.name,
                    IndexedSymbol {
                        qualified_name: format!("{container_type_fqn}.{}", field.name),
                        kind: IndexSymbolKind::Field,
                        container_name: Some(container_type_fqn.clone()),
                        location: SymbolLocation {
                            file: file.to_string(),
                            line: 0,
                            column: 0,
                        },
                        ast_id: id.ast_id.to_raw(),
                    },
                );
                collect_type_references(&field.ty, references);
            }
            HirMember::Method(id) => {
                let method = tree.method(*id);
                out.symbols.insert(
                    &method.name,
                    IndexedSymbol {
                        qualified_name: format!("{container_type_fqn}.{}", method.name),
                        kind: IndexSymbolKind::Method,
                        container_name: Some(container_type_fqn.clone()),
                        location: SymbolLocation {
                            file: file.to_string(),
                            line: 0,
                            column: 0,
                        },
                        ast_id: id.ast_id.to_raw(),
                    },
                );
                collect_type_references(&method.return_ty, references);
                for param in &method.params {
                    collect_type_references(&param.ty, references);
                }
            }
            HirMember::Constructor(id) => {
                let ctor = tree.constructor(*id);
                out.symbols.insert(
                    &ctor.name,
                    IndexedSymbol {
                        qualified_name: format!("{container_type_fqn}.{}", ctor.name),
                        kind: IndexSymbolKind::Constructor,
                        container_name: Some(container_type_fqn.clone()),
                        location: SymbolLocation {
                            file: file.to_string(),
                            line: 0,
                            column: 0,
                        },
                        ast_id: id.ast_id.to_raw(),
                    },
                );
                for param in &ctor.params {
                    collect_type_references(&param.ty, references);
                }
            }
            HirMember::Initializer(_) => {}
            HirMember::Type(item) => {
                collect_hir_item(tree, *item, package, type_stack, file, out, references);
            }
        }
    }
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_identifier_continue(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
}

fn collect_type_references(ty: &str, out: &mut BTreeSet<String>) {
    let bytes = ty.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        let ch = b as char;
        if !is_identifier_start(ch) {
            i += 1;
            continue;
        }

        // Parse the first identifier segment.
        let mut last_start = i;
        i += 1;
        while i < bytes.len() && is_identifier_continue(bytes[i] as char) {
            i += 1;
        }
        let mut last_end = i;

        // Consume qualified name segments (`Foo.Bar.Baz`), keeping the last segment.
        loop {
            while i < bytes.len() && (bytes[i] as char).is_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'.' {
                break;
            }
            i += 1; // '.'
            while i < bytes.len() && (bytes[i] as char).is_whitespace() {
                i += 1;
            }

            if i >= bytes.len() || !is_identifier_start(bytes[i] as char) {
                break;
            }

            last_start = i;
            i += 1;
            while i < bytes.len() && is_identifier_continue(bytes[i] as char) {
                i += 1;
            }
            last_end = i;
        }

        if last_end > last_start {
            let name = &ty[last_start..last_end];
            if !is_type_keyword(name) {
                out.insert(name.to_string());
            }
        }
    }
}

fn is_type_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "byte"
            | "short"
            | "int"
            | "long"
            | "float"
            | "double"
            | "boolean"
            | "char"
            | "void"
            | "extends"
            | "super"
    )
}

fn fqn_for_type(package: Option<&str>, type_parts: &[String]) -> String {
    let package = package.filter(|pkg| !pkg.is_empty());
    let mut out = String::new();
    if let Some(pkg) = package {
        out.push_str(pkg);
    }
    for part in type_parts {
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_core::FileId;
    use nova_hir::{ast_id::AstIdMap, lowering::lower_item_tree_with};

    fn lower_hir(text: &str) -> (nova_syntax::JavaParseResult, HirItemTree) {
        let file = FileId::from_raw(0);
        let parse_java = nova_syntax::parse_java(text);
        let syntax = parse_java.syntax();
        let parse_light = nova_syntax::java::parse_with_syntax(&syntax, text.len());
        let ast_id_map = AstIdMap::new(&syntax);
        let mut cancelled = || {};
        let hir = lower_item_tree_with(
            file,
            parse_light.compilation_unit(),
            &parse_java,
            &ast_id_map,
            &mut cancelled,
        );
        (parse_java, hir)
    }

    #[test]
    fn indexes_definitions_inheritance_and_annotations() {
        let text = r#"
            package com.example;

            @Anno
            public class A extends B implements I1, I2 {
                @Inject int field;
                void m(@Param String s) {}
                class Inner {}
            }

            interface I1 {}
            interface I2 {}
            class B {}

            @interface Anno {}
            @interface Inject {}
            @interface Param {}
        "#;

        let (parse_java, hir) = lower_hir(text);
        let extras = extract_java_file_index_extras(&parse_java);
        let indexes = build_file_indexes("A.java", &hir, &extras);

        // Symbol definitions (from HIR).
        for name in [
            "A", "Inner", "field", "m", "I1", "I2", "B", "Anno", "Inject", "Param",
        ] {
            assert!(
                indexes.symbols.symbols.contains_key(name),
                "expected symbol {name} in symbols index"
            );
            assert!(
                !indexes
                    .symbols
                    .symbols
                    .get(name)
                    .expect("key checked above")
                    .is_empty(),
                "expected symbol {name} to have at least one definition"
            );
        }

        let inner = indexes
            .symbols
            .symbols
            .get("Inner")
            .unwrap()
            .iter()
            .find(|sym| sym.qualified_name == "com.example.A.Inner")
            .expect("expected Inner to have qualified name com.example.A.Inner");
        assert_eq!(inner.kind, IndexSymbolKind::Class);
        assert_eq!(inner.container_name.as_deref(), Some("com.example.A"));
        assert!(inner.ast_id > 0, "expected Inner ast_id to be non-zero");

        let field = indexes
            .symbols
            .symbols
            .get("field")
            .unwrap()
            .iter()
            .find(|sym| sym.qualified_name == "com.example.A.field")
            .expect("expected field to have qualified name com.example.A.field");
        assert_eq!(field.kind, IndexSymbolKind::Field);
        assert_eq!(field.container_name.as_deref(), Some("com.example.A"));

        let method = indexes
            .symbols
            .symbols
            .get("m")
            .unwrap()
            .iter()
            .find(|sym| sym.qualified_name == "com.example.A.m")
            .expect("expected method m to have qualified name com.example.A.m");
        assert_eq!(method.kind, IndexSymbolKind::Method);
        assert_eq!(method.container_name.as_deref(), Some("com.example.A"));

        // Inheritance edges (from rowan AST).
        assert!(
            indexes.inheritance.supertypes.get("A").is_some(),
            "expected inheritance edges for A"
        );
        let supertypes = indexes.inheritance.supertypes.get("A").unwrap();
        assert!(supertypes.contains(&"B".to_string()));
        assert!(supertypes.contains(&"I1".to_string()));
        assert!(supertypes.contains(&"I2".to_string()));

        // Annotations (from rowan AST).
        assert!(indexes.annotations.annotations.contains_key("@Anno"));
        assert!(indexes.annotations.annotations.contains_key("@Inject"));
        assert!(indexes.annotations.annotations.contains_key("@Param"));
    }

    #[test]
    fn collect_type_references_supports_dollar_sign() {
        let mut refs = BTreeSet::new();
        collect_type_references("com.example.Foo$Bar", &mut refs);
        assert_eq!(
            refs,
            BTreeSet::from([String::from("Foo$Bar")]),
            "expected qualified types containing `$` to keep the full simple name"
        );

        refs.clear();
        collect_type_references("Foo$Bar", &mut refs);
        assert_eq!(
            refs,
            BTreeSet::from([String::from("Foo$Bar")]),
            "expected unqualified types containing `$` to keep the full simple name"
        );
    }

    #[test]
    fn collect_type_references_handles_qualified_and_generic_types() {
        let mut refs = BTreeSet::new();
        collect_type_references(
            "java.util.Map<String, java.util.List<com.example.Foo>>",
            &mut refs,
        );
        let expected = BTreeSet::from_iter(["Map", "String", "List", "Foo"].map(String::from));
        assert_eq!(refs, expected);
    }
}
