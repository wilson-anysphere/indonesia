use crate::ast_id::{span_to_text_range, AstId, AstIdMap};
use crate::hir::{
    Arena, AssignOp, BinaryOp, Body, CatchClause, Expr, ExprId, LambdaBody, LambdaParam,
    LiteralKind, Local, LocalId, Stmt, StmtId, UnaryOp,
};
use crate::ids::{
    AnnotationId, ClassId, ConstructorId, EnumId, FieldId, InitializerId, InterfaceId, MethodId,
    RecordId,
};
use crate::item_tree::{
    Annotation, AnnotationUse, Class, Constructor, Enum, Field, FieldKind, Import, Initializer,
    Interface, Item, ItemTree, Member, Method, Modifiers, ModuleDecl, ModuleDirective, PackageDecl,
    Param, Record, RecordComponent, TypeParam,
};
use nova_syntax::java::ast as syntax;
use nova_syntax::{JavaParseResult, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};
use nova_types::Span;
use nova_vfs::FileId;

#[must_use]
pub fn lower_item_tree(
    file: FileId,
    unit: &syntax::CompilationUnit,
    parse: &JavaParseResult,
    ast_id_map: &AstIdMap,
) -> ItemTree {
    lower_item_tree_with(file, unit, parse, ast_id_map, &mut || {})
}

#[must_use]
pub fn lower_item_tree_with(
    file: FileId,
    unit: &syntax::CompilationUnit,
    parse: &JavaParseResult,
    ast_id_map: &AstIdMap,
    check_cancelled: &mut dyn FnMut(),
) -> ItemTree {
    let mut ctx = ItemTreeLower {
        file,
        parse,
        ast_id_map,
        tree: ItemTree::default(),
        check_cancelled,
    };
    ctx.lower_compilation_unit(unit);
    ctx.tree
}

struct ItemTreeLower<'a> {
    file: FileId,
    parse: &'a JavaParseResult,
    ast_id_map: &'a AstIdMap,
    tree: ItemTree,
    check_cancelled: &'a mut dyn FnMut(),
}

