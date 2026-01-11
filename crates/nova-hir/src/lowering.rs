use crate::ast_id::{span_to_text_range, AstId, AstIdMap};
use crate::hir::{
    Arena, BinaryOp, Body, Expr, ExprId, LiteralKind, Local, LocalId, Stmt, StmtId,
};
use crate::ids::{
    AnnotationId, ClassId, ConstructorId, EnumId, FieldId, InitializerId, InterfaceId, MethodId,
    RecordId,
};
use crate::item_tree::{
    Annotation, Class, Constructor, Enum, Field, Import, Initializer, Interface, Item, ItemTree,
    Member, Method, PackageDecl, Param, Record,
};
use nova_syntax::java::ast as syntax;
use nova_syntax::{JavaParseResult, SyntaxKind};
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
            name: pkg.name.clone(),
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

        for decl in &unit.types {
            self.check_cancelled();
            let item = self.lower_type_decl(decl);
            self.tree.items.push(item);
        }
    }

    fn lower_type_decl(&mut self, decl: &syntax::TypeDecl) -> Item {
        self.check_cancelled();
        match decl {
            syntax::TypeDecl::Class(class) => {
                let members = self.lower_members(&class.members);
                let ast_id = self.ast_id_for_name(SyntaxKind::ClassDeclaration, class.name_range);
                let id = ClassId::new(self.file, ast_id);
                self.tree.classes.insert(
                    ast_id,
                    Class {
                        name: class.name.clone(),
                        name_range: class.name_range,
                        range: class.range,
                        body_range: class.body_range,
                        members,
                    },
                );
                Item::Class(id)
            }
            syntax::TypeDecl::Interface(interface) => {
                let members = self.lower_members(&interface.members);
                let ast_id =
                    self.ast_id_for_name(SyntaxKind::InterfaceDeclaration, interface.name_range);
                let id = InterfaceId::new(self.file, ast_id);
                self.tree.interfaces.insert(
                    ast_id,
                    Interface {
                        name: interface.name.clone(),
                        name_range: interface.name_range,
                        range: interface.range,
                        body_range: interface.body_range,
                        members,
                    },
                );
                Item::Interface(id)
            }
            syntax::TypeDecl::Enum(enm) => {
                let members = self.lower_members(&enm.members);
                let ast_id = self.ast_id_for_name(SyntaxKind::EnumDeclaration, enm.name_range);
                let id = EnumId::new(self.file, ast_id);
                self.tree.enums.insert(
                    ast_id,
                    Enum {
                        name: enm.name.clone(),
                        name_range: enm.name_range,
                        range: enm.range,
                        body_range: enm.body_range,
                        members,
                    },
                );
                Item::Enum(id)
            }
            syntax::TypeDecl::Record(record) => {
                let members = self.lower_members(&record.members);
                let ast_id =
                    self.ast_id_for_name(SyntaxKind::RecordDeclaration, record.name_range);
                let id = RecordId::new(self.file, ast_id);
                self.tree.records.insert(
                    ast_id,
                    Record {
                        name: record.name.clone(),
                        name_range: record.name_range,
                        range: record.range,
                        body_range: record.body_range,
                        members,
                    },
                );
                Item::Record(id)
            }
            syntax::TypeDecl::Annotation(annotation) => {
                let members = self.lower_members(&annotation.members);
                let ast_id = self.ast_id_for_name(
                    SyntaxKind::AnnotationTypeDeclaration,
                    annotation.name_range,
                );
                let id = AnnotationId::new(self.file, ast_id);
                self.tree.annotations.insert(
                    ast_id,
                    Annotation {
                        name: annotation.name.clone(),
                        name_range: annotation.name_range,
                        range: annotation.range,
                        body_range: annotation.body_range,
                        members,
                    },
                );
                Item::Annotation(id)
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
                let ast_id = self.ast_id_for_name(SyntaxKind::FieldDeclaration, field.name_range);
                let id = FieldId::new(self.file, ast_id);
                self.tree.fields.insert(
                    ast_id,
                    Field {
                        ty: field.ty.text.clone(),
                        name: field.name.clone(),
                        range: field.range,
                        name_range: field.name_range,
                    },
                );
                Some(Member::Field(id))
            }
            syntax::MemberDecl::Method(method) => {
                let ast_id =
                    self.ast_id_for_name(SyntaxKind::MethodDeclaration, method.name_range);
                let id = MethodId::new(self.file, ast_id);
                let params = {
                    let mut params = Vec::with_capacity(method.params.len());
                    for param in &method.params {
                        self.check_cancelled();
                        params.push(lower_param(param));
                    }
                    params
                };
                let body = method
                    .body
                    .as_ref()
                    .map(|block| self.ast_id_for_range(SyntaxKind::Block, block.range));
                self.tree.methods.insert(
                    ast_id,
                    Method {
                        return_ty: method.return_ty.text.clone(),
                        name: method.name.clone(),
                        range: method.range,
                        name_range: method.name_range,
                        params,
                        body,
                    },
                );
                Some(Member::Method(id))
            }
            syntax::MemberDecl::Constructor(cons) => {
                let ast_id =
                    self.ast_id_for_name(SyntaxKind::ConstructorDeclaration, cons.name_range);
                let id = ConstructorId::new(self.file, ast_id);
                let params = {
                    let mut params = Vec::with_capacity(cons.params.len());
                    for param in &cons.params {
                        self.check_cancelled();
                        params.push(lower_param(param));
                    }
                    params
                };
                let body = self.ast_id_for_range(SyntaxKind::Block, cons.body.range);
                self.tree.constructors.insert(
                    ast_id,
                    Constructor {
                        name: cons.name.clone(),
                        range: cons.range,
                        name_range: cons.name_range,
                        params,
                        body,
                    },
                );
                Some(Member::Constructor(id))
            }
            syntax::MemberDecl::Initializer(init) => {
                let ast_id = self.ast_id_for_range(SyntaxKind::InitializerBlock, init.range);
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
            syntax::MemberDecl::Type(decl) => Some(Member::Type(self.lower_type_decl(decl))),
        }
    }

    fn ast_id_for_range(&self, kind: SyntaxKind, range: Span) -> AstId {
        let text_range = span_to_text_range(range);
        if let Some(id) = self.ast_id_map.ast_id_for_ptr(kind, text_range) {
            return id;
        }

        let offset = u32::try_from(range.start).expect("range start does not fit in u32");
        self.ast_id_for_offset(kind, offset)
            .unwrap_or_else(|| panic!("missing AstId for {kind:?} at {text_range:?}"))
    }

    fn ast_id_for_name(&self, kind: SyntaxKind, name_range: Span) -> AstId {
        let offset = u32::try_from(name_range.start).expect("name start does not fit in u32");
        self.ast_id_for_offset(kind, offset)
            .unwrap_or_else(|| panic!("missing AstId for {kind:?} at offset {offset}"))
    }

    fn ast_id_for_offset(&self, kind: SyntaxKind, offset: u32) -> Option<AstId> {
        let token = self.parse.token_at_offset(offset).right_biased()?;
        let node = token.parent()?.ancestors().find(|ancestor| ancestor.kind() == kind)?;
        self.ast_id_map.ast_id(&node)
    }
}

fn lower_param(param: &syntax::ParamDecl) -> Param {
    Param {
        ty: param.ty.text.clone(),
        name: param.name.clone(),
        range: param.range,
        name_range: param.name_range,
    }
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
            syntax::Expr::StringLiteral(lit) => self.alloc_expr(Expr::Literal {
                kind: LiteralKind::String,
                value: lit.value.clone(),
                range: lit.range,
            }),
            syntax::Expr::FieldAccess(access) => {
                let receiver = self.lower_expr(&access.receiver);
                self.alloc_expr(Expr::FieldAccess {
                    receiver,
                    name: access.name.clone(),
                    name_range: access.name_range,
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
                self.alloc_expr(Expr::Call {
                    callee,
                    args,
                    range: call.range,
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
                };
                self.alloc_expr(Expr::Binary {
                    op,
                    lhs,
                    rhs,
                    range: binary.range,
                })
            }
            syntax::Expr::Missing(range) => self.alloc_expr(Expr::Missing { range: *range }),
        }
    }
}

#[must_use]
pub(crate) fn slice_range(text: &str, range: Span) -> &str {
    &text[range.start..range.end]
}
