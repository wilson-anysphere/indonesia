//! Generated file, do not edit by hand.
//!
//! To regenerate, run:
//!   cargo xtask codegen

use crate::ast::{support, AstNode};
use crate::parser::{SyntaxNode, SyntaxToken};
use crate::syntax_kind::SyntaxKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilationUnit {
    syntax: SyntaxNode,
}

impl AstNode for CompilationUnit {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::CompilationUnit
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl CompilationUnit {
    pub fn package(&self) -> Option<PackageDeclaration> {
        support::child::<PackageDeclaration>(&self.syntax)
    }

    pub fn imports(&self) -> impl Iterator<Item = ImportDeclaration> + '_ {
        support::children::<ImportDeclaration>(&self.syntax)
    }

    pub fn module_declaration(&self) -> Option<ModuleDeclaration> {
        support::child::<ModuleDeclaration>(&self.syntax)
    }

    pub fn type_declarations(&self) -> impl Iterator<Item = TypeDeclaration> + '_ {
        support::children::<TypeDeclaration>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for PackageDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PackageDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl PackageDeclaration {
    pub fn annotations(&self) -> impl Iterator<Item = Annotation> + '_ {
        support::children::<Annotation>(&self.syntax)
    }

    pub fn name(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for ImportDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ImportDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ImportDeclaration {
    pub fn name(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modifiers {
    syntax: SyntaxNode,
}

impl AstNode for Modifiers {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Modifiers
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Modifiers {
    pub fn annotations(&self) -> impl Iterator<Item = Annotation> + '_ {
        support::children::<Annotation>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    syntax: SyntaxNode,
}

impl AstNode for Annotation {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Annotation
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Annotation {
    pub fn name(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

    pub fn arguments(&self) -> Option<AnnotationElementValuePairList> {
        support::child::<AnnotationElementValuePairList>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationElementValuePairList {
    syntax: SyntaxNode,
}

impl AstNode for AnnotationElementValuePairList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::AnnotationElementValuePairList
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl AnnotationElementValuePairList {
    pub fn pairs(&self) -> impl Iterator<Item = AnnotationElementValuePair> + '_ {
        support::children::<AnnotationElementValuePair>(&self.syntax)
    }

    pub fn value(&self) -> Option<AnnotationElementValue> {
        support::child::<AnnotationElementValue>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationElementValuePair {
    syntax: SyntaxNode,
}

impl AstNode for AnnotationElementValuePair {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::AnnotationElementValuePair
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl AnnotationElementValuePair {
    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn value(&self) -> Option<AnnotationElementValue> {
        support::child::<AnnotationElementValue>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationElementValue {
    syntax: SyntaxNode,
}

impl AstNode for AnnotationElementValue {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::AnnotationElementValue
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl AnnotationElementValue {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn annotation(&self) -> Option<Annotation> {
        support::child::<Annotation>(&self.syntax)
    }

    pub fn array_initializer(&self) -> Option<AnnotationElementValueArrayInitializer> {
        support::child::<AnnotationElementValueArrayInitializer>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationElementValueArrayInitializer {
    syntax: SyntaxNode,
}

impl AstNode for AnnotationElementValueArrayInitializer {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::AnnotationElementValueArrayInitializer
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl AnnotationElementValueArrayInitializer {
    pub fn values(&self) -> impl Iterator<Item = AnnotationElementValue> + '_ {
        support::children::<AnnotationElementValue>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    syntax: SyntaxNode,
}

impl AstNode for Name {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Name
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for ClassDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ClassDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ClassDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn type_parameters(&self) -> Option<TypeParameters> {
        support::child::<TypeParameters>(&self.syntax)
    }

    pub fn extends_clause(&self) -> Option<ExtendsClause> {
        support::child::<ExtendsClause>(&self.syntax)
    }

    pub fn implements_clause(&self) -> Option<ImplementsClause> {
        support::child::<ImplementsClause>(&self.syntax)
    }

    pub fn permits_clause(&self) -> Option<PermitsClause> {
        support::child::<PermitsClause>(&self.syntax)
    }

    pub fn body(&self) -> Option<ClassBody> {
        support::child::<ClassBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for InterfaceDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::InterfaceDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl InterfaceDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn type_parameters(&self) -> Option<TypeParameters> {
        support::child::<TypeParameters>(&self.syntax)
    }

    pub fn extends_clause(&self) -> Option<ExtendsClause> {
        support::child::<ExtendsClause>(&self.syntax)
    }

    pub fn implements_clause(&self) -> Option<ImplementsClause> {
        support::child::<ImplementsClause>(&self.syntax)
    }

    pub fn permits_clause(&self) -> Option<PermitsClause> {
        support::child::<PermitsClause>(&self.syntax)
    }

    pub fn body(&self) -> Option<InterfaceBody> {
        support::child::<InterfaceBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for EnumDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::EnumDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl EnumDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn implements_clause(&self) -> Option<ImplementsClause> {
        support::child::<ImplementsClause>(&self.syntax)
    }

    pub fn permits_clause(&self) -> Option<PermitsClause> {
        support::child::<PermitsClause>(&self.syntax)
    }

    pub fn body(&self) -> Option<EnumBody> {
        support::child::<EnumBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for RecordDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::RecordDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl RecordDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn type_parameters(&self) -> Option<TypeParameters> {
        support::child::<TypeParameters>(&self.syntax)
    }

    pub fn parameter_list(&self) -> Option<ParameterList> {
        support::child::<ParameterList>(&self.syntax)
    }

    pub fn implements_clause(&self) -> Option<ImplementsClause> {
        support::child::<ImplementsClause>(&self.syntax)
    }

    pub fn permits_clause(&self) -> Option<PermitsClause> {
        support::child::<PermitsClause>(&self.syntax)
    }

    pub fn body(&self) -> Option<RecordBody> {
        support::child::<RecordBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationTypeDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for AnnotationTypeDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::AnnotationTypeDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl AnnotationTypeDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn body(&self) -> Option<AnnotationBody> {
        support::child::<AnnotationBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassBody {
    syntax: SyntaxNode,
}

impl AstNode for ClassBody {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ClassBody
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ClassBody {
    pub fn members(&self) -> impl Iterator<Item = ClassMember> + '_ {
        support::children::<ClassMember>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceBody {
    syntax: SyntaxNode,
}

impl AstNode for InterfaceBody {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::InterfaceBody
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl InterfaceBody {
    pub fn members(&self) -> impl Iterator<Item = ClassMember> + '_ {
        support::children::<ClassMember>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumBody {
    syntax: SyntaxNode,
}

impl AstNode for EnumBody {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::EnumBody
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl EnumBody {
    pub fn constants(&self) -> impl Iterator<Item = EnumConstant> + '_ {
        support::children::<EnumConstant>(&self.syntax)
    }

    pub fn members(&self) -> impl Iterator<Item = ClassMember> + '_ {
        support::children::<ClassMember>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordBody {
    syntax: SyntaxNode,
}

impl AstNode for RecordBody {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::RecordBody
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl RecordBody {
    pub fn members(&self) -> impl Iterator<Item = ClassMember> + '_ {
        support::children::<ClassMember>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationBody {
    syntax: SyntaxNode,
}

impl AstNode for AnnotationBody {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::AnnotationBody
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl AnnotationBody {
    pub fn members(&self) -> impl Iterator<Item = ClassMember> + '_ {
        support::children::<ClassMember>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumConstant {
    syntax: SyntaxNode,
}

impl AstNode for EnumConstant {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::EnumConstant
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl EnumConstant {
    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn arguments(&self) -> Option<ArgumentList> {
        support::child::<ArgumentList>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for FieldDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::FieldDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl FieldDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn type_parameters(&self) -> Option<TypeParameters> {
        support::child::<TypeParameters>(&self.syntax)
    }

    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn declarator_list(&self) -> Option<VariableDeclaratorList> {
        support::child::<VariableDeclaratorList>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for MethodDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::MethodDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl MethodDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn type_parameters(&self) -> Option<TypeParameters> {
        support::child::<TypeParameters>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn parameter_list(&self) -> Option<ParameterList> {
        support::child::<ParameterList>(&self.syntax)
    }

    pub fn default_value(&self) -> Option<DefaultValue> {
        support::child::<DefaultValue>(&self.syntax)
    }

    pub fn body(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstructorDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for ConstructorDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ConstructorDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ConstructorDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn type_parameters(&self) -> Option<TypeParameters> {
        support::child::<TypeParameters>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn parameter_list(&self) -> Option<ParameterList> {
        support::child::<ParameterList>(&self.syntax)
    }

    pub fn body(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializerBlock {
    syntax: SyntaxNode,
}

impl AstNode for InitializerBlock {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::InitializerBlock
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl InitializerBlock {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn body(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for EmptyDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::EmptyDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterList {
    syntax: SyntaxNode,
}

impl AstNode for ParameterList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ParameterList
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ParameterList {
    pub fn parameters(&self) -> impl Iterator<Item = Parameter> + '_ {
        support::children::<Parameter>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    syntax: SyntaxNode,
}

impl AstNode for Parameter {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Parameter
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Parameter {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    syntax: SyntaxNode,
}

impl AstNode for Block {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Block
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Block {
    pub fn statements(&self) -> impl Iterator<Item = Statement> + '_ {
        support::children::<Statement>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledStatement {
    syntax: SyntaxNode,
}

impl AstNode for LabeledStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LabeledStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl LabeledStatement {
    pub fn label_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn statement(&self) -> Option<Statement> {
        support::child::<Statement>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfStatement {
    syntax: SyntaxNode,
}

impl AstNode for IfStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::IfStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl IfStatement {
    pub fn condition(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn then_branch(&self) -> Option<Statement> {
        support::child::<Statement>(&self.syntax)
    }

    pub fn else_branch(&self) -> Option<Statement> {
        support::children::<Statement>(&self.syntax).nth(1)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchStatement {
    syntax: SyntaxNode,
}

impl AstNode for SwitchStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SwitchStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl SwitchStatement {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn block(&self) -> Option<SwitchBlock> {
        support::child::<SwitchBlock>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchExpression {
    syntax: SyntaxNode,
}

impl AstNode for SwitchExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SwitchExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl SwitchExpression {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn block(&self) -> Option<SwitchBlock> {
        support::child::<SwitchBlock>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchBlock {
    syntax: SyntaxNode,
}

impl AstNode for SwitchBlock {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SwitchBlock
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl SwitchBlock {
    pub fn groups(&self) -> impl Iterator<Item = SwitchGroup> + '_ {
        support::children::<SwitchGroup>(&self.syntax)
    }

    pub fn rules(&self) -> impl Iterator<Item = SwitchRule> + '_ {
        support::children::<SwitchRule>(&self.syntax)
    }

    pub fn statements(&self) -> impl Iterator<Item = Statement> + '_ {
        support::children::<Statement>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchGroup {
    syntax: SyntaxNode,
}

impl AstNode for SwitchGroup {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SwitchGroup
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl SwitchGroup {
    pub fn labels(&self) -> impl Iterator<Item = SwitchLabel> + '_ {
        support::children::<SwitchLabel>(&self.syntax)
    }

    pub fn statements(&self) -> impl Iterator<Item = Statement> + '_ {
        support::children::<Statement>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchRule {
    syntax: SyntaxNode,
}

impl AstNode for SwitchRule {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SwitchRule
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl SwitchRule {
    pub fn labels(&self) -> impl Iterator<Item = SwitchLabel> + '_ {
        support::children::<SwitchLabel>(&self.syntax)
    }

    pub fn body(&self) -> Option<SwitchRuleBody> {
        support::child::<SwitchRuleBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchLabel {
    syntax: SyntaxNode,
}

impl AstNode for SwitchLabel {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SwitchLabel
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl SwitchLabel {
    pub fn case_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::CaseKw)
    }

    pub fn default_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::DefaultKw)
    }

    pub fn colon_token(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::Colon)
    }

    pub fn arrow_token(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::Arrow)
    }

    pub fn elements(&self) -> impl Iterator<Item = CaseLabelElement> + '_ {
        support::children::<CaseLabelElement>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseLabelElement {
    syntax: SyntaxNode,
}

impl AstNode for CaseLabelElement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::CaseLabelElement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl CaseLabelElement {
    pub fn pattern(&self) -> Option<Pattern> {
        support::child::<Pattern>(&self.syntax)
    }

    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn guard(&self) -> Option<Guard> {
        support::child::<Guard>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    syntax: SyntaxNode,
}

impl AstNode for Guard {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Guard
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Guard {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    syntax: SyntaxNode,
}

impl AstNode for Pattern {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Pattern
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Pattern {
    pub fn type_pattern(&self) -> Option<TypePattern> {
        support::child::<TypePattern>(&self.syntax)
    }

    pub fn record_pattern(&self) -> Option<RecordPattern> {
        support::child::<RecordPattern>(&self.syntax)
    }

    pub fn unnamed_pattern(&self) -> Option<UnnamedPattern> {
        support::child::<UnnamedPattern>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypePattern {
    syntax: SyntaxNode,
}

impl AstNode for TypePattern {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TypePattern
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl TypePattern {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn unnamed_pattern(&self) -> Option<UnnamedPattern> {
        support::child::<UnnamedPattern>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordPattern {
    syntax: SyntaxNode,
}

impl AstNode for RecordPattern {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::RecordPattern
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl RecordPattern {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn components(&self) -> impl Iterator<Item = Pattern> + '_ {
        support::children::<Pattern>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnnamedPattern {
    syntax: SyntaxNode,
}

impl AstNode for UnnamedPattern {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::UnnamedPattern
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForStatement {
    syntax: SyntaxNode,
}

impl AstNode for ForStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ForStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ForStatement {
    pub fn header(&self) -> Option<ForHeader> {
        support::child::<ForHeader>(&self.syntax)
    }

    pub fn body(&self) -> Option<Statement> {
        support::child::<Statement>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForHeader {
    syntax: SyntaxNode,
}

impl AstNode for ForHeader {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ForHeader
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhileStatement {
    syntax: SyntaxNode,
}

impl AstNode for WhileStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::WhileStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl WhileStatement {
    pub fn condition(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn body(&self) -> Option<Statement> {
        support::child::<Statement>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoWhileStatement {
    syntax: SyntaxNode,
}

impl AstNode for DoWhileStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::DoWhileStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl DoWhileStatement {
    pub fn body(&self) -> Option<Statement> {
        support::child::<Statement>(&self.syntax)
    }

    pub fn condition(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynchronizedStatement {
    syntax: SyntaxNode,
}

impl AstNode for SynchronizedStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SynchronizedStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl SynchronizedStatement {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn body(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TryStatement {
    syntax: SyntaxNode,
}

impl AstNode for TryStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TryStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl TryStatement {
    pub fn resources(&self) -> Option<ResourceSpecification> {
        support::child::<ResourceSpecification>(&self.syntax)
    }

    pub fn body(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

    pub fn catches(&self) -> impl Iterator<Item = CatchClause> + '_ {
        support::children::<CatchClause>(&self.syntax)
    }

    pub fn finally_clause(&self) -> Option<FinallyClause> {
        support::child::<FinallyClause>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSpecification {
    syntax: SyntaxNode,
}

impl AstNode for ResourceSpecification {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ResourceSpecification
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ResourceSpecification {
    pub fn resources(&self) -> impl Iterator<Item = Resource> + '_ {
        support::children::<Resource>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    syntax: SyntaxNode,
}

impl AstNode for Resource {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Resource
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchClause {
    syntax: SyntaxNode,
}

impl AstNode for CatchClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::CatchClause
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl CatchClause {
    pub fn body(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinallyClause {
    syntax: SyntaxNode,
}

impl AstNode for FinallyClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::FinallyClause
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl FinallyClause {
    pub fn body(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertStatement {
    syntax: SyntaxNode,
}

impl AstNode for AssertStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::AssertStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl AssertStatement {
    pub fn condition(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn message(&self) -> Option<Expression> {
        support::children::<Expression>(&self.syntax).nth(1)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YieldStatement {
    syntax: SyntaxNode,
}

impl AstNode for YieldStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::YieldStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl YieldStatement {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnStatement {
    syntax: SyntaxNode,
}

impl AstNode for ReturnStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ReturnStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ReturnStatement {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThrowStatement {
    syntax: SyntaxNode,
}

impl AstNode for ThrowStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ThrowStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ThrowStatement {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakStatement {
    syntax: SyntaxNode,
}

impl AstNode for BreakStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::BreakStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl BreakStatement {
    pub fn label_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueStatement {
    syntax: SyntaxNode,
}

impl AstNode for ContinueStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ContinueStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ContinueStatement {
    pub fn label_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTypeDeclarationStatement {
    syntax: SyntaxNode,
}

impl AstNode for LocalTypeDeclarationStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LocalTypeDeclarationStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl LocalTypeDeclarationStatement {
    pub fn declaration(&self) -> Option<TypeDeclaration> {
        support::child::<TypeDeclaration>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVariableDeclarationStatement {
    syntax: SyntaxNode,
}

impl AstNode for LocalVariableDeclarationStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LocalVariableDeclarationStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl LocalVariableDeclarationStatement {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn declarator_list(&self) -> Option<VariableDeclaratorList> {
        support::child::<VariableDeclaratorList>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionStatement {
    syntax: SyntaxNode,
}

impl AstNode for ExpressionStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ExpressionStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ExpressionStatement {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyStatement {
    syntax: SyntaxNode,
}

impl AstNode for EmptyStatement {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::EmptyStatement
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableDeclaratorList {
    syntax: SyntaxNode,
}

impl AstNode for VariableDeclaratorList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::VariableDeclaratorList
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl VariableDeclaratorList {
    pub fn declarators(&self) -> impl Iterator<Item = VariableDeclarator> + '_ {
        support::children::<VariableDeclarator>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableDeclarator {
    syntax: SyntaxNode,
}

impl AstNode for VariableDeclarator {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::VariableDeclarator
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl VariableDeclarator {
    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn unnamed_pattern(&self) -> Option<UnnamedPattern> {
        support::child::<UnnamedPattern>(&self.syntax)
    }

    pub fn initializer(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Type {
    syntax: SyntaxNode,
}

impl AstNode for Type {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Type
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Type {
    pub fn annotations(&self) -> impl Iterator<Item = Annotation> + '_ {
        support::children::<Annotation>(&self.syntax)
    }

    pub fn primitive(&self) -> Option<PrimitiveType> {
        support::child::<PrimitiveType>(&self.syntax)
    }

    pub fn named(&self) -> Option<NamedType> {
        support::child::<NamedType>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimitiveType {
    syntax: SyntaxNode,
}

impl AstNode for PrimitiveType {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PrimitiveType
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedType {
    syntax: SyntaxNode,
}

impl AstNode for NamedType {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::NamedType
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl NamedType {
    pub fn type_arguments(&self) -> Option<TypeArguments> {
        support::child::<TypeArguments>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeArguments {
    syntax: SyntaxNode,
}

impl AstNode for TypeArguments {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TypeArguments
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl TypeArguments {
    pub fn arguments(&self) -> impl Iterator<Item = TypeArgument> + '_ {
        support::children::<TypeArgument>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeArgument {
    syntax: SyntaxNode,
}

impl AstNode for TypeArgument {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TypeArgument
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl TypeArgument {
    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn wildcard(&self) -> Option<WildcardType> {
        support::child::<WildcardType>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WildcardType {
    syntax: SyntaxNode,
}

impl AstNode for WildcardType {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::WildcardType
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl WildcardType {
    pub fn bound(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentList {
    syntax: SyntaxNode,
}

impl AstNode for ArgumentList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ArgumentList
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ArgumentList {
    pub fn arguments(&self) -> impl Iterator<Item = Expression> + '_ {
        support::children::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralExpression {
    syntax: SyntaxNode,
}

impl AstNode for LiteralExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LiteralExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameExpression {
    syntax: SyntaxNode,
}

impl AstNode for NameExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::NameExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThisExpression {
    syntax: SyntaxNode,
}

impl AstNode for ThisExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ThisExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperExpression {
    syntax: SyntaxNode,
}

impl AstNode for SuperExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SuperExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParenthesizedExpression {
    syntax: SyntaxNode,
}

impl AstNode for ParenthesizedExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ParenthesizedExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ParenthesizedExpression {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewExpression {
    syntax: SyntaxNode,
}

impl AstNode for NewExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::NewExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl NewExpression {
    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn arguments(&self) -> Option<ArgumentList> {
        support::child::<ArgumentList>(&self.syntax)
    }

    pub fn class_body(&self) -> Option<ClassBody> {
        support::child::<ClassBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayCreationExpression {
    syntax: SyntaxNode,
}

impl AstNode for ArrayCreationExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ArrayCreationExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ArrayCreationExpression {
    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn dim_exprs(&self) -> Option<DimExprs> {
        support::child::<DimExprs>(&self.syntax)
    }

    pub fn dims(&self) -> Option<Dims> {
        support::child::<Dims>(&self.syntax)
    }

    pub fn initializer(&self) -> Option<ArrayInitializer> {
        support::child::<ArrayInitializer>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DimExprs {
    syntax: SyntaxNode,
}

impl AstNode for DimExprs {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::DimExprs
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl DimExprs {
    pub fn dims(&self) -> impl Iterator<Item = DimExpr> + '_ {
        support::children::<DimExpr>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DimExpr {
    syntax: SyntaxNode,
}

impl AstNode for DimExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::DimExpr
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl DimExpr {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dims {
    syntax: SyntaxNode,
}

impl AstNode for Dims {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Dims
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl Dims {
    pub fn dims(&self) -> impl Iterator<Item = Dim> + '_ {
        support::children::<Dim>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dim {
    syntax: SyntaxNode,
}

impl AstNode for Dim {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::Dim
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodCallExpression {
    syntax: SyntaxNode,
}

impl AstNode for MethodCallExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::MethodCallExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl MethodCallExpression {
    pub fn callee(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn arguments(&self) -> Option<ArgumentList> {
        support::child::<ArgumentList>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldAccessExpression {
    syntax: SyntaxNode,
}

impl AstNode for FieldAccessExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::FieldAccessExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl FieldAccessExpression {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn type_arguments(&self) -> Option<TypeArguments> {
        support::child::<TypeArguments>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassLiteralExpression {
    syntax: SyntaxNode,
}

impl AstNode for ClassLiteralExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ClassLiteralExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ClassLiteralExpression {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodReferenceExpression {
    syntax: SyntaxNode,
}

impl AstNode for MethodReferenceExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::MethodReferenceExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl MethodReferenceExpression {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn type_arguments(&self) -> Option<TypeArguments> {
        support::child::<TypeArguments>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstructorReferenceExpression {
    syntax: SyntaxNode,
}

impl AstNode for ConstructorReferenceExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ConstructorReferenceExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ConstructorReferenceExpression {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn type_arguments(&self) -> Option<TypeArguments> {
        support::child::<TypeArguments>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayAccessExpression {
    syntax: SyntaxNode,
}

impl AstNode for ArrayAccessExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ArrayAccessExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ArrayAccessExpression {
    pub fn array(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn index(&self) -> Option<Expression> {
        support::children::<Expression>(&self.syntax).nth(1)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnaryExpression {
    syntax: SyntaxNode,
}

impl AstNode for UnaryExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::UnaryExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl UnaryExpression {
    pub fn operand(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryExpression {
    syntax: SyntaxNode,
}

impl AstNode for BinaryExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::BinaryExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl BinaryExpression {
    pub fn lhs(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn rhs(&self) -> Option<Expression> {
        support::children::<Expression>(&self.syntax).nth(1)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceofExpression {
    syntax: SyntaxNode,
}

impl AstNode for InstanceofExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::InstanceofExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl InstanceofExpression {
    pub fn lhs(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn pattern(&self) -> Option<Pattern> {
        support::child::<Pattern>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentExpression {
    syntax: SyntaxNode,
}

impl AstNode for AssignmentExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::AssignmentExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl AssignmentExpression {
    pub fn lhs(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn rhs(&self) -> Option<Expression> {
        support::children::<Expression>(&self.syntax).nth(1)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalExpression {
    syntax: SyntaxNode,
}

impl AstNode for ConditionalExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ConditionalExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ConditionalExpression {
    pub fn condition(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

    pub fn then_branch(&self) -> Option<Expression> {
        support::children::<Expression>(&self.syntax).nth(1)
    }

    pub fn else_branch(&self) -> Option<Expression> {
        support::children::<Expression>(&self.syntax).nth(2)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaExpression {
    syntax: SyntaxNode,
}

impl AstNode for LambdaExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LambdaExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl LambdaExpression {
    pub fn parameters(&self) -> Option<LambdaParameters> {
        support::child::<LambdaParameters>(&self.syntax)
    }

    pub fn body(&self) -> Option<LambdaBody> {
        support::child::<LambdaBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaParameters {
    syntax: SyntaxNode,
}

impl AstNode for LambdaParameters {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LambdaParameters
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl LambdaParameters {
    pub fn parameter_list(&self) -> Option<LambdaParameterList> {
        support::child::<LambdaParameterList>(&self.syntax)
    }

    pub fn parameter(&self) -> Option<LambdaParameter> {
        support::child::<LambdaParameter>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaParameterList {
    syntax: SyntaxNode,
}

impl AstNode for LambdaParameterList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LambdaParameterList
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl LambdaParameterList {
    pub fn parameters(&self) -> impl Iterator<Item = LambdaParameter> + '_ {
        support::children::<LambdaParameter>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaParameter {
    syntax: SyntaxNode,
}

impl AstNode for LambdaParameter {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LambdaParameter
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl LambdaParameter {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn unnamed_pattern(&self) -> Option<UnnamedPattern> {
        support::child::<UnnamedPattern>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaBody {
    syntax: SyntaxNode,
}

impl AstNode for LambdaBody {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LambdaBody
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl LambdaBody {
    pub fn block(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastExpression {
    syntax: SyntaxNode,
}

impl AstNode for CastExpression {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::CastExpression
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl CastExpression {
    pub fn ty(&self) -> Option<Type> {
        support::child::<Type>(&self.syntax)
    }

    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayInitializer {
    syntax: SyntaxNode,
}

impl AstNode for ArrayInitializer {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ArrayInitializer
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ArrayInitializer {
    pub fn initializers(&self) -> Option<ArrayInitializerList> {
        support::child::<ArrayInitializerList>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayInitializerList {
    syntax: SyntaxNode,
}

impl AstNode for ArrayInitializerList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ArrayInitializerList
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ArrayInitializerList {
    pub fn initializers(&self) -> impl Iterator<Item = VariableInitializer> + '_ {
        support::children::<VariableInitializer>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableInitializer {
    syntax: SyntaxNode,
}

impl AstNode for VariableInitializer {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::VariableInitializer
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl VariableInitializer {
    pub fn initializer(&self) -> Option<ArrayInitializer> {
        support::child::<ArrayInitializer>(&self.syntax)
    }

    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendsClause {
    syntax: SyntaxNode,
}

impl AstNode for ExtendsClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ExtendsClause
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ExtendsClause {
    pub fn types(&self) -> impl Iterator<Item = Type> + '_ {
        support::children::<Type>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplementsClause {
    syntax: SyntaxNode,
}

impl AstNode for ImplementsClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ImplementsClause
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ImplementsClause {
    pub fn types(&self) -> impl Iterator<Item = Type> + '_ {
        support::children::<Type>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermitsClause {
    syntax: SyntaxNode,
}

impl AstNode for PermitsClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PermitsClause
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl PermitsClause {
    pub fn types(&self) -> impl Iterator<Item = Type> + '_ {
        support::children::<Type>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameters {
    syntax: SyntaxNode,
}

impl AstNode for TypeParameters {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TypeParameters
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl TypeParameters {
    pub fn type_parameters(&self) -> impl Iterator<Item = TypeParameter> + '_ {
        support::children::<TypeParameter>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameter {
    syntax: SyntaxNode,
}

impl AstNode for TypeParameter {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TypeParameter
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl TypeParameter {
    pub fn name_token(&self) -> Option<SyntaxToken> {
        support::ident_token(&self.syntax)
    }

    pub fn bounds(&self) -> impl Iterator<Item = Type> + '_ {
        support::children::<Type>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultValue {
    syntax: SyntaxNode,
}

impl AstNode for DefaultValue {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::DefaultValue
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl DefaultValue {
    pub fn value(&self) -> Option<AnnotationElementValue> {
        support::child::<AnnotationElementValue>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionRoot {
    syntax: SyntaxNode,
}

impl AstNode for ExpressionRoot {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ExpressionRoot
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ExpressionRoot {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionFragment {
    syntax: SyntaxNode,
}

impl AstNode for ExpressionFragment {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ExpressionFragment
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ExpressionFragment {
    pub fn expression(&self) -> Option<Expression> {
        support::child::<Expression>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementFragment {
    syntax: SyntaxNode,
}

impl AstNode for StatementFragment {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::StatementFragment
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl StatementFragment {
    pub fn statement(&self) -> Option<Statement> {
        support::child::<Statement>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockFragment {
    syntax: SyntaxNode,
}

impl AstNode for BlockFragment {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::BlockFragment
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl BlockFragment {
    pub fn block(&self) -> Option<Block> {
        support::child::<Block>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassMemberFragment {
    syntax: SyntaxNode,
}

impl AstNode for ClassMemberFragment {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ClassMemberFragment
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ClassMemberFragment {
    pub fn member(&self) -> Option<ClassMember> {
        support::child::<ClassMember>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDeclaration {
    syntax: SyntaxNode,
}

impl AstNode for ModuleDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ModuleDeclaration
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ModuleDeclaration {
    pub fn modifiers(&self) -> Option<Modifiers> {
        support::child::<Modifiers>(&self.syntax)
    }

    pub fn open_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::OpenKw)
    }

    pub fn module_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::ModuleKw)
    }

    pub fn name(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

    pub fn body(&self) -> Option<ModuleBody> {
        support::child::<ModuleBody>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleBody {
    syntax: SyntaxNode,
}

impl AstNode for ModuleBody {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ModuleBody
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ModuleBody {
    pub fn directive_wrappers(&self) -> impl Iterator<Item = ModuleDirective> + '_ {
        support::children::<ModuleDirective>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDirective {
    syntax: SyntaxNode,
}

impl AstNode for ModuleDirective {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ModuleDirective
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ModuleDirective {
    pub fn directive(&self) -> Option<ModuleDirectiveKind> {
        support::child::<ModuleDirectiveKind>(&self.syntax)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiresDirective {
    syntax: SyntaxNode,
}

impl AstNode for RequiresDirective {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::RequiresDirective
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl RequiresDirective {
    pub fn requires_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::RequiresKw)
    }

    pub fn transitive_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::TransitiveKw)
    }

    pub fn static_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::StaticKw)
    }

    pub fn module(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

    pub fn semicolon(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::Semicolon)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportsDirective {
    syntax: SyntaxNode,
}

impl AstNode for ExportsDirective {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ExportsDirective
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ExportsDirective {
    pub fn exports_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::ExportsKw)
    }

    pub fn package(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

    pub fn to_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::ToKw)
    }

    pub fn semicolon(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::Semicolon)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpensDirective {
    syntax: SyntaxNode,
}

impl AstNode for OpensDirective {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::OpensDirective
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl OpensDirective {
    pub fn opens_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::OpensKw)
    }

    pub fn package(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

    pub fn to_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::ToKw)
    }

    pub fn semicolon(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::Semicolon)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsesDirective {
    syntax: SyntaxNode,
}

impl AstNode for UsesDirective {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::UsesDirective
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl UsesDirective {
    pub fn uses_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::UsesKw)
    }

    pub fn service(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

    pub fn semicolon(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::Semicolon)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidesDirective {
    syntax: SyntaxNode,
}

impl AstNode for ProvidesDirective {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ProvidesDirective
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        Self::can_cast(syntax.kind()).then_some(Self { syntax })
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.syntax
    }
}

impl ProvidesDirective {
    pub fn provides_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::ProvidesKw)
    }

    pub fn service(&self) -> Option<Name> {
        support::child::<Name>(&self.syntax)
    }

    pub fn with_kw(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::WithKw)
    }

    pub fn semicolon(&self) -> Option<SyntaxToken> {
        support::token(&self.syntax, SyntaxKind::Semicolon)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeDeclaration {
    ClassDeclaration(ClassDeclaration),
    InterfaceDeclaration(InterfaceDeclaration),
    EnumDeclaration(EnumDeclaration),
    RecordDeclaration(RecordDeclaration),
    AnnotationTypeDeclaration(AnnotationTypeDeclaration),
    EmptyDeclaration(EmptyDeclaration),
}

impl AstNode for TypeDeclaration {
    fn can_cast(kind: SyntaxKind) -> bool {
        ClassDeclaration::can_cast(kind)
            || InterfaceDeclaration::can_cast(kind)
            || EnumDeclaration::can_cast(kind)
            || RecordDeclaration::can_cast(kind)
            || AnnotationTypeDeclaration::can_cast(kind)
            || EmptyDeclaration::can_cast(kind)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        let kind = syntax.kind();
        if !Self::can_cast(kind) {
            return None;
        }

        if let Some(it) = ClassDeclaration::cast(syntax.clone()) { return Some(Self::ClassDeclaration(it)); }
        if let Some(it) = InterfaceDeclaration::cast(syntax.clone()) { return Some(Self::InterfaceDeclaration(it)); }
        if let Some(it) = EnumDeclaration::cast(syntax.clone()) { return Some(Self::EnumDeclaration(it)); }
        if let Some(it) = RecordDeclaration::cast(syntax.clone()) { return Some(Self::RecordDeclaration(it)); }
        if let Some(it) = AnnotationTypeDeclaration::cast(syntax.clone()) { return Some(Self::AnnotationTypeDeclaration(it)); }
        if let Some(it) = EmptyDeclaration::cast(syntax.clone()) { return Some(Self::EmptyDeclaration(it)); }

        None
    }

    fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::ClassDeclaration(it) => it.syntax(),
            Self::InterfaceDeclaration(it) => it.syntax(),
            Self::EnumDeclaration(it) => it.syntax(),
            Self::RecordDeclaration(it) => it.syntax(),
            Self::AnnotationTypeDeclaration(it) => it.syntax(),
            Self::EmptyDeclaration(it) => it.syntax(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClassMember {
    FieldDeclaration(FieldDeclaration),
    MethodDeclaration(MethodDeclaration),
    ConstructorDeclaration(ConstructorDeclaration),
    InitializerBlock(InitializerBlock),
    EmptyDeclaration(EmptyDeclaration),
    ClassDeclaration(ClassDeclaration),
    InterfaceDeclaration(InterfaceDeclaration),
    EnumDeclaration(EnumDeclaration),
    RecordDeclaration(RecordDeclaration),
    AnnotationTypeDeclaration(AnnotationTypeDeclaration),
}

impl AstNode for ClassMember {
    fn can_cast(kind: SyntaxKind) -> bool {
        FieldDeclaration::can_cast(kind)
            || MethodDeclaration::can_cast(kind)
            || ConstructorDeclaration::can_cast(kind)
            || InitializerBlock::can_cast(kind)
            || EmptyDeclaration::can_cast(kind)
            || ClassDeclaration::can_cast(kind)
            || InterfaceDeclaration::can_cast(kind)
            || EnumDeclaration::can_cast(kind)
            || RecordDeclaration::can_cast(kind)
            || AnnotationTypeDeclaration::can_cast(kind)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        let kind = syntax.kind();
        if !Self::can_cast(kind) {
            return None;
        }

        if let Some(it) = FieldDeclaration::cast(syntax.clone()) { return Some(Self::FieldDeclaration(it)); }
        if let Some(it) = MethodDeclaration::cast(syntax.clone()) { return Some(Self::MethodDeclaration(it)); }
        if let Some(it) = ConstructorDeclaration::cast(syntax.clone()) { return Some(Self::ConstructorDeclaration(it)); }
        if let Some(it) = InitializerBlock::cast(syntax.clone()) { return Some(Self::InitializerBlock(it)); }
        if let Some(it) = EmptyDeclaration::cast(syntax.clone()) { return Some(Self::EmptyDeclaration(it)); }
        if let Some(it) = ClassDeclaration::cast(syntax.clone()) { return Some(Self::ClassDeclaration(it)); }
        if let Some(it) = InterfaceDeclaration::cast(syntax.clone()) { return Some(Self::InterfaceDeclaration(it)); }
        if let Some(it) = EnumDeclaration::cast(syntax.clone()) { return Some(Self::EnumDeclaration(it)); }
        if let Some(it) = RecordDeclaration::cast(syntax.clone()) { return Some(Self::RecordDeclaration(it)); }
        if let Some(it) = AnnotationTypeDeclaration::cast(syntax.clone()) { return Some(Self::AnnotationTypeDeclaration(it)); }

        None
    }

    fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::FieldDeclaration(it) => it.syntax(),
            Self::MethodDeclaration(it) => it.syntax(),
            Self::ConstructorDeclaration(it) => it.syntax(),
            Self::InitializerBlock(it) => it.syntax(),
            Self::EmptyDeclaration(it) => it.syntax(),
            Self::ClassDeclaration(it) => it.syntax(),
            Self::InterfaceDeclaration(it) => it.syntax(),
            Self::EnumDeclaration(it) => it.syntax(),
            Self::RecordDeclaration(it) => it.syntax(),
            Self::AnnotationTypeDeclaration(it) => it.syntax(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Statement {
    Block(Block),
    LabeledStatement(LabeledStatement),
    IfStatement(IfStatement),
    SwitchStatement(SwitchStatement),
    YieldStatement(YieldStatement),
    ForStatement(ForStatement),
    WhileStatement(WhileStatement),
    DoWhileStatement(DoWhileStatement),
    SynchronizedStatement(SynchronizedStatement),
    TryStatement(TryStatement),
    AssertStatement(AssertStatement),
    ReturnStatement(ReturnStatement),
    ThrowStatement(ThrowStatement),
    BreakStatement(BreakStatement),
    ContinueStatement(ContinueStatement),
    LocalTypeDeclarationStatement(LocalTypeDeclarationStatement),
    LocalVariableDeclarationStatement(LocalVariableDeclarationStatement),
    ExpressionStatement(ExpressionStatement),
    EmptyStatement(EmptyStatement),
}

impl AstNode for Statement {
    fn can_cast(kind: SyntaxKind) -> bool {
        Block::can_cast(kind)
            || LabeledStatement::can_cast(kind)
            || IfStatement::can_cast(kind)
            || SwitchStatement::can_cast(kind)
            || YieldStatement::can_cast(kind)
            || ForStatement::can_cast(kind)
            || WhileStatement::can_cast(kind)
            || DoWhileStatement::can_cast(kind)
            || SynchronizedStatement::can_cast(kind)
            || TryStatement::can_cast(kind)
            || AssertStatement::can_cast(kind)
            || ReturnStatement::can_cast(kind)
            || ThrowStatement::can_cast(kind)
            || BreakStatement::can_cast(kind)
            || ContinueStatement::can_cast(kind)
            || LocalTypeDeclarationStatement::can_cast(kind)
            || LocalVariableDeclarationStatement::can_cast(kind)
            || ExpressionStatement::can_cast(kind)
            || EmptyStatement::can_cast(kind)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        let kind = syntax.kind();
        if !Self::can_cast(kind) {
            return None;
        }

        if let Some(it) = Block::cast(syntax.clone()) { return Some(Self::Block(it)); }
        if let Some(it) = LabeledStatement::cast(syntax.clone()) { return Some(Self::LabeledStatement(it)); }
        if let Some(it) = IfStatement::cast(syntax.clone()) { return Some(Self::IfStatement(it)); }
        if let Some(it) = SwitchStatement::cast(syntax.clone()) { return Some(Self::SwitchStatement(it)); }
        if let Some(it) = YieldStatement::cast(syntax.clone()) { return Some(Self::YieldStatement(it)); }
        if let Some(it) = ForStatement::cast(syntax.clone()) { return Some(Self::ForStatement(it)); }
        if let Some(it) = WhileStatement::cast(syntax.clone()) { return Some(Self::WhileStatement(it)); }
        if let Some(it) = DoWhileStatement::cast(syntax.clone()) { return Some(Self::DoWhileStatement(it)); }
        if let Some(it) = SynchronizedStatement::cast(syntax.clone()) { return Some(Self::SynchronizedStatement(it)); }
        if let Some(it) = TryStatement::cast(syntax.clone()) { return Some(Self::TryStatement(it)); }
        if let Some(it) = AssertStatement::cast(syntax.clone()) { return Some(Self::AssertStatement(it)); }
        if let Some(it) = ReturnStatement::cast(syntax.clone()) { return Some(Self::ReturnStatement(it)); }
        if let Some(it) = ThrowStatement::cast(syntax.clone()) { return Some(Self::ThrowStatement(it)); }
        if let Some(it) = BreakStatement::cast(syntax.clone()) { return Some(Self::BreakStatement(it)); }
        if let Some(it) = ContinueStatement::cast(syntax.clone()) { return Some(Self::ContinueStatement(it)); }
        if let Some(it) = LocalTypeDeclarationStatement::cast(syntax.clone()) { return Some(Self::LocalTypeDeclarationStatement(it)); }
        if let Some(it) = LocalVariableDeclarationStatement::cast(syntax.clone()) { return Some(Self::LocalVariableDeclarationStatement(it)); }
        if let Some(it) = ExpressionStatement::cast(syntax.clone()) { return Some(Self::ExpressionStatement(it)); }
        if let Some(it) = EmptyStatement::cast(syntax.clone()) { return Some(Self::EmptyStatement(it)); }

        None
    }

    fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::Block(it) => it.syntax(),
            Self::LabeledStatement(it) => it.syntax(),
            Self::IfStatement(it) => it.syntax(),
            Self::SwitchStatement(it) => it.syntax(),
            Self::YieldStatement(it) => it.syntax(),
            Self::ForStatement(it) => it.syntax(),
            Self::WhileStatement(it) => it.syntax(),
            Self::DoWhileStatement(it) => it.syntax(),
            Self::SynchronizedStatement(it) => it.syntax(),
            Self::TryStatement(it) => it.syntax(),
            Self::AssertStatement(it) => it.syntax(),
            Self::ReturnStatement(it) => it.syntax(),
            Self::ThrowStatement(it) => it.syntax(),
            Self::BreakStatement(it) => it.syntax(),
            Self::ContinueStatement(it) => it.syntax(),
            Self::LocalTypeDeclarationStatement(it) => it.syntax(),
            Self::LocalVariableDeclarationStatement(it) => it.syntax(),
            Self::ExpressionStatement(it) => it.syntax(),
            Self::EmptyStatement(it) => it.syntax(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Expression {
    LiteralExpression(LiteralExpression),
    NameExpression(NameExpression),
    ThisExpression(ThisExpression),
    SuperExpression(SuperExpression),
    ParenthesizedExpression(ParenthesizedExpression),
    NewExpression(NewExpression),
    ArrayCreationExpression(ArrayCreationExpression),
    MethodCallExpression(MethodCallExpression),
    FieldAccessExpression(FieldAccessExpression),
    ArrayAccessExpression(ArrayAccessExpression),
    ClassLiteralExpression(ClassLiteralExpression),
    MethodReferenceExpression(MethodReferenceExpression),
    ConstructorReferenceExpression(ConstructorReferenceExpression),
    UnaryExpression(UnaryExpression),
    BinaryExpression(BinaryExpression),
    InstanceofExpression(InstanceofExpression),
    AssignmentExpression(AssignmentExpression),
    ConditionalExpression(ConditionalExpression),
    SwitchExpression(SwitchExpression),
    LambdaExpression(LambdaExpression),
    CastExpression(CastExpression),
    ArrayInitializer(ArrayInitializer),
}

impl AstNode for Expression {
    fn can_cast(kind: SyntaxKind) -> bool {
        LiteralExpression::can_cast(kind)
            || NameExpression::can_cast(kind)
            || ThisExpression::can_cast(kind)
            || SuperExpression::can_cast(kind)
            || ParenthesizedExpression::can_cast(kind)
            || NewExpression::can_cast(kind)
            || ArrayCreationExpression::can_cast(kind)
            || MethodCallExpression::can_cast(kind)
            || FieldAccessExpression::can_cast(kind)
            || ArrayAccessExpression::can_cast(kind)
            || ClassLiteralExpression::can_cast(kind)
            || MethodReferenceExpression::can_cast(kind)
            || ConstructorReferenceExpression::can_cast(kind)
            || UnaryExpression::can_cast(kind)
            || BinaryExpression::can_cast(kind)
            || InstanceofExpression::can_cast(kind)
            || AssignmentExpression::can_cast(kind)
            || ConditionalExpression::can_cast(kind)
            || SwitchExpression::can_cast(kind)
            || LambdaExpression::can_cast(kind)
            || CastExpression::can_cast(kind)
            || ArrayInitializer::can_cast(kind)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        let kind = syntax.kind();
        if !Self::can_cast(kind) {
            return None;
        }

        if let Some(it) = LiteralExpression::cast(syntax.clone()) { return Some(Self::LiteralExpression(it)); }
        if let Some(it) = NameExpression::cast(syntax.clone()) { return Some(Self::NameExpression(it)); }
        if let Some(it) = ThisExpression::cast(syntax.clone()) { return Some(Self::ThisExpression(it)); }
        if let Some(it) = SuperExpression::cast(syntax.clone()) { return Some(Self::SuperExpression(it)); }
        if let Some(it) = ParenthesizedExpression::cast(syntax.clone()) { return Some(Self::ParenthesizedExpression(it)); }
        if let Some(it) = NewExpression::cast(syntax.clone()) { return Some(Self::NewExpression(it)); }
        if let Some(it) = ArrayCreationExpression::cast(syntax.clone()) { return Some(Self::ArrayCreationExpression(it)); }
        if let Some(it) = MethodCallExpression::cast(syntax.clone()) { return Some(Self::MethodCallExpression(it)); }
        if let Some(it) = FieldAccessExpression::cast(syntax.clone()) { return Some(Self::FieldAccessExpression(it)); }
        if let Some(it) = ArrayAccessExpression::cast(syntax.clone()) { return Some(Self::ArrayAccessExpression(it)); }
        if let Some(it) = ClassLiteralExpression::cast(syntax.clone()) { return Some(Self::ClassLiteralExpression(it)); }
        if let Some(it) = MethodReferenceExpression::cast(syntax.clone()) { return Some(Self::MethodReferenceExpression(it)); }
        if let Some(it) = ConstructorReferenceExpression::cast(syntax.clone()) { return Some(Self::ConstructorReferenceExpression(it)); }
        if let Some(it) = UnaryExpression::cast(syntax.clone()) { return Some(Self::UnaryExpression(it)); }
        if let Some(it) = BinaryExpression::cast(syntax.clone()) { return Some(Self::BinaryExpression(it)); }
        if let Some(it) = InstanceofExpression::cast(syntax.clone()) { return Some(Self::InstanceofExpression(it)); }
        if let Some(it) = AssignmentExpression::cast(syntax.clone()) { return Some(Self::AssignmentExpression(it)); }
        if let Some(it) = ConditionalExpression::cast(syntax.clone()) { return Some(Self::ConditionalExpression(it)); }
        if let Some(it) = SwitchExpression::cast(syntax.clone()) { return Some(Self::SwitchExpression(it)); }
        if let Some(it) = LambdaExpression::cast(syntax.clone()) { return Some(Self::LambdaExpression(it)); }
        if let Some(it) = CastExpression::cast(syntax.clone()) { return Some(Self::CastExpression(it)); }
        if let Some(it) = ArrayInitializer::cast(syntax.clone()) { return Some(Self::ArrayInitializer(it)); }

        None
    }

    fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::LiteralExpression(it) => it.syntax(),
            Self::NameExpression(it) => it.syntax(),
            Self::ThisExpression(it) => it.syntax(),
            Self::SuperExpression(it) => it.syntax(),
            Self::ParenthesizedExpression(it) => it.syntax(),
            Self::NewExpression(it) => it.syntax(),
            Self::ArrayCreationExpression(it) => it.syntax(),
            Self::MethodCallExpression(it) => it.syntax(),
            Self::FieldAccessExpression(it) => it.syntax(),
            Self::ArrayAccessExpression(it) => it.syntax(),
            Self::ClassLiteralExpression(it) => it.syntax(),
            Self::MethodReferenceExpression(it) => it.syntax(),
            Self::ConstructorReferenceExpression(it) => it.syntax(),
            Self::UnaryExpression(it) => it.syntax(),
            Self::BinaryExpression(it) => it.syntax(),
            Self::InstanceofExpression(it) => it.syntax(),
            Self::AssignmentExpression(it) => it.syntax(),
            Self::ConditionalExpression(it) => it.syntax(),
            Self::SwitchExpression(it) => it.syntax(),
            Self::LambdaExpression(it) => it.syntax(),
            Self::CastExpression(it) => it.syntax(),
            Self::ArrayInitializer(it) => it.syntax(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SwitchRuleBody {
    Block(Block),
    Statement(Statement),
    Expression(Expression),
}

impl AstNode for SwitchRuleBody {
    fn can_cast(kind: SyntaxKind) -> bool {
        Block::can_cast(kind)
            || Statement::can_cast(kind)
            || Expression::can_cast(kind)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        let kind = syntax.kind();
        if !Self::can_cast(kind) {
            return None;
        }

        if let Some(it) = Block::cast(syntax.clone()) { return Some(Self::Block(it)); }
        if let Some(it) = Statement::cast(syntax.clone()) { return Some(Self::Statement(it)); }
        if let Some(it) = Expression::cast(syntax.clone()) { return Some(Self::Expression(it)); }

        None
    }

    fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::Block(it) => it.syntax(),
            Self::Statement(it) => it.syntax(),
            Self::Expression(it) => it.syntax(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModuleDirectiveKind {
    RequiresDirective(RequiresDirective),
    ExportsDirective(ExportsDirective),
    OpensDirective(OpensDirective),
    UsesDirective(UsesDirective),
    ProvidesDirective(ProvidesDirective),
}

impl AstNode for ModuleDirectiveKind {
    fn can_cast(kind: SyntaxKind) -> bool {
        RequiresDirective::can_cast(kind)
            || ExportsDirective::can_cast(kind)
            || OpensDirective::can_cast(kind)
            || UsesDirective::can_cast(kind)
            || ProvidesDirective::can_cast(kind)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        let kind = syntax.kind();
        if !Self::can_cast(kind) {
            return None;
        }

        if let Some(it) = RequiresDirective::cast(syntax.clone()) { return Some(Self::RequiresDirective(it)); }
        if let Some(it) = ExportsDirective::cast(syntax.clone()) { return Some(Self::ExportsDirective(it)); }
        if let Some(it) = OpensDirective::cast(syntax.clone()) { return Some(Self::OpensDirective(it)); }
        if let Some(it) = UsesDirective::cast(syntax.clone()) { return Some(Self::UsesDirective(it)); }
        if let Some(it) = ProvidesDirective::cast(syntax.clone()) { return Some(Self::ProvidesDirective(it)); }

        None
    }

    fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::RequiresDirective(it) => it.syntax(),
            Self::ExportsDirective(it) => it.syntax(),
            Self::OpensDirective(it) => it.syntax(),
            Self::UsesDirective(it) => it.syntax(),
            Self::ProvidesDirective(it) => it.syntax(),
        }
    }
}