impl ItemTreeLower<'_> {
    fn check_cancelled(&mut self) {
        let check = &mut *self.check_cancelled;
        check();
    }

    fn lower_compilation_unit(&mut self, unit: &syntax::CompilationUnit) {
        self.check_cancelled();
        self.tree.package = unit.package.as_ref().map(|pkg| PackageDecl {
            name: pkg.name.trim().to_string(),
            range: pkg.range,
        });

        self.tree.imports = {
            let mut imports = Vec::with_capacity(unit.imports.len());
            for import in &unit.imports {
                self.check_cancelled();
                imports.push(Import {
                    is_static: import.is_static,
                    is_star: import.is_star,
                    path: import.path.clone(),
                    range: import.range,
                });
            }
            imports
        };

        self.tree.module = unit
            .module
            .as_ref()
            .map(|module| self.lower_module_decl(module));

        for decl in &unit.types {
            self.check_cancelled();
            if let Some(item) = self.lower_type_decl(decl) {
                self.tree.items.push(item);
            }
        }
    }

    fn lower_type_decl(&mut self, decl: &syntax::TypeDecl) -> Option<Item> {
        self.check_cancelled();
        match decl {
            syntax::TypeDecl::Class(class) => {
                let members = self.lower_members(&class.members);
                let node =
                    self.syntax_node_for_name(SyntaxKind::ClassDeclaration, class.name_range)?;
                let ast_id = self.ast_id_map.ast_id(&node)?;
                let id = ClassId::new(self.file, ast_id);
                let type_params = lower_type_params(&node);
                let (extends, extends_ranges) =
                    collect_clause_types(&node, SyntaxKind::ExtendsClause);
                let (implements, implements_ranges) =
                    collect_clause_types(&node, SyntaxKind::ImplementsClause);
                let (permits, permits_ranges) =
                    collect_clause_types(&node, SyntaxKind::PermitsClause);
                self.tree.classes.insert(
                    ast_id,
                    Class {
                        name: class.name.clone(),
                        name_range: class.name_range,
                        modifiers: lower_modifiers(class.modifiers),
                        annotations: lower_annotation_uses(&class.annotations),
                        type_params,
                        extends,
                        extends_ranges,
                        implements,
                        implements_ranges,
                        permits,
                        permits_ranges,
                        range: class.range,
                        body_range: class.body_range,
                        members,
                    },
                );
                Some(Item::Class(id))
            }
            syntax::TypeDecl::Interface(interface) => {
                let members = self.lower_members(&interface.members);
                let node = self
                    .syntax_node_for_name(SyntaxKind::InterfaceDeclaration, interface.name_range)?;
                let ast_id = self.ast_id_map.ast_id(&node)?;
                let id = InterfaceId::new(self.file, ast_id);
                let type_params = lower_type_params(&node);
                let (extends, extends_ranges) =
                    collect_clause_types(&node, SyntaxKind::ExtendsClause);
                let (permits, permits_ranges) =
                    collect_clause_types(&node, SyntaxKind::PermitsClause);
                self.tree.interfaces.insert(
                    ast_id,
                    Interface {
                        name: interface.name.clone(),
                        name_range: interface.name_range,
                        modifiers: lower_modifiers(interface.modifiers),
                        annotations: lower_annotation_uses(&interface.annotations),
                        type_params,
                        extends,
                        extends_ranges,
                        permits,
                        permits_ranges,
                        range: interface.range,
                        body_range: interface.body_range,
                        members,
                    },
                );
                Some(Item::Interface(id))
            }
            syntax::TypeDecl::Enum(enm) => {
                let mut members = Vec::new();

                for constant in &enm.constants {
                    self.check_cancelled();
                    if let Some(member) = self.lower_enum_constant(&enm.name, constant) {
                        members.push(member);
                    }
                }

                members.extend(self.lower_members(&enm.members));

                let node =
                    self.syntax_node_for_name(SyntaxKind::EnumDeclaration, enm.name_range)?;
                let ast_id = self.ast_id_map.ast_id(&node)?;
                let id = EnumId::new(self.file, ast_id);
                let (implements, implements_ranges) = collect_direct_child_types_after_token(
                    &node,
                    SyntaxKind::ImplementsKw,
                    SyntaxKind::EnumBody,
                );
                let (permits, permits_ranges) =
                    collect_clause_types(&node, SyntaxKind::PermitsClause);
                self.tree.enums.insert(
                    ast_id,
                    Enum {
                        name: enm.name.clone(),
                        name_range: enm.name_range,
                        modifiers: lower_modifiers(enm.modifiers),
                        annotations: lower_annotation_uses(&enm.annotations),
                        implements,
                        implements_ranges,
                        permits,
                        permits_ranges,
                        range: enm.range,
                        body_range: enm.body_range,
                        members,
                    },
                );
                Some(Item::Enum(id))
            }
            syntax::TypeDecl::Record(record) => {
                let node =
                    self.syntax_node_for_name(SyntaxKind::RecordDeclaration, record.name_range)?;
                let ast_id = self.ast_id_map.ast_id(&node)?;
                let (mut members, components) = self.lower_record_components(&node);
                members.extend(self.lower_members(&record.members));
                let header_params = self.lower_record_header_params(&node);
                if !header_params.is_empty() {
                    for member in &members {
                        let Member::Constructor(ctor_id) = *member else {
                            continue;
                        };
                        let is_compact = self.ast_id_map.ptr(ctor_id.ast_id).is_some_and(|ptr| {
                            ptr.kind == SyntaxKind::CompactConstructorDeclaration
                        });
                        if !is_compact {
                            continue;
                        }
                        if let Some(ctor) = self.tree.constructors.get_mut(&ctor_id.ast_id) {
                            ctor.params = header_params.clone();
                        }
                    }
                }
                let id = RecordId::new(self.file, ast_id);
                let type_params = lower_type_params(&node);
                let (implements, implements_ranges) = collect_direct_child_types_after_token(
                    &node,
                    SyntaxKind::ImplementsKw,
                    SyntaxKind::RecordBody,
                );
                let (permits, permits_ranges) =
                    collect_clause_types(&node, SyntaxKind::PermitsClause);
                self.tree.records.insert(
                    ast_id,
                    Record {
                        name: record.name.clone(),
                        name_range: record.name_range,
                        modifiers: lower_modifiers(record.modifiers),
                        annotations: lower_annotation_uses(&record.annotations),
                        type_params,
                        implements,
                        implements_ranges,
                        permits,
                        permits_ranges,
                        components,
                        range: record.range,
                        body_range: record.body_range,
                        members,
                    },
                );
                Some(Item::Record(id))
            }
            syntax::TypeDecl::Annotation(annotation) => {
                let members = self.lower_members(&annotation.members);
                let ast_id = self.ast_id_for_name(
                    SyntaxKind::AnnotationTypeDeclaration,
                    annotation.name_range,
                )?;
                let id = AnnotationId::new(self.file, ast_id);
                self.tree.annotations.insert(
                    ast_id,
                    Annotation {
                        name: annotation.name.clone(),
                        name_range: annotation.name_range,
                        modifiers: lower_modifiers(annotation.modifiers),
                        annotations: lower_annotation_uses(&annotation.annotations),
                        range: annotation.range,
                        body_range: annotation.body_range,
                        members,
                    },
                );
                Some(Item::Annotation(id))
            }
        }
    }

    fn lower_members(&mut self, members: &[syntax::MemberDecl]) -> Vec<Member> {
        let mut lowered = Vec::new();
        for member in members {
            self.check_cancelled();
            if let Some(member) = self.lower_member(member) {
                lowered.push(member);
            }
        }
        lowered
    }

    fn lower_member(&mut self, member: &syntax::MemberDecl) -> Option<Member> {
        self.check_cancelled();
        match member {
            syntax::MemberDecl::Field(field) => {
                let ast_id =
                    self.ast_id_for_name(SyntaxKind::VariableDeclarator, field.name_range)?;
                let id = FieldId::new(self.file, ast_id);
                self.tree.fields.insert(
                    ast_id,
                    Field {
                        kind: FieldKind::Field,
                        modifiers: lower_modifiers(field.modifiers),
                        annotations: lower_annotation_uses(&field.annotations),
                        ty: field.ty.text.clone(),
                        ty_range: field.ty.range,
                        name: field.name.clone(),
                        range: field.range,
                        name_range: field.name_range,
                    },
                );
                Some(Member::Field(id))
            }
            syntax::MemberDecl::Method(method) => {
                let node =
                    self.syntax_node_for_name(SyntaxKind::MethodDeclaration, method.name_range)?;
                let ast_id = self.ast_id_map.ast_id(&node)?;
                let id = MethodId::new(self.file, ast_id);
                let type_params = lower_type_params(&node);
                let params = {
                    let mut params = Vec::with_capacity(method.params.len());
                    for param in &method.params {
                        self.check_cancelled();
                        params.push(lower_param(param));
                    }
                    params
                };
                let (throws, throws_ranges) = collect_throws_clause_types(&node, SyntaxKind::Block);
                let body = method
                    .body
                    .as_ref()
                    .and_then(|block| self.ast_id_for_range(SyntaxKind::Block, block.range));
                self.tree.methods.insert(
                    ast_id,
                    Method {
                        modifiers: lower_modifiers(method.modifiers),
                        annotations: lower_annotation_uses(&method.annotations),
                        type_params,
                        return_ty: method.return_ty.text.clone(),
                        return_ty_range: method.return_ty.range,
                        name: method.name.clone(),
                        range: method.range,
                        name_range: method.name_range,
                        params,
                        throws,
                        throws_ranges,
                        body,
                    },
                );
                Some(Member::Method(id))
            }
            syntax::MemberDecl::Constructor(cons) => {
                let node = self
                    .syntax_node_for_name(SyntaxKind::ConstructorDeclaration, cons.name_range)
                    .or_else(|| {
                        self.syntax_node_for_name(
                            SyntaxKind::CompactConstructorDeclaration,
                            cons.name_range,
                        )
                    })?;
                let ast_id = self.ast_id_map.ast_id(&node)?;
                let id = ConstructorId::new(self.file, ast_id);
                let type_params = lower_type_params(&node);
                let params = {
                    let mut params = Vec::with_capacity(cons.params.len());
                    for param in &cons.params {
                        self.check_cancelled();
                        params.push(lower_param(param));
                    }
                    params
                };
                let (throws, throws_ranges) = collect_throws_clause_types(&node, SyntaxKind::Block);
                let body = self.ast_id_for_range(SyntaxKind::Block, cons.body.range);
                self.tree.constructors.insert(
                    ast_id,
                    Constructor {
                        modifiers: lower_modifiers(cons.modifiers),
                        annotations: lower_annotation_uses(&cons.annotations),
                        type_params,
                        name: cons.name.clone(),
                        range: cons.range,
                        name_range: cons.name_range,
                        params,
                        throws,
                        throws_ranges,
                        body,
                    },
                );
                Some(Member::Constructor(id))
            }
            syntax::MemberDecl::Initializer(init) => {
                let ast_id = self.ast_id_for_range(SyntaxKind::InitializerBlock, init.range)?;
                let id = InitializerId::new(self.file, ast_id);
                let body = self.ast_id_for_range(SyntaxKind::Block, init.body.range);
                self.tree.initializers.insert(
                    ast_id,
                    Initializer {
                        is_static: init.is_static,
                        range: init.range,
                        body,
                    },
                );
                Some(Member::Initializer(id))
            }
            syntax::MemberDecl::Type(decl) => self.lower_type_decl(decl).map(Member::Type),
        }
    }

    fn lower_enum_constant(
        &mut self,
        enum_name: &str,
        constant: &syntax::EnumConstantDecl,
    ) -> Option<Member> {
        self.check_cancelled();
        let ast_id = self.ast_id_for_name(SyntaxKind::EnumConstant, constant.name_range)?;
        let id = FieldId::new(self.file, ast_id);
        self.tree.fields.insert(
            ast_id,
            Field {
                kind: FieldKind::EnumConstant,
                modifiers: Modifiers::default(),
                annotations: Vec::new(),
                ty: enum_name.to_string(),
                ty_range: Span::new(constant.name_range.start, constant.name_range.start),
                name: constant.name.clone(),
                range: constant.range,
                name_range: constant.name_range,
            },
        );
        Some(Member::Field(id))
    }

    fn lower_module_decl(&mut self, module: &syntax::ModuleDecl) -> ModuleDecl {
        self.check_cancelled();
        let directives = module
            .directives
            .iter()
            .map(|directive| {
                self.check_cancelled();
                lower_module_directive(directive)
            })
            .collect();

        ModuleDecl {
            name: module.name.clone(),
            name_range: module.name_range,
            is_open: module.is_open,
            directives,
            range: module.range,
            body_range: module.body_range,
        }
    }

    fn ast_id_for_range(&self, kind: SyntaxKind, range: Span) -> Option<AstId> {
        let text_range = span_to_text_range(range);
        if let Some(id) = self.ast_id_map.ast_id_for_ptr(kind, text_range) {
            return Some(id);
        }

        let offset = u32::try_from(range.start).expect("range start does not fit in u32");
        self.ast_id_for_offset(kind, offset)
    }

    fn ast_id_for_name(&self, kind: SyntaxKind, name_range: Span) -> Option<AstId> {
        let offset = u32::try_from(name_range.start).expect("name start does not fit in u32");
        self.ast_id_for_offset(kind, offset)
    }

    fn syntax_node_for_name(&self, kind: SyntaxKind, name_range: Span) -> Option<SyntaxNode> {
        let offset = u32::try_from(name_range.start).expect("name start does not fit in u32");
        self.syntax_node_for_offset(kind, offset)
    }

    fn ast_id_for_offset(&self, kind: SyntaxKind, offset: u32) -> Option<AstId> {
        let node = self.syntax_node_for_offset(kind, offset)?;
        self.ast_id_map.ast_id(&node)
    }

    fn syntax_node_for_offset(&self, kind: SyntaxKind, offset: u32) -> Option<SyntaxNode> {
        let token = self.parse.token_at_offset(offset).right_biased()?;
        token
            .parent()?
            .ancestors()
            .find(|ancestor| ancestor.kind() == kind)
    }

    fn lower_record_components(
        &mut self,
        record_decl: &SyntaxNode,
    ) -> (Vec<Member>, Vec<RecordComponent>) {
        let Some(param_list) = record_decl
            .children()
            .find(|child| child.kind() == SyntaxKind::ParameterList)
        else {
            return (Vec::new(), Vec::new());
        };

        let mut members = Vec::new();
        let mut components = Vec::new();

        for param in param_list
            .children()
            .filter(|child| child.kind() == SyntaxKind::Parameter)
        {
            self.check_cancelled();

            let Some(ast_id) = self.ast_id_map.ast_id(&param) else {
                continue;
            };

            let Some((ty, ty_range, name, range, name_range)) = lower_parameter_signature(&param)
            else {
                continue;
            };

            components.push(RecordComponent {
                ty: ty.clone(),
                ty_range,
                name: name.clone(),
                name_range,
            });

            let id = FieldId::new(self.file, ast_id);
            self.tree.fields.insert(
                ast_id,
                Field {
                    kind: FieldKind::RecordComponent,
                    modifiers: Modifiers::default(),
                    annotations: Vec::new(),
                    ty,
                    ty_range,
                    name,
                    range,
                    name_range,
                },
            );
            members.push(Member::Field(id));
        }

        (members, components)
    }

    fn lower_record_header_params(&mut self, record_decl: &SyntaxNode) -> Vec<Param> {
        let Some(param_list) = record_decl
            .children()
            .find(|child| child.kind() == SyntaxKind::ParameterList)
        else {
            return Vec::new();
        };

        let mut params = Vec::new();
        for param in param_list
            .children()
            .filter(|child| child.kind() == SyntaxKind::Parameter)
        {
            self.check_cancelled();

            let Some((ty, ty_range, name, range, name_range)) = lower_parameter_signature(&param)
            else {
                continue;
            };

            let (modifiers, annotations) = lower_param_modifiers_and_annotations(&param);

            let is_varargs = param
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|tok| tok.kind() == SyntaxKind::Ellipsis);

            params.push(Param {
                modifiers,
                annotations,
                ty,
                ty_range,
                is_varargs,
                name,
                range,
                name_range,
            });
        }
        params
    }
}

fn lower_param_modifiers_and_annotations(param: &SyntaxNode) -> (Modifiers, Vec<AnnotationUse>) {
    let Some(mods_node) = param
        .children()
        .find(|child| child.kind() == SyntaxKind::Modifiers)
    else {
        return (Modifiers::default(), Vec::new());
    };

    let mut modifiers = Modifiers::default();
    let mut annotations = Vec::new();

    for child in mods_node.children_with_tokens() {
        match child {
            SyntaxElement::Node(node) if node.kind() == SyntaxKind::Annotation => {
                if let Some(use_) = lower_rowan_annotation_use(&node) {
                    annotations.push(use_);
                }
            }
            SyntaxElement::Token(tok) => {
                modifiers.raw |= match tok.kind() {
                    SyntaxKind::PublicKw => Modifiers::PUBLIC,
                    SyntaxKind::ProtectedKw => Modifiers::PROTECTED,
                    SyntaxKind::PrivateKw => Modifiers::PRIVATE,
                    SyntaxKind::StaticKw => Modifiers::STATIC,
                    SyntaxKind::FinalKw => Modifiers::FINAL,
                    SyntaxKind::AbstractKw => Modifiers::ABSTRACT,
                    SyntaxKind::NativeKw => Modifiers::NATIVE,
                    SyntaxKind::SynchronizedKw => Modifiers::SYNCHRONIZED,
                    SyntaxKind::TransientKw => Modifiers::TRANSIENT,
                    SyntaxKind::VolatileKw => Modifiers::VOLATILE,
                    SyntaxKind::StrictfpKw => Modifiers::STRICTFP,
                    SyntaxKind::DefaultKw => Modifiers::DEFAULT,
                    SyntaxKind::SealedKw => Modifiers::SEALED,
                    SyntaxKind::NonSealedKw => Modifiers::NON_SEALED,
                    _ => 0,
                };
            }
            _ => {}
        }
    }

    (modifiers, annotations)
}

fn lower_rowan_annotation_use(node: &SyntaxNode) -> Option<AnnotationUse> {
    let name_node = node
        .children()
        .find(|child| child.kind() == SyntaxKind::Name)?;
    let name = non_trivia_text(&name_node);
    Some(AnnotationUse {
        name,
        range: node_text_range_to_span(node),
    })
}

fn lower_modifiers(modifiers: syntax::Modifiers) -> Modifiers {
    Modifiers { raw: modifiers.raw }
}

fn lower_annotation_uses(annotations: &[syntax::AnnotationUse]) -> Vec<AnnotationUse> {
    annotations
        .iter()
        .map(|annotation| AnnotationUse {
            name: annotation.name.clone(),
            range: annotation.range,
        })
        .collect()
}

fn lower_module_directive(directive: &syntax::ModuleDirective) -> ModuleDirective {
    match directive {
        syntax::ModuleDirective::Requires {
            module,
            is_transitive,
            is_static,
            range,
        } => ModuleDirective::Requires {
            module: module.clone(),
            is_transitive: *is_transitive,
            is_static: *is_static,
            range: *range,
        },
        syntax::ModuleDirective::Exports { package, to, range } => ModuleDirective::Exports {
            package: package.clone(),
            to: to.clone(),
            range: *range,
        },
        syntax::ModuleDirective::Opens { package, to, range } => ModuleDirective::Opens {
            package: package.clone(),
            to: to.clone(),
            range: *range,
        },
        syntax::ModuleDirective::Uses { service, range } => ModuleDirective::Uses {
            service: service.clone(),
            range: *range,
        },
        syntax::ModuleDirective::Provides {
            service,
            implementations,
            range,
        } => ModuleDirective::Provides {
            service: service.clone(),
            implementations: implementations.clone(),
            range: *range,
        },
        syntax::ModuleDirective::Unknown { range } => ModuleDirective::Unknown { range: *range },
    }
}

fn lower_param(param: &syntax::ParamDecl) -> Param {
    Param {
        modifiers: lower_modifiers(param.modifiers),
        annotations: lower_annotation_uses(&param.annotations),
        ty: param.ty.text.clone(),
        ty_range: param.ty.range,
        is_varargs: param.is_varargs,
        name: param.name.clone(),
        range: param.range,
        name_range: param.name_range,
    }
}

fn lower_type_params(node: &SyntaxNode) -> Vec<TypeParam> {
    let Some(type_params) = node
        .children()
        .find(|child| child.kind() == SyntaxKind::TypeParameters)
    else {
        return Vec::new();
    };

    type_params
        .children()
        .filter(|child| child.kind() == SyntaxKind::TypeParameter)
        .filter_map(|type_param| lower_type_param(&type_param))
        .collect()
}

fn lower_type_param(node: &SyntaxNode) -> Option<TypeParam> {
    let name_tok = node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind().is_identifier_like())?;

    let name = name_tok.text().to_string();
    let name_range = token_text_range_to_span(&name_tok);
    let mut bounds = Vec::new();
    let mut bounds_ranges = Vec::new();
    for ty in node
        .children()
        .filter(|child| child.kind() == SyntaxKind::Type)
    {
        bounds.push(non_trivia_text(&ty));
        bounds_ranges.push(non_trivia_span(&ty).unwrap_or_else(|| node_text_range_to_span(&ty)));
    }

    Some(TypeParam {
        name,
        name_range,
        bounds,
        bounds_ranges,
    })
}

fn collect_clause_types(decl: &SyntaxNode, clause_kind: SyntaxKind) -> (Vec<String>, Vec<Span>) {
    let Some(clause) = decl.children().find(|child| child.kind() == clause_kind) else {
        return (Vec::new(), Vec::new());
    };

    let mut types = Vec::new();
    let mut ranges = Vec::new();
    for ty in clause
        .children()
        .filter(|child| child.kind() == SyntaxKind::Type)
    {
        types.push(non_trivia_text(&ty));
        ranges.push(non_trivia_span(&ty).unwrap_or_else(|| node_text_range_to_span(&ty)));
    }
    (types, ranges)
}

fn collect_direct_child_types_after_token(
    decl: &SyntaxNode,
    keyword: SyntaxKind,
    end_node_kind: SyntaxKind,
) -> (Vec<String>, Vec<Span>) {
    let Some(keyword_end) = decl
        .children_with_tokens()
        .filter_map(|child| child.into_token())
        .find(|tok| tok.kind() == keyword)
        .map(|tok| tok.text_range().end())
    else {
        return (Vec::new(), Vec::new());
    };

    let end_start = decl
        .children()
        .find(|child| child.kind() == end_node_kind)
        .map(|node| node.text_range().start())
        .unwrap_or_else(|| decl.text_range().end());

    let mut types = Vec::new();
    let mut ranges = Vec::new();
    for ty in decl
        .children()
        .filter(|child| child.kind() == SyntaxKind::Type)
        .filter(|ty| ty.text_range().start() >= keyword_end && ty.text_range().end() <= end_start)
    {
        types.push(non_trivia_text(&ty));
        ranges.push(non_trivia_span(&ty).unwrap_or_else(|| node_text_range_to_span(&ty)));
    }

    (types, ranges)
}

fn collect_throws_clause_types(
    decl: &SyntaxNode,
    end_node_kind: SyntaxKind,
) -> (Vec<String>, Vec<Span>) {
    let signature_end = decl
        .children()
        .find(|child| child.kind() == end_node_kind)
        .map(|node| node.text_range().start())
        .unwrap_or_else(|| decl.text_range().end());

    let throws_clauses: Vec<_> = decl
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::ThrowsClause)
        .filter(|node| node.text_range().end() <= signature_end)
        .collect();

    // Prefer structured `ThrowsClause` nodes when present in the rowan tree.
    if !throws_clauses.is_empty() {
        let mut types = Vec::new();
        let mut ranges = Vec::new();
        for clause in throws_clauses {
            for ty in clause
                .descendants()
                .filter(|n| n.kind() == SyntaxKind::Type)
            {
                // Only keep the outermost `Type` nodes within the throws clause. This avoids
                // treating type arguments (e.g. `Foo<Bar>`) as separate thrown types.
                let nested_in_type = ty
                    .ancestors()
                    .skip(1)
                    .take_while(|a| a.kind() != SyntaxKind::ThrowsClause)
                    .any(|a| a.kind() == SyntaxKind::Type);
                if nested_in_type {
                    continue;
                }
                types.push(non_trivia_text(&ty));
                ranges.push(non_trivia_span(&ty).unwrap_or_else(|| node_text_range_to_span(&ty)));
            }
        }
        return (types, ranges);
    }

    // Fallback for syntax trees that don't wrap `throws` in a `ThrowsClause` node: search for
    // `Type` nodes that appear after the `throws` keyword.
    collect_direct_child_types_after_token(decl, SyntaxKind::ThrowsKw, end_node_kind)
}

fn non_trivia_text(node: &SyntaxNode) -> String {
    let mut text = String::new();
    for tok in node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
    {
        if tok.kind().is_trivia() || tok.kind() == SyntaxKind::Eof {
            continue;
        }
        text.push_str(tok.text());
    }
    text
}

fn non_trivia_span(node: &SyntaxNode) -> Option<Span> {
    let mut tokens = node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof);

    let first = tokens.next()?;
    let mut last = first.clone();
    for tok in tokens {
        last = tok;
    }

    let start = token_text_range_to_span(&first).start;
    let end = token_text_range_to_span(&last).end;
    Some(Span::new(start, end))
}

fn node_text_range_to_span(node: &SyntaxNode) -> Span {
    let range = node.text_range();
    Span::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn token_text_range_to_span(tok: &SyntaxToken) -> Span {
    let range = tok.text_range();
    Span::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

fn lower_parameter_signature(param: &SyntaxNode) -> Option<(String, Span, String, Span, Span)> {
    let range = node_text_range_to_span(param);

    let ty_node = param
        .children()
        .find(|child| child.kind() == SyntaxKind::Type)?;
    let mut ty_range =
        non_trivia_span(&ty_node).unwrap_or_else(|| node_text_range_to_span(&ty_node));
    let mut ty = non_trivia_text(&ty_node);

    if let Some(ellipsis) = param
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::Ellipsis)
    {
        ty.push_str("...");
        ty_range.end = u32::from(ellipsis.text_range().end()) as usize;
    }

    let mut name: Option<String> = None;
    let mut name_range: Option<Span> = None;
    let mut saw_name = false;
    let mut dims = 0usize;
    let mut saw_lbracket = false;

    for tok in param
        .children_with_tokens()
        .filter_map(|el| el.into_token())
    {
        if tok.kind().is_trivia() {
            continue;
        }

        if !saw_name && tok.kind().is_identifier_like() {
            name = Some(tok.text().to_string());
            name_range = Some(token_text_range_to_span(&tok));
            saw_name = true;
            continue;
        }

        if !saw_name {
            continue;
        }

        match tok.kind() {
            SyntaxKind::LBracket => saw_lbracket = true,
            SyntaxKind::RBracket if saw_lbracket => {
                dims += 1;
                saw_lbracket = false;
            }
            _ => saw_lbracket = false,
        }
    }

    ty.push_str(&"[]".repeat(dims));

    Some((ty, ty_range, name?, range, name_range?))
}

#[must_use]
pub fn lower_body(block: &syntax::Block) -> Body {
    lower_body_with(block, &mut || {})
}

#[must_use]
pub fn lower_body_with(block: &syntax::Block, check_cancelled: &mut dyn FnMut()) -> Body {
    let mut ctx = BodyLower::new(check_cancelled);
    let root = ctx.lower_block(block);
    Body {
        root,
        stmts: ctx.stmts,
        exprs: ctx.exprs,
        locals: ctx.locals,
    }
}

struct BodyLower<'a> {
    stmts: Arena<Stmt>,
    exprs: Arena<Expr>,
    locals: Arena<Local>,
    check_cancelled: &'a mut dyn FnMut(),
}

impl<'a> BodyLower<'a> {
    fn new(check_cancelled: &'a mut dyn FnMut()) -> Self {
        Self {
            stmts: Arena::default(),
            exprs: Arena::default(),
            locals: Arena::default(),
            check_cancelled,
        }
    }

    fn check_cancelled(&mut self) {
        let check = &mut *self.check_cancelled;
        check();
    }

    fn alloc_stmt(&mut self, stmt: Stmt) -> StmtId {
        StmtId::from_raw(self.stmts.alloc(stmt))
    }

    fn alloc_expr(&mut self, expr: Expr) -> ExprId {
        ExprId::from_raw(self.exprs.alloc(expr))
    }

    fn alloc_local(&mut self, local: Local) -> LocalId {
        LocalId::from_raw(self.locals.alloc(local))
    }

    fn lower_block(&mut self, block: &syntax::Block) -> StmtId {
        self.check_cancelled();
        let mut statements = Vec::new();
        for stmt in &block.statements {
            self.check_cancelled();
            if let Some(stmt) = self.lower_stmt(stmt) {
                statements.push(stmt);
            }
        }
        self.alloc_stmt(Stmt::Block {
            statements,
            range: block.range,
        })
    }

    fn lower_stmt(&mut self, stmt: &syntax::Stmt) -> Option<StmtId> {
        self.check_cancelled();
        match stmt {
            syntax::Stmt::LocalVar(local) => {
                let local_id = self.alloc_local(Local {
                    ty_text: local.ty.text.clone(),
                    ty_range: local.ty.range,
                    name: local.name.clone(),
                    name_range: local.name_range,
                    range: local.range,
                });
                let initializer = local.initializer.as_ref().map(|expr| self.lower_expr(expr));
                Some(self.alloc_stmt(Stmt::Let {
                    local: local_id,
                    initializer,
                    range: local.range,
                }))
            }
            syntax::Stmt::Expr(expr_stmt) => {
                let expr = self.lower_expr(&expr_stmt.expr);
                Some(self.alloc_stmt(Stmt::Expr {
                    expr,
                    range: expr_stmt.range,
                }))
            }
            syntax::Stmt::Return(ret) => {
                let expr = ret.expr.as_ref().map(|expr| self.lower_expr(expr));
                Some(self.alloc_stmt(Stmt::Return {
                    expr,
                    range: ret.range,
                }))
            }
            syntax::Stmt::Block(block) => Some(self.lower_block(block)),
            syntax::Stmt::If(if_stmt) => {
                let condition = self.lower_expr(&if_stmt.condition);
                let then_branch = self
                    .lower_stmt(if_stmt.then_branch.as_ref())
                    .unwrap_or_else(|| {
                        self.alloc_stmt(Stmt::Empty {
                            range: if_stmt.range,
                        })
                    });
                let else_branch = if_stmt
                    .else_branch
                    .as_ref()
                    .and_then(|stmt| self.lower_stmt(stmt.as_ref()));
                Some(self.alloc_stmt(Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    range: if_stmt.range,
                }))
            }
            syntax::Stmt::While(while_stmt) => {
                let condition = self.lower_expr(&while_stmt.condition);
                let body = self
                    .lower_stmt(while_stmt.body.as_ref())
                    .unwrap_or_else(|| {
                        self.alloc_stmt(Stmt::Empty {
                            range: while_stmt.range,
                        })
                    });
                Some(self.alloc_stmt(Stmt::While {
                    condition,
                    body,
                    range: while_stmt.range,
                }))
            }
            syntax::Stmt::For(for_stmt) => {
                let mut init = Vec::with_capacity(for_stmt.init.len());
                for stmt in &for_stmt.init {
                    self.check_cancelled();
                    if let Some(stmt) = self.lower_stmt(stmt) {
                        init.push(stmt);
                    }
                }

                let condition = for_stmt
                    .condition
                    .as_ref()
                    .map(|expr| self.lower_expr(expr));

                let mut update = Vec::with_capacity(for_stmt.update.len());
                for expr in &for_stmt.update {
                    self.check_cancelled();
                    update.push(self.lower_expr(expr));
                }

                let body = self.lower_stmt(for_stmt.body.as_ref()).unwrap_or_else(|| {
                    self.alloc_stmt(Stmt::Empty {
                        range: for_stmt.range,
                    })
                });

                Some(self.alloc_stmt(Stmt::For {
                    init,
                    condition,
                    update,
                    body,
                    range: for_stmt.range,
                }))
            }
            syntax::Stmt::ForEach(for_each) => {
                let local_id = self.alloc_local(Local {
                    ty_text: for_each.var.ty.text.clone(),
                    ty_range: for_each.var.ty.range,
                    name: for_each.var.name.clone(),
                    name_range: for_each.var.name_range,
                    range: for_each.var.range,
                });
                let iterable = self.lower_expr(&for_each.iterable);
                let body = self.lower_stmt(for_each.body.as_ref()).unwrap_or_else(|| {
                    self.alloc_stmt(Stmt::Empty {
                        range: for_each.range,
                    })
                });
                Some(self.alloc_stmt(Stmt::ForEach {
                    local: local_id,
                    iterable,
                    body,
                    range: for_each.range,
                }))
            }
            syntax::Stmt::Synchronized(sync) => {
                let expr = self.lower_expr(&sync.expr);
                let body = self.lower_block(&sync.body);
                Some(self.alloc_stmt(Stmt::Synchronized {
                    expr,
                    body,
                    range: sync.range,
                }))
            }
            syntax::Stmt::Switch(switch_stmt) => {
                let selector = self.lower_expr(&switch_stmt.selector);
                let body = self.lower_block(&switch_stmt.body);
                Some(self.alloc_stmt(Stmt::Switch {
                    selector,
                    body,
                    range: switch_stmt.range,
                }))
            }
            syntax::Stmt::Try(try_stmt) => {
                let body = self.lower_block(&try_stmt.body);
                let mut catches = Vec::with_capacity(try_stmt.catches.len());
                for clause in &try_stmt.catches {
                    self.check_cancelled();
                    let param = &clause.param;
                    let local_id = self.alloc_local(Local {
                        ty_text: param.ty.text.clone(),
                        ty_range: param.ty.range,
                        name: param.name.clone(),
                        name_range: param.name_range,
                        range: param.range,
                    });
                    let body = self.lower_block(&clause.body);
                    catches.push(CatchClause {
                        param: local_id,
                        body,
                        range: clause.range,
                    });
                }

                let finally = try_stmt
                    .finally
                    .as_ref()
                    .map(|block| self.lower_block(block));

                Some(self.alloc_stmt(Stmt::Try {
                    body,
                    catches,
                    finally,
                    range: try_stmt.range,
                }))
            }
            syntax::Stmt::Assert(assert_stmt) => {
                let condition = self.lower_expr(&assert_stmt.condition);
                let message = assert_stmt
                    .message
                    .as_ref()
                    .map(|expr| self.lower_expr(expr));
                Some(self.alloc_stmt(Stmt::Assert {
                    condition,
                    message,
                    range: assert_stmt.range,
                }))
            }
            syntax::Stmt::Throw(throw_stmt) => {
                let expr = self.lower_expr(&throw_stmt.expr);
                Some(self.alloc_stmt(Stmt::Throw {
                    expr,
                    range: throw_stmt.range,
                }))
            }
            syntax::Stmt::Break(range) => Some(self.alloc_stmt(Stmt::Break { range: *range })),
            syntax::Stmt::Continue(range) => {
                Some(self.alloc_stmt(Stmt::Continue { range: *range }))
            }
            syntax::Stmt::Empty(range) => Some(self.alloc_stmt(Stmt::Empty { range: *range })),
        }
    }

    fn lower_expr(&mut self, expr: &syntax::Expr) -> ExprId {
        self.check_cancelled();
        match expr {
            syntax::Expr::Name(name) => self.alloc_expr(Expr::Name {
                name: name.name.clone(),
                range: name.range,
            }),
            syntax::Expr::IntLiteral(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::Int,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::LongLiteral(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::Long,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::FloatLiteral(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::Float,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::DoubleLiteral(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::Double,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::CharLiteral(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::Char,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::StringLiteral(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::String,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::TextBlock(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::String,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::BoolLiteral(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::Bool,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::NullLiteral(range) => self.alloc_expr(Expr::Null { range: *range }),
            syntax::Expr::This(range) => self.alloc_expr(Expr::This { range: *range }),
            syntax::Expr::Super(range) => self.alloc_expr(Expr::Super { range: *range }),
            syntax::Expr::FieldAccess(access) => {
                let receiver = self.lower_expr(&access.receiver);
                self.alloc_expr(Expr::FieldAccess {
                    receiver,
                    name: access.name.clone(),
                    name_range: access.name_range,
                    range: access.range,
                })
            }
            syntax::Expr::ArrayAccess(access) => {
                let array = self.lower_expr(access.array.as_ref());
                let index = self.lower_expr(access.index.as_ref());
                self.alloc_expr(Expr::ArrayAccess {
                    array,
                    index,
                    range: access.range,
                })
            }
            syntax::Expr::Call(call) => {
                let callee = self.lower_expr(&call.callee);
                let mut args = Vec::with_capacity(call.args.len());
                for arg in &call.args {
                    self.check_cancelled();
                    args.push(self.lower_expr(arg));
                }
                let explicit_type_args = call
                    .explicit_type_args
                    .iter()
                    .map(|ty| (ty.text.clone(), ty.range))
                    .collect();
                self.alloc_expr(Expr::Call {
                    callee,
                    args,
                    explicit_type_args,
                    range: call.range,
                })
            }
            syntax::Expr::New(new_expr) => {
                let mut args = Vec::with_capacity(new_expr.args.len());
                for arg in &new_expr.args {
                    self.check_cancelled();
                    args.push(self.lower_expr(arg));
                }
                self.alloc_expr(Expr::New {
                    class: new_expr.class.text.clone(),
                    class_range: new_expr.class.range,
                    args,
                    range: new_expr.range,
                })
            }
            syntax::Expr::ArrayCreation(array) => {
                let mut dim_exprs = Vec::with_capacity(array.dim_exprs.len());
                for expr in &array.dim_exprs {
                    self.check_cancelled();
                    dim_exprs.push(self.lower_expr(expr));
                }

                let initializer = array
                    .initializer
                    .as_ref()
                    .map(|expr| self.lower_expr(expr.as_ref()));

                self.alloc_expr(Expr::ArrayCreation {
                    elem_ty_text: array.elem_ty.text.clone(),
                    elem_ty_range: array.elem_ty.range,
                    dim_exprs,
                    extra_dims: array.extra_dims,
                    initializer,
                    range: array.range,
                })
            }
            syntax::Expr::ArrayInitializer(init) => {
                let mut items = Vec::with_capacity(init.items.len());
                for item in &init.items {
                    self.check_cancelled();
                    items.push(self.lower_expr(item));
                }
                self.alloc_expr(Expr::ArrayInitializer {
                    items,
                    range: init.range,
                })
            }
            syntax::Expr::Unary(unary) => {
                let expr = self.lower_expr(&unary.expr);
                let op = match unary.op {
                    syntax::UnaryOp::Plus => UnaryOp::Plus,
                    syntax::UnaryOp::Minus => UnaryOp::Minus,
                    syntax::UnaryOp::Not => UnaryOp::Not,
                    syntax::UnaryOp::BitNot => UnaryOp::BitNot,
                    syntax::UnaryOp::PreInc => UnaryOp::PreInc,
                    syntax::UnaryOp::PreDec => UnaryOp::PreDec,
                    syntax::UnaryOp::PostInc => UnaryOp::PostInc,
                    syntax::UnaryOp::PostDec => UnaryOp::PostDec,
                };
                self.alloc_expr(Expr::Unary {
                    op,
                    expr,
                    range: unary.range,
                })
            }
            syntax::Expr::Binary(binary) => {
                let lhs = self.lower_expr(&binary.lhs);
                let rhs = self.lower_expr(&binary.rhs);
                let op = match binary.op {
                    syntax::BinaryOp::Add => BinaryOp::Add,
                    syntax::BinaryOp::Sub => BinaryOp::Sub,
                    syntax::BinaryOp::Mul => BinaryOp::Mul,
                    syntax::BinaryOp::Div => BinaryOp::Div,
                    syntax::BinaryOp::Rem => BinaryOp::Rem,
                    syntax::BinaryOp::EqEq => BinaryOp::EqEq,
                    syntax::BinaryOp::NotEq => BinaryOp::NotEq,
                    syntax::BinaryOp::Less => BinaryOp::Less,
                    syntax::BinaryOp::LessEq => BinaryOp::LessEq,
                    syntax::BinaryOp::Greater => BinaryOp::Greater,
                    syntax::BinaryOp::GreaterEq => BinaryOp::GreaterEq,
                    syntax::BinaryOp::AndAnd => BinaryOp::AndAnd,
                    syntax::BinaryOp::OrOr => BinaryOp::OrOr,
                    syntax::BinaryOp::BitAnd => BinaryOp::BitAnd,
                    syntax::BinaryOp::BitOr => BinaryOp::BitOr,
                    syntax::BinaryOp::BitXor => BinaryOp::BitXor,
                    syntax::BinaryOp::Shl => BinaryOp::Shl,
                    syntax::BinaryOp::Shr => BinaryOp::Shr,
                    syntax::BinaryOp::UShr => BinaryOp::UShr,
                };
                self.alloc_expr(Expr::Binary {
                    op,
                    lhs,
                    rhs,
                    range: binary.range,
                })
            }
            syntax::Expr::Instanceof(instanceof) => {
                let expr = self.lower_expr(instanceof.expr.as_ref());
                self.alloc_expr(Expr::Instanceof {
                    expr,
                    ty_text: instanceof.ty.text.clone(),
                    ty_range: instanceof.ty.range,
                    range: instanceof.range,
                })
            }
            syntax::Expr::MethodReference(expr) => {
                let receiver = self.lower_expr(&expr.receiver);
                self.alloc_expr(Expr::MethodReference {
                    receiver,
                    name: expr.name.clone(),
                    name_range: expr.name_range,
                    range: expr.range,
                })
            }
            syntax::Expr::ConstructorReference(expr) => {
                let receiver = self.lower_expr(&expr.receiver);
                self.alloc_expr(Expr::ConstructorReference {
                    receiver,
                    range: expr.range,
                })
            }
            syntax::Expr::ClassLiteral(expr) => {
                let ty = self.lower_expr(&expr.ty);
                self.alloc_expr(Expr::ClassLiteral {
                    ty,
                    range: expr.range,
                })
            }
            syntax::Expr::Assign(assign) => {
                let lhs = self.lower_expr(&assign.lhs);
                let rhs = self.lower_expr(&assign.rhs);
                let op = match assign.op {
                    syntax::AssignOp::Assign => AssignOp::Assign,
                    syntax::AssignOp::AddAssign => AssignOp::AddAssign,
                    syntax::AssignOp::SubAssign => AssignOp::SubAssign,
                    syntax::AssignOp::MulAssign => AssignOp::MulAssign,
                    syntax::AssignOp::DivAssign => AssignOp::DivAssign,
                    syntax::AssignOp::RemAssign => AssignOp::RemAssign,
                    syntax::AssignOp::AndAssign => AssignOp::AndAssign,
                    syntax::AssignOp::OrAssign => AssignOp::OrAssign,
                    syntax::AssignOp::XorAssign => AssignOp::XorAssign,
                    syntax::AssignOp::ShlAssign => AssignOp::ShlAssign,
                    syntax::AssignOp::ShrAssign => AssignOp::ShrAssign,
                    syntax::AssignOp::UShrAssign => AssignOp::UShrAssign,
                };
                self.alloc_expr(Expr::Assign {
                    op,
                    lhs,
                    rhs,
                    range: assign.range,
                })
            }
            syntax::Expr::Conditional(cond) => {
                let condition = self.lower_expr(&cond.condition);
                let then_expr = self.lower_expr(&cond.then_expr);
                let else_expr = self.lower_expr(&cond.else_expr);
                self.alloc_expr(Expr::Conditional {
                    condition,
                    then_expr,
                    else_expr,
                    range: cond.range,
                })
            }
            syntax::Expr::Lambda(lambda) => {
                let mut params = Vec::with_capacity(lambda.params.len());
                for param in &lambda.params {
                    self.check_cancelled();
                    let local = self.alloc_local(Local {
                        ty_text: String::new(),
                        ty_range: Span::new(param.range.start, param.range.start),
                        name: param.name.clone(),
                        name_range: param.range,
                        range: param.range,
                    });
                    params.push(LambdaParam { local });
                }

                let body = match &lambda.body {
                    syntax::LambdaBody::Expr(expr) => LambdaBody::Expr(self.lower_expr(expr)),
                    syntax::LambdaBody::Block(block) => LambdaBody::Block(self.lower_block(block)),
                };

                self.alloc_expr(Expr::Lambda {
                    params,
                    body,
                    range: lambda.range,
                })
            }
            syntax::Expr::Cast(cast) => {
                let inner = self.lower_expr(cast.expr.as_ref());
                self.alloc_expr(Expr::Cast {
                    ty_text: cast.ty.text.clone(),
                    ty_range: cast.ty.range,
                    expr: inner,
                    range: cast.range,
                })
            }
            syntax::Expr::Invalid { children, range } => {
                let mut lowered = Vec::with_capacity(children.len());
                for child in children {
                    self.check_cancelled();
                    lowered.push(self.lower_expr(child));
                }
                self.alloc_expr(Expr::Invalid {
                    children: lowered,
                    range: *range,
                })
            }
            syntax::Expr::Missing(range) => self.alloc_expr(Expr::Missing { range: *range }),
        }
    }
}

// Intentionally no helpers past this point: keep this module warning-free under `-D warnings`.
