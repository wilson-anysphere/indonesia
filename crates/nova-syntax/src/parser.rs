use std::collections::VecDeque;

use rowan::{GreenNode, GreenNodeBuilder};
#[cfg(test)]
use rowan::NodeOrToken;
use text_size::TextSize;

use crate::lexer::{lex, Token};
use crate::syntax_kind::{JavaLanguage, SyntaxKind};
use crate::{ParseError, TextRange};

pub type SyntaxNode = rowan::SyntaxNode<JavaLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<JavaLanguage>;
pub type SyntaxElement = rowan::SyntaxElement<JavaLanguage>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaParseResult {
    pub green: GreenNode,
    pub errors: Vec<ParseError>,
}

impl JavaParseResult {
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    pub fn token_at_offset(&self, offset: u32) -> rowan::TokenAtOffset<SyntaxToken> {
        self.syntax().token_at_offset(TextSize::from(offset))
    }

    pub fn covering_element(&self, range: TextRange) -> SyntaxElement {
        self.syntax().covering_element(text_size::TextRange::new(
            TextSize::from(range.start),
            TextSize::from(range.end),
        ))
    }
}

pub fn parse_java(input: &str) -> JavaParseResult {
    Parser::new(input).parse()
}

struct Parser<'a> {
    input: &'a str,
    tokens: VecDeque<Token>,
    builder: GreenNodeBuilder<'static>,
    errors: Vec<ParseError>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            tokens: VecDeque::from(lex(input)),
            builder: GreenNodeBuilder::new(),
            errors: Vec::new(),
        }
    }

    fn parse(mut self) -> JavaParseResult {
        self.builder.start_node(SyntaxKind::CompilationUnit.into());
        self.eat_trivia();

        if self.at(SyntaxKind::PackageKw) {
            self.parse_package_decl();
        }

        while self.at(SyntaxKind::ImportKw) {
            self.parse_import_decl();
        }

        while !self.at(SyntaxKind::Eof) {
            if self.at_type_decl_start() {
                self.parse_type_declaration();
            } else {
                self.recover_top_level();
            }
        }

        self.eat_trivia();
        self.expect(SyntaxKind::Eof, "expected end of file");
        self.builder.finish_node();

        JavaParseResult {
            green: self.builder.finish(),
            errors: self.errors,
        }
    }

    fn parse_package_decl(&mut self) {
        self.builder
            .start_node(SyntaxKind::PackageDeclaration.into());
        self.expect(SyntaxKind::PackageKw, "expected `package`");
        self.parse_name();
        self.expect(SyntaxKind::Semicolon, "expected `;` after package declaration");
        self.builder.finish_node();
    }

    fn parse_import_decl(&mut self) {
        self.builder
            .start_node(SyntaxKind::ImportDeclaration.into());
        self.expect(SyntaxKind::ImportKw, "expected `import`");
        // `import static` and wildcard imports are common; keep it permissive.
        if self.at(SyntaxKind::StaticKw) {
            self.bump();
        }
        self.parse_name();
        if self.at(SyntaxKind::Dot) && self.nth(1) == Some(SyntaxKind::Star) {
            self.bump(); // .
            self.bump(); // *
        }
        self.expect(SyntaxKind::Semicolon, "expected `;` after import declaration");
        self.builder.finish_node();
    }

    fn parse_type_declaration(&mut self) {
        let checkpoint = self.builder.checkpoint();
        self.parse_modifiers();
        self.parse_type_declaration_inner(checkpoint);
    }

    fn parse_type_declaration_inner(&mut self, checkpoint: rowan::Checkpoint) {
        match self.current() {
            SyntaxKind::At if self.nth(1) == Some(SyntaxKind::InterfaceKw) => {
                self.parse_annotation_type_decl(checkpoint)
            }
            SyntaxKind::ClassKw => {
                self.parse_class_decl(checkpoint, SyntaxKind::ClassDeclaration, SyntaxKind::ClassBody)
            }
            SyntaxKind::InterfaceKw => self.parse_class_decl(
                checkpoint,
                SyntaxKind::InterfaceDeclaration,
                SyntaxKind::InterfaceBody,
            ),
            SyntaxKind::EnumKw => self.parse_enum_decl(checkpoint),
            SyntaxKind::RecordKw => self.parse_record_decl(checkpoint),
            SyntaxKind::Semicolon => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::EmptyDeclaration.into());
                self.bump();
                self.builder.finish_node();
            }
            _ => {
                self.builder.start_node_at(checkpoint, SyntaxKind::Error.into());
                self.error_here("expected type declaration");
                self.recover_to(&[
                    SyntaxKind::PackageKw,
                    SyntaxKind::ImportKw,
                    SyntaxKind::ClassKw,
                    SyntaxKind::InterfaceKw,
                    SyntaxKind::EnumKw,
                    SyntaxKind::RecordKw,
                    SyntaxKind::Eof,
                ]);
                self.builder.finish_node();
            }
        }
    }

    fn parse_annotation_type_decl(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::AnnotationTypeDeclaration.into());
        self.expect(SyntaxKind::At, "expected `@`");
        self.expect(SyntaxKind::InterfaceKw, "expected `interface` after `@`");
        self.expect_ident_like("expected annotation type name");
        self.parse_class_body(SyntaxKind::AnnotationBody);
        self.builder.finish_node();
    }

    fn parse_class_decl(
        &mut self,
        checkpoint: rowan::Checkpoint,
        decl_kind: SyntaxKind,
        body_kind: SyntaxKind,
    ) {
        self.builder.start_node_at(checkpoint, decl_kind.into());
        // `class`/`interface` keyword already in current()
        self.bump();
        self.expect_ident_like("expected name");

        if self.at(SyntaxKind::ExtendsKw) {
            self.bump();
            self.parse_type();
        }
        if self.at(SyntaxKind::ImplementsKw) {
            self.bump();
            self.parse_type();
            while self.at(SyntaxKind::Comma) {
                self.bump();
                self.parse_type();
            }
        }

        self.parse_class_body(body_kind);
        self.builder.finish_node();
    }

    fn parse_enum_decl(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::EnumDeclaration.into());
        self.expect(SyntaxKind::EnumKw, "expected `enum`");
        self.expect_ident_like("expected enum name");
        if self.at(SyntaxKind::ImplementsKw) {
            self.bump();
            self.parse_type();
            while self.at(SyntaxKind::Comma) {
                self.bump();
                self.parse_type();
            }
        }

        self.builder.start_node(SyntaxKind::EnumBody.into());
        self.expect(SyntaxKind::LBrace, "expected `{` for enum body");
        // Enum constants (very permissive).
        while !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            if self.at_ident_like() {
                self.builder.start_node(SyntaxKind::EnumConstant.into());
                self.bump();
                // Optional arguments.
                if self.at(SyntaxKind::LParen) {
                    self.parse_argument_list();
                }
                self.builder.finish_node();
                if self.at(SyntaxKind::Comma) {
                    self.bump();
                    continue;
                }
                if self.at(SyntaxKind::Semicolon) {
                    break;
                }
            } else {
                break;
            }
        }
        if self.at(SyntaxKind::Semicolon) {
            self.bump();
            // Class body declarations after constants.
            while !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
                self.parse_class_member();
            }
        }
        self.expect(SyntaxKind::RBrace, "expected `}` to close enum body");
        self.builder.finish_node(); // EnumBody
        self.builder.finish_node(); // EnumDeclaration
    }

    fn parse_record_decl(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::RecordDeclaration.into());
        self.expect(SyntaxKind::RecordKw, "expected `record`");
        self.expect_ident_like("expected record name");
        // Header.
        if self.at(SyntaxKind::LParen) {
            self.parse_parameter_list();
        } else {
            self.error_here("expected record header");
        }
        if self.at(SyntaxKind::ImplementsKw) {
            self.bump();
            self.parse_type();
            while self.at(SyntaxKind::Comma) {
                self.bump();
                self.parse_type();
            }
        }
        self.parse_class_body(SyntaxKind::RecordBody);
        self.builder.finish_node();
    }

    fn parse_class_body(&mut self, body_kind: SyntaxKind) {
        self.builder.start_node(body_kind.into());
        self.expect(SyntaxKind::LBrace, "expected `{`");
        while !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            self.parse_class_member();
        }
        self.expect(SyntaxKind::RBrace, "expected `}`");
        self.builder.finish_node();
    }

    fn parse_class_member(&mut self) {
        let checkpoint = self.builder.checkpoint();
        self.parse_modifiers();

        // Initializer blocks.
        if self.at(SyntaxKind::LBrace) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::InitializerBlock.into());
            self.parse_block();
            self.builder.finish_node();
            return;
        }

        // Empty declaration.
        if self.at(SyntaxKind::Semicolon) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::EmptyDeclaration.into());
            self.bump();
            self.builder.finish_node();
            return;
        }

        // Nested types.
        if matches!(
            self.current(),
            SyntaxKind::ClassKw | SyntaxKind::InterfaceKw | SyntaxKind::EnumKw | SyntaxKind::RecordKw
        ) || (self.at(SyntaxKind::At) && self.nth(1) == Some(SyntaxKind::InterfaceKw))
        {
            self.parse_type_declaration_inner(checkpoint);
            return;
        }

        // Constructor: Ident '('
        if self.at_ident_like() && self.nth(1) == Some(SyntaxKind::LParen) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::ConstructorDeclaration.into());
            self.bump(); // name
            self.parse_parameter_list();
            self.parse_throws_opt();
            self.parse_block();
            self.builder.finish_node();
            return;
        }

        // Method or field.
        if self.at(SyntaxKind::VoidKw) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::MethodDeclaration.into());
            self.bump();
            self.expect_ident_like("expected method name");
            self.parse_parameter_list();
            self.parse_throws_opt();
            if self.at(SyntaxKind::LBrace) {
                self.parse_block();
            } else {
                self.expect(SyntaxKind::Semicolon, "expected `;` or method body");
            }
            self.builder.finish_node();
            return;
        }

        if self.at_type_start() {
            self.parse_type();
            if !self.at_ident_like() {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::Error.into());
                self.error_here("expected member name");
                self.recover_to_class_member_boundary();
                self.builder.finish_node();
                return;
            }

            // After type + identifier: method if '(' follows, else field.
            if self.nth(1) == Some(SyntaxKind::LParen) {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::MethodDeclaration.into());
                self.bump(); // name
                self.parse_parameter_list();
                self.parse_throws_opt();
                if self.at(SyntaxKind::LBrace) {
                    self.parse_block();
                } else {
                    self.expect(SyntaxKind::Semicolon, "expected `;` or method body");
                }
                self.builder.finish_node();
            } else {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::FieldDeclaration.into());
                self.parse_variable_declarator_list();
                self.expect(SyntaxKind::Semicolon, "expected `;` after field declaration");
                self.builder.finish_node();
            }
            return;
        }

        // Give up: recover.
        self.builder.start_node_at(checkpoint, SyntaxKind::Error.into());
        self.error_here("unexpected token in class body");
        self.recover_to_class_member_boundary();
        self.builder.finish_node();
    }

    fn parse_throws_opt(&mut self) {
        if !self.at(SyntaxKind::ThrowsKw) {
            return;
        }
        self.bump();
        self.parse_type();
        while self.at(SyntaxKind::Comma) {
            self.bump();
            self.parse_type();
        }
    }

    fn parse_modifiers(&mut self) {
        self.builder.start_node(SyntaxKind::Modifiers.into());
        loop {
            self.eat_trivia();
            if self.at(SyntaxKind::At) {
                // `@interface` is an annotation *type* declaration, not an annotation modifier.
                if self.nth(1) == Some(SyntaxKind::InterfaceKw) {
                    break;
                }
                self.parse_annotation();
                continue;
            }
            match self.current() {
                SyntaxKind::PublicKw
                | SyntaxKind::PrivateKw
                | SyntaxKind::ProtectedKw
                | SyntaxKind::StaticKw
                | SyntaxKind::AbstractKw
                | SyntaxKind::FinalKw
                | SyntaxKind::NativeKw
                | SyntaxKind::SynchronizedKw
                | SyntaxKind::TransientKw
                | SyntaxKind::VolatileKw
                | SyntaxKind::StrictfpKw
                | SyntaxKind::DefaultKw
                | SyntaxKind::SealedKw
                | SyntaxKind::NonSealedKw => {
                    self.bump();
                }
                _ => break,
            }
        }
        self.builder.finish_node();
    }

    fn parse_annotation(&mut self) {
        self.builder.start_node(SyntaxKind::Annotation.into());
        self.expect(SyntaxKind::At, "expected `@`");
        self.parse_name();
        if self.at(SyntaxKind::LParen) {
            self.parse_argument_list();
        }
        self.builder.finish_node();
    }

    fn parse_name(&mut self) {
        self.builder.start_node(SyntaxKind::Name.into());
        self.expect_ident_like("expected name");
        while self.at(SyntaxKind::Dot) && self.nth(1).map_or(false, |k| k.is_identifier_like() || k == SyntaxKind::Star)
        {
            self.bump(); // .
            if self.at(SyntaxKind::Star) {
                self.bump();
                break;
            }
            self.expect_ident_like("expected name segment");
        }
        self.builder.finish_node();
    }

    fn parse_parameter_list(&mut self) {
        self.builder.start_node(SyntaxKind::ParameterList.into());
        self.expect(SyntaxKind::LParen, "expected `(`");
        while !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
            self.builder.start_node(SyntaxKind::Parameter.into());
            self.parse_modifiers();
            if self.at_type_start() {
                self.parse_type();
            } else {
                self.error_here("expected parameter type");
            }
            self.expect_ident_like("expected parameter name");
            self.builder.finish_node();

            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect(SyntaxKind::RParen, "expected `)`");
        self.builder.finish_node();
    }

    fn parse_argument_list(&mut self) {
        self.builder.start_node(SyntaxKind::ArgumentList.into());
        self.expect(SyntaxKind::LParen, "expected `(`");
        while !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
            self.parse_expression(0);
            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect(SyntaxKind::RParen, "expected `)`");
        self.builder.finish_node();
    }

    fn parse_block(&mut self) {
        self.builder.start_node(SyntaxKind::Block.into());
        self.expect(SyntaxKind::LBrace, "expected `{`");
        while !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            self.parse_statement();
        }
        self.expect(SyntaxKind::RBrace, "expected `}`");
        self.builder.finish_node();
    }

    fn parse_statement(&mut self) {
        self.eat_trivia();
        let checkpoint = self.builder.checkpoint();
        if self.at_ident_like() && self.nth(1) == Some(SyntaxKind::Colon) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::LabeledStatement.into());
            self.bump(); // label
            self.expect(SyntaxKind::Colon, "expected `:` after label");
            self.parse_statement();
            self.builder.finish_node();
            return;
        }
        match self.current() {
            SyntaxKind::LBrace => self.parse_block(),
            SyntaxKind::IfKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::IfStatement.into());
                self.bump();
                self.expect(SyntaxKind::LParen, "expected `(` after if");
                self.parse_expression(0);
                self.expect(SyntaxKind::RParen, "expected `)`");
                self.parse_statement();
                if self.at(SyntaxKind::ElseKw) {
                    self.bump();
                    self.parse_statement();
                }
                self.builder.finish_node();
            }
            SyntaxKind::SwitchKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::SwitchStatement.into());
                self.bump();
                self.expect(SyntaxKind::LParen, "expected `(` after switch");
                self.parse_expression(0);
                self.expect(SyntaxKind::RParen, "expected `)` after switch expression");

                self.builder.start_node(SyntaxKind::SwitchBlock.into());
                self.expect(SyntaxKind::LBrace, "expected `{` after switch");
                while !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
                    self.eat_trivia();
                    if self.at(SyntaxKind::CaseKw) || self.at(SyntaxKind::DefaultKw) {
                        let is_arrow = self.parse_switch_label();
                        if is_arrow {
                            // Switch rule body: either a block or a single statement/expression.
                            self.eat_trivia();
                            if self.at(SyntaxKind::LBrace) {
                                self.parse_block();
                            } else if self.at(SyntaxKind::Semicolon) {
                                self.bump();
                            } else {
                                self.parse_statement();
                            }
                        }
                    } else {
                        self.parse_statement();
                    }
                }
                self.expect(SyntaxKind::RBrace, "expected `}` after switch block");
                self.builder.finish_node(); // SwitchBlock
                self.builder.finish_node(); // SwitchStatement
            }
            SyntaxKind::ForKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::ForStatement.into());
                self.bump();
                self.builder.start_node(SyntaxKind::ForHeader.into());
                self.expect(SyntaxKind::LParen, "expected `(` after for");
                self.parse_for_header_contents();
                self.expect(SyntaxKind::RParen, "expected `)` after for header");
                self.builder.finish_node(); // ForHeader
                self.parse_statement();
                self.builder.finish_node();
            }
            SyntaxKind::WhileKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::WhileStatement.into());
                self.bump();
                self.expect(SyntaxKind::LParen, "expected `(` after while");
                self.parse_expression(0);
                self.expect(SyntaxKind::RParen, "expected `)`");
                self.parse_statement();
                self.builder.finish_node();
            }
            SyntaxKind::DoKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::DoWhileStatement.into());
                self.bump();
                self.parse_statement();
                self.expect(SyntaxKind::WhileKw, "expected `while` after `do` body");
                self.expect(SyntaxKind::LParen, "expected `(` after while");
                self.parse_expression(0);
                self.expect(SyntaxKind::RParen, "expected `)`");
                self.expect(SyntaxKind::Semicolon, "expected `;` after do-while");
                self.builder.finish_node();
            }
            SyntaxKind::SynchronizedKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::SynchronizedStatement.into());
                self.bump();
                self.expect(SyntaxKind::LParen, "expected `(` after synchronized");
                self.parse_expression(0);
                self.expect(
                    SyntaxKind::RParen,
                    "expected `)` after synchronized expression",
                );
                self.parse_block();
                self.builder.finish_node();
            }
            SyntaxKind::TryKw => self.parse_try_statement(checkpoint),
            SyntaxKind::AssertKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::AssertStatement.into());
                self.bump();
                self.parse_expression(0);
                if self.at(SyntaxKind::Colon) {
                    self.bump();
                    self.parse_expression(0);
                }
                self.expect(SyntaxKind::Semicolon, "expected `;` after assert");
                self.builder.finish_node();
            }
            SyntaxKind::ReturnKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::ReturnStatement.into());
                self.bump();
                if !self.at(SyntaxKind::Semicolon) {
                    self.parse_expression(0);
                }
                self.expect(SyntaxKind::Semicolon, "expected `;` after return");
                self.builder.finish_node();
            }
            SyntaxKind::BreakKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::BreakStatement.into());
                self.bump();
                // Optional label.
                if self.at_ident_like() {
                    self.bump();
                }
                self.expect(SyntaxKind::Semicolon, "expected `;` after break");
                self.builder.finish_node();
            }
            SyntaxKind::ContinueKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::ContinueStatement.into());
                self.bump();
                if self.at_ident_like() {
                    self.bump();
                }
                self.expect(SyntaxKind::Semicolon, "expected `;` after continue");
                self.builder.finish_node();
            }
            SyntaxKind::ThrowKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::ThrowStatement.into());
                self.bump();
                self.parse_expression(0);
                self.expect(SyntaxKind::Semicolon, "expected `;` after throw");
                self.builder.finish_node();
            }
            SyntaxKind::Semicolon => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::EmptyStatement.into());
                self.bump();
                self.builder.finish_node();
            }
            _ => {
                if self.at_local_var_decl_start() {
                    self.builder.start_node_at(
                        checkpoint,
                        SyntaxKind::LocalVariableDeclarationStatement.into(),
                    );
                    self.parse_modifiers();
                    self.parse_type();
                    self.parse_variable_declarator_list();
                    self.expect(
                        SyntaxKind::Semicolon,
                        "expected `;` after local variable declaration",
                    );
                    self.builder.finish_node();
                } else {
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::ExpressionStatement.into());
                    self.parse_expression(0);
                    self.expect(SyntaxKind::Semicolon, "expected `;` after expression");
                    self.builder.finish_node();
                }
            }
        }
    }

    fn parse_switch_label(&mut self) -> bool {
        self.builder.start_node(SyntaxKind::SwitchLabel.into());
        let is_case = self.at(SyntaxKind::CaseKw);
        self.bump(); // case/default
        if is_case {
            if !self.at(SyntaxKind::Colon) && !self.at(SyntaxKind::Arrow) {
                self.parse_expression(0);
                while self.at(SyntaxKind::Comma) {
                    self.bump();
                    self.parse_expression(0);
                }
            } else {
                self.error_here("expected case label expression");
            }
        }

        let is_arrow = if self.at(SyntaxKind::Arrow) {
            self.bump();
            true
        } else {
            self.expect(
                SyntaxKind::Colon,
                "expected `:` or `->` after switch label",
            );
            false
        };

        self.builder.finish_node();
        is_arrow
    }

    fn parse_try_statement(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::TryStatement.into());
        self.expect(SyntaxKind::TryKw, "expected `try`");
        if self.at(SyntaxKind::LParen) {
            self.parse_resource_specification();
        }
        self.parse_block();
        while self.at(SyntaxKind::CatchKw) {
            self.builder.start_node(SyntaxKind::CatchClause.into());
            self.bump();
            self.expect(SyntaxKind::LParen, "expected `(` after catch");
            // Multi-catch: `catch (A | B e)`.
            if self.at(SyntaxKind::FinalKw) || self.at(SyntaxKind::At) {
                self.parse_modifiers();
            }
            if self.at_type_start() {
                self.parse_type();
                while self.at(SyntaxKind::Pipe) {
                    self.bump();
                    self.parse_type();
                }
            }
            self.expect_ident_like("expected catch parameter name");
            self.expect(SyntaxKind::RParen, "expected `)` after catch parameter");
            self.parse_block();
            self.builder.finish_node();
        }
        if self.at(SyntaxKind::FinallyKw) {
            self.builder.start_node(SyntaxKind::FinallyClause.into());
            self.bump();
            self.parse_block();
            self.builder.finish_node();
        }
        self.builder.finish_node();
    }

    fn parse_resource_specification(&mut self) {
        self.builder
            .start_node(SyntaxKind::ResourceSpecification.into());
        self.expect(SyntaxKind::LParen, "expected `(` after try");
        while !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
            self.builder.start_node(SyntaxKind::Resource.into());
            if self.at_local_var_decl_start() {
                self.parse_modifiers();
                self.parse_type();
                // Try-with-resources allows only a single declarator.
                self.parse_variable_declarator();
            } else {
                self.parse_expression(0);
            }
            self.builder.finish_node(); // Resource

            if self.at(SyntaxKind::Semicolon) {
                self.bump();
                // Trailing semicolon is allowed.
                continue;
            }
            break;
        }
        self.expect(SyntaxKind::RParen, "expected `)` after resource specification");
        self.builder.finish_node(); // ResourceSpecification
    }

    fn parse_for_header_contents(&mut self) {
        // Enhanced-for and classic-for share the same outer structure: `for ( ... )`.
        if self.at_local_var_decl_start() {
            self.parse_modifiers();
            self.parse_type();
            self.parse_variable_declarator_list();

            if self.at(SyntaxKind::Colon) {
                // Enhanced for: `for (T x : expr)`.
                self.bump();
                self.parse_expression(0);
                return;
            }

            // Classic for with local var init: `for (T x = 0; cond; update)`.
            self.expect(SyntaxKind::Semicolon, "expected `;` in for header");
            if !self.at(SyntaxKind::Semicolon) {
                self.parse_expression(0);
            }
            self.expect(SyntaxKind::Semicolon, "expected `;` in for header");
            if !self.at(SyntaxKind::RParen) {
                self.parse_expression(0);
                while self.at(SyntaxKind::Comma) {
                    self.bump();
                    self.parse_expression(0);
                }
            }
            return;
        }

        // Classic for: `for (init; cond; update)`, where init/update are expression lists.
        if !self.at(SyntaxKind::Semicolon) {
            self.parse_expression(0);
            while self.at(SyntaxKind::Comma) {
                self.bump();
                self.parse_expression(0);
            }
        }
        self.expect(SyntaxKind::Semicolon, "expected `;` in for header");
        if !self.at(SyntaxKind::Semicolon) {
            self.parse_expression(0);
        }
        self.expect(SyntaxKind::Semicolon, "expected `;` in for header");
        if !self.at(SyntaxKind::RParen) {
            self.parse_expression(0);
            while self.at(SyntaxKind::Comma) {
                self.bump();
                self.parse_expression(0);
            }
        }
    }

    fn parse_variable_declarator_list(&mut self) {
        self.builder
            .start_node(SyntaxKind::VariableDeclaratorList.into());
        self.parse_variable_declarator();
        while self.at(SyntaxKind::Comma) {
            self.bump();
            self.parse_variable_declarator();
        }
        self.builder.finish_node();
    }

    fn parse_variable_declarator(&mut self) {
        self.builder
            .start_node(SyntaxKind::VariableDeclarator.into());
        self.expect_ident_like("expected variable name");
        if self.at(SyntaxKind::Eq) {
            self.bump();
            if self.at(SyntaxKind::Semicolon) || self.at(SyntaxKind::Comma) {
                self.error_here("expected initializer expression");
            } else {
                self.parse_expression(0);
            }
        }
        self.builder.finish_node();
    }

    fn parse_type(&mut self) {
        self.builder.start_node(SyntaxKind::Type.into());
        self.eat_trivia();
        if self.at_primitive_type() {
            self.builder.start_node(SyntaxKind::PrimitiveType.into());
            self.bump();
            self.builder.finish_node();
        } else {
            self.builder.start_node(SyntaxKind::NamedType.into());
            self.expect_ident_like("expected type name");
            while self.at(SyntaxKind::Dot) && self.nth(1).map_or(false, |k| k.is_identifier_like()) {
                self.bump();
                self.expect_ident_like("expected type name segment");
            }
            if self.at(SyntaxKind::Less) {
                self.parse_type_arguments();
            }
            self.builder.finish_node();
        }
        while self.at(SyntaxKind::LBracket) && self.nth(1) == Some(SyntaxKind::RBracket) {
            self.bump();
            self.bump();
        }
        self.builder.finish_node();
    }

    fn parse_type_arguments(&mut self) {
        self.builder.start_node(SyntaxKind::TypeArguments.into());
        self.expect(SyntaxKind::Less, "expected `<`");
        while !matches!(
            self.current(),
            SyntaxKind::Greater | SyntaxKind::RightShift | SyntaxKind::UnsignedRightShift | SyntaxKind::Eof
        ) {
            self.builder.start_node(SyntaxKind::TypeArgument.into());
            if self.at(SyntaxKind::Question) {
                self.builder.start_node(SyntaxKind::WildcardType.into());
                self.bump();
                if self.at(SyntaxKind::ExtendsKw) || self.at(SyntaxKind::SuperKw) {
                    self.bump();
                    self.parse_type();
                }
                self.builder.finish_node();
            } else {
                self.parse_type();
            }
            self.builder.finish_node();
            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect_gt();
        self.builder.finish_node();
    }

    fn expect_gt(&mut self) {
        self.eat_trivia();
        match self.current() {
            SyntaxKind::Greater => {
                self.bump();
            }
            SyntaxKind::RightShift | SyntaxKind::UnsignedRightShift => {
                self.split_shift_as_greater();
                self.bump();
            }
            _ => {
                self.error_here("expected `>`");
            }
        }
    }

    fn split_shift_as_greater(&mut self) {
        let tok = match self.tokens.pop_front() {
            Some(tok) => tok,
            None => return,
        };
        let start = tok.range.start;
        let end = tok.range.end;
        match tok.kind {
            SyntaxKind::RightShift => {
                // Push remaining '>' first.
                self.tokens.push_front(Token {
                    kind: SyntaxKind::Greater,
                    range: TextRange {
                        start: start + 1,
                        end,
                    },
                });
                self.tokens.push_front(Token {
                    kind: SyntaxKind::Greater,
                    range: TextRange {
                        start,
                        end: start + 1,
                    },
                });
            }
            SyntaxKind::UnsignedRightShift => {
                self.tokens.push_front(Token {
                    kind: SyntaxKind::Greater,
                    range: TextRange {
                        start: start + 2,
                        end,
                    },
                });
                self.tokens.push_front(Token {
                    kind: SyntaxKind::Greater,
                    range: TextRange {
                        start: start + 1,
                        end: start + 2,
                    },
                });
                self.tokens.push_front(Token {
                    kind: SyntaxKind::Greater,
                    range: TextRange {
                        start,
                        end: start + 1,
                    },
                });
            }
            _ => {
                self.tokens.push_front(tok);
            }
        }
    }

    fn parse_expression(&mut self, min_bp: u8) {
        self.eat_trivia();
        let checkpoint = self.builder.checkpoint();

        // Prefix / primary.
        match self.current() {
            SyntaxKind::IntLiteral
            | SyntaxKind::LongLiteral
            | SyntaxKind::FloatLiteral
            | SyntaxKind::DoubleLiteral
            | SyntaxKind::CharLiteral
            | SyntaxKind::StringLiteral
            | SyntaxKind::TextBlock
            | SyntaxKind::TrueKw
            | SyntaxKind::FalseKw
            | SyntaxKind::NullKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::LiteralExpression.into());
                self.bump();
                self.builder.finish_node();
            }
            SyntaxKind::ThisKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::ThisExpression.into());
                self.bump();
                self.builder.finish_node();
            }
            SyntaxKind::SuperKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::SuperExpression.into());
                self.bump();
                self.builder.finish_node();
            }
            SyntaxKind::NewKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::NewExpression.into());
                self.bump();
                self.parse_type();
                if self.at(SyntaxKind::LParen) {
                    self.parse_argument_list();
                }
                self.builder.finish_node();
            }
            SyntaxKind::Plus
            | SyntaxKind::Minus
            | SyntaxKind::Bang
            | SyntaxKind::Tilde
            | SyntaxKind::PlusPlus
            | SyntaxKind::MinusMinus => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::UnaryExpression.into());
                self.bump();
                self.parse_expression(100);
                self.builder.finish_node();
            }
            SyntaxKind::Identifier
            | SyntaxKind::VarKw
            | SyntaxKind::YieldKw
            | SyntaxKind::RecordKw
            | SyntaxKind::SealedKw
            | SyntaxKind::PermitsKw
            | SyntaxKind::NonSealedKw
            | SyntaxKind::WhenKw
            | SyntaxKind::ModuleKw
            | SyntaxKind::OpenKw
            | SyntaxKind::OpensKw
            | SyntaxKind::RequiresKw
            | SyntaxKind::TransitiveKw
            | SyntaxKind::ExportsKw
            | SyntaxKind::ToKw
            | SyntaxKind::UsesKw
            | SyntaxKind::ProvidesKw
            | SyntaxKind::WithKw => {
                if self.nth(1) == Some(SyntaxKind::Arrow) {
                    self.parse_lambda_expression(checkpoint);
                } else {
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::NameExpression.into());
                    self.bump();
                    while self.at(SyntaxKind::Dot) && self.nth(1).map_or(false, |k| k.is_identifier_like())
                    {
                        self.bump();
                        self.bump();
                    }
                    self.builder.finish_node();
                }
            }
            SyntaxKind::LParen => {
                if self.is_lambda_paren() {
                    self.parse_lambda_expression(checkpoint);
                } else if self.is_cast_expression() {
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::CastExpression.into());
                    self.bump();
                    self.parse_type();
                    self.expect(SyntaxKind::RParen, "expected `)` in cast");
                    self.parse_expression(100);
                    self.builder.finish_node();
                } else {
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::ParenthesizedExpression.into());
                    self.bump();
                    self.parse_expression(0);
                    self.expect(SyntaxKind::RParen, "expected `)`");
                    self.builder.finish_node();
                }
            }
            _ => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::Error.into());
                self.error_here("expected expression");
                // Consume one token to ensure progress.
                if !self.at(SyntaxKind::Eof) {
                    self.bump_any();
                }
                self.builder.finish_node();
            }
        }

        loop {
            self.eat_trivia();
            let op = self.current();

            // Postfix: call, field access, array access.
            match op {
                SyntaxKind::LParen => {
                    if min_bp > 120 {
                        break;
                    }
                    self.builder.start_node_at(checkpoint, SyntaxKind::MethodCallExpression.into());
                    self.parse_argument_list();
                    self.builder.finish_node();
                    continue;
                }
                SyntaxKind::Dot => {
                    if min_bp > 120 {
                        break;
                    }
                    if self.nth(1).map_or(false, |k| k.is_identifier_like()) {
                        self.builder
                            .start_node_at(checkpoint, SyntaxKind::FieldAccessExpression.into());
                        self.bump();
                        self.bump();
                        self.builder.finish_node();
                        continue;
                    }
                    break;
                }
                SyntaxKind::LBracket => {
                    if min_bp > 120 {
                        break;
                    }
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::ArrayAccessExpression.into());
                    self.bump();
                    if !self.at(SyntaxKind::RBracket) {
                        self.parse_expression(0);
                    }
                    self.expect(SyntaxKind::RBracket, "expected `]`");
                    self.builder.finish_node();
                    continue;
                }
                _ => {}
            }

            if let Some((l_bp, r_bp, expr_kind)) = infix_binding_power(op) {
                if l_bp < min_bp {
                    break;
                }
                self.builder.start_node_at(checkpoint, expr_kind.into());
                self.bump();
                self.parse_expression(r_bp);
                self.builder.finish_node();
                continue;
            }

            // Conditional.
            if op == SyntaxKind::Question {
                let (l_bp, r_bp) = (2, 1);
                if l_bp < min_bp {
                    break;
                }
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::ConditionalExpression.into());
                self.bump(); // ?
                self.parse_expression(0);
                self.expect(SyntaxKind::Colon, "expected `:` in conditional expression");
                self.parse_expression(r_bp);
                self.builder.finish_node();
                continue;
            }

            break;
        }
    }

    fn parse_lambda_expression(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::LambdaExpression.into());
        // Params.
        if self.at(SyntaxKind::LParen) {
            self.bump();
            while !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
                if self.at_ident_like() {
                    self.bump();
                } else if self.at(SyntaxKind::Comma) {
                    self.bump();
                } else {
                    self.bump_any();
                }
            }
            self.expect(SyntaxKind::RParen, "expected `)` in lambda parameters");
        } else {
            self.expect_ident_like("expected lambda parameter");
        }
        self.expect(SyntaxKind::Arrow, "expected `->` in lambda");
        if self.at(SyntaxKind::LBrace) {
            self.parse_block();
        } else {
            self.parse_expression(0);
        }
        self.builder.finish_node();
    }

    fn is_lambda_paren(&mut self) -> bool {
        if !self.at(SyntaxKind::LParen) {
            return false;
        }
        // Look for matching ')', then '->'.
        let mut depth = 0usize;
        let mut i = 0usize;
        while let Some(tok) = self.tokens.get(i) {
            let kind = tok.kind;
            if kind.is_trivia() {
                i += 1;
                continue;
            }
            if kind == SyntaxKind::LParen {
                depth += 1;
            } else if kind == SyntaxKind::RParen {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    // Next non-trivia.
                    let mut j = i + 1;
                    while let Some(next) = self.tokens.get(j) {
                        if next.kind.is_trivia() {
                            j += 1;
                            continue;
                        }
                        return next.kind == SyntaxKind::Arrow;
                    }
                    return false;
                }
            }
            i += 1;
        }
        false
    }

    fn is_cast_expression(&mut self) -> bool {
        // Basic heuristic: `(Type) expr` where Type starts with primitive or identifier-like.
        if !self.at(SyntaxKind::LParen) {
            return false;
        }
        let mut i = 1usize;
        while let Some(tok) = self.tokens.get(i) {
            if tok.kind.is_trivia() {
                i += 1;
                continue;
            }
            if tok.kind == SyntaxKind::RParen {
                // Need an expression after ')'.
                let mut j = i + 1;
                while let Some(next) = self.tokens.get(j) {
                    if next.kind.is_trivia() {
                        j += 1;
                        continue;
                    }
                    return can_start_expression(next.kind);
                }
                return false;
            }
            // Reject obvious separators that indicate it's a parenthesized expression.
            if tok.kind == SyntaxKind::Comma {
                return false;
            }
            i += 1;
        }
        false
    }

    fn at_local_var_decl_start(&mut self) -> bool {
        let mut i = skip_trivia(&self.tokens, 0);

        // Local variable modifiers: `final` + annotations.
        loop {
            match self.tokens.get(i).map(|t| t.kind) {
                Some(SyntaxKind::FinalKw) => {
                    i = skip_trivia(&self.tokens, i + 1);
                }
                Some(SyntaxKind::At) => {
                    // Skip `@Name(...)` very loosely.
                    i = skip_trivia(&self.tokens, i + 1);
                    if self.tokens.get(i).map_or(false, |t| t.kind.is_identifier_like()) {
                        i += 1;
                        loop {
                            let dot = skip_trivia(&self.tokens, i);
                            if self.tokens.get(dot).map(|t| t.kind) != Some(SyntaxKind::Dot) {
                                i = dot;
                                break;
                            }
                            let seg = skip_trivia(&self.tokens, dot + 1);
                            if !self
                                .tokens
                                .get(seg)
                                .map_or(false, |t| t.kind.is_identifier_like())
                            {
                                i = dot;
                                break;
                            }
                            i = seg + 1;
                        }
                    }

                    i = skip_trivia(&self.tokens, i);
                    if self.tokens.get(i).map(|t| t.kind) == Some(SyntaxKind::LParen) {
                        i = skip_balanced_parens(&self.tokens, i);
                    }
                    i = skip_trivia(&self.tokens, i);
                }
                _ => break,
            }
        }

        // Parse a probable type, then require an identifier-like declarator name.
        let Some(first) = self.tokens.get(i).map(|t| t.kind) else {
            return false;
        };

        if first == SyntaxKind::VarKw {
            let j = skip_trivia(&self.tokens, i + 1);
            return self
                .tokens
                .get(j)
                .map_or(false, |t| t.kind.is_identifier_like());
        }

        if is_primitive_type(first) {
            i += 1;
        } else if first.is_identifier_like() {
            i += 1;
            // Qualified name.
            loop {
                let dot = skip_trivia(&self.tokens, i);
                if self.tokens.get(dot).map(|t| t.kind) != Some(SyntaxKind::Dot) {
                    i = dot;
                    break;
                }
                let seg = skip_trivia(&self.tokens, dot + 1);
                if !self
                    .tokens
                    .get(seg)
                    .map_or(false, |t| t.kind.is_identifier_like())
                {
                    i = dot;
                    break;
                }
                i = seg + 1;
            }

            i = skip_trivia(&self.tokens, i);
            if self.tokens.get(i).map(|t| t.kind) == Some(SyntaxKind::Less) {
                i = skip_type_arguments(&self.tokens, i);
            }
        } else {
            return false;
        }

        // Array dims: `[]`*
        loop {
            let j = skip_trivia(&self.tokens, i);
            if self.tokens.get(j).map(|t| t.kind) != Some(SyntaxKind::LBracket) {
                i = j;
                break;
            }
            let after_l = skip_trivia(&self.tokens, j + 1);
            if self.tokens.get(after_l).map(|t| t.kind) != Some(SyntaxKind::RBracket) {
                i = j;
                break;
            }
            i = after_l + 1;
        }

        i = skip_trivia(&self.tokens, i);
        self.tokens
            .get(i)
            .map_or(false, |t| t.kind.is_identifier_like())
    }

    fn recover_top_level(&mut self) {
        self.builder.start_node(SyntaxKind::Error.into());
        self.error_here("unexpected token at top level");
        self.recover_to(&[
            SyntaxKind::PackageKw,
            SyntaxKind::ImportKw,
            SyntaxKind::ClassKw,
            SyntaxKind::InterfaceKw,
            SyntaxKind::EnumKw,
            SyntaxKind::RecordKw,
            SyntaxKind::Eof,
        ]);
        self.builder.finish_node();
    }

    fn recover_to_class_member_boundary(&mut self) {
        self.recover_to(&[
            SyntaxKind::Semicolon,
            SyntaxKind::RBrace,
            SyntaxKind::ClassKw,
            SyntaxKind::InterfaceKw,
            SyntaxKind::EnumKw,
            SyntaxKind::RecordKw,
            SyntaxKind::PublicKw,
            SyntaxKind::PrivateKw,
            SyntaxKind::ProtectedKw,
            SyntaxKind::StaticKw,
            SyntaxKind::FinalKw,
            SyntaxKind::AbstractKw,
            SyntaxKind::At,
        ]);
        // If we stopped at `;`, consume it to avoid loops.
        if self.at(SyntaxKind::Semicolon) {
            self.bump();
        }
    }

    fn recover_to(&mut self, recovery: &[SyntaxKind]) {
        while !self.at(SyntaxKind::Eof) {
            if recovery.contains(&self.current()) {
                break;
            }
            self.bump_any();
        }
    }

    fn at_type_decl_start(&mut self) -> bool {
        matches!(
            self.current(),
            SyntaxKind::ClassKw
                | SyntaxKind::InterfaceKw
                | SyntaxKind::EnumKw
                | SyntaxKind::RecordKw
                | SyntaxKind::PublicKw
                | SyntaxKind::PrivateKw
                | SyntaxKind::ProtectedKw
                | SyntaxKind::StaticKw
                | SyntaxKind::FinalKw
                | SyntaxKind::AbstractKw
                | SyntaxKind::At
                | SyntaxKind::SealedKw
                | SyntaxKind::NonSealedKw
                | SyntaxKind::StrictfpKw
                | SyntaxKind::Semicolon
        )
    }

    fn at_type_start(&mut self) -> bool {
        self.at_primitive_type() || self.at_ident_like()
    }

    fn at_primitive_type(&mut self) -> bool {
        is_primitive_type(self.current())
    }

    fn current(&mut self) -> SyntaxKind {
        self.eat_trivia();
        self.tokens.front().map(|t| t.kind).unwrap_or(SyntaxKind::Eof)
    }

    fn nth(&mut self, n: usize) -> Option<SyntaxKind> {
        let mut idx = 0usize;
        let mut remaining = n;
        while let Some(tok) = self.tokens.get(idx) {
            if tok.kind.is_trivia() {
                idx += 1;
                continue;
            }
            if remaining == 0 {
                return Some(tok.kind);
            }
            remaining -= 1;
            idx += 1;
        }
        None
    }

    fn at(&mut self, kind: SyntaxKind) -> bool {
        self.current() == kind
    }

    fn at_ident_like(&mut self) -> bool {
        self.current().is_identifier_like()
    }

    fn eat_trivia(&mut self) {
        while self.tokens.front().map_or(false, |t| t.kind.is_trivia()) {
            self.bump_any();
        }
    }

    fn bump(&mut self) {
        self.eat_trivia();
        self.bump_any();
    }

    fn bump_any(&mut self) {
        if let Some(tok) = self.tokens.pop_front() {
            let text = tok.text(self.input);
            self.builder.token(tok.kind.into(), text);
        }
    }

    fn expect(&mut self, kind: SyntaxKind, message: &str) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            self.error_here(message);
            false
        }
    }

    fn expect_ident_like(&mut self, message: &str) {
        if self.at_ident_like() {
            self.bump();
        } else {
            self.error_here(message);
        }
    }

    fn error_here(&mut self, message: &str) {
        let range = self.current_range();
        self.errors.push(ParseError {
            message: message.to_string(),
            range,
        });
    }

    fn current_range(&mut self) -> TextRange {
        self.eat_trivia();
        self.tokens
            .front()
            .map(|t| t.range)
            .unwrap_or_else(|| {
                let end = self.input.len() as u32;
                TextRange { start: end, end }
            })
    }
}

fn skip_trivia(tokens: &VecDeque<Token>, mut idx: usize) -> usize {
    while tokens.get(idx).map_or(false, |t| t.kind.is_trivia()) {
        idx += 1;
    }
    idx
}

fn skip_balanced_parens(tokens: &VecDeque<Token>, mut idx: usize) -> usize {
    // Assumes `tokens[idx]` is `(`.
    let mut depth = 0usize;
    while let Some(tok) = tokens.get(idx) {
        if tok.kind.is_trivia() {
            idx += 1;
            continue;
        }
        match tok.kind {
            SyntaxKind::LParen => {
                depth += 1;
            }
            SyntaxKind::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    idx += 1;
                    break;
                }
            }
            SyntaxKind::Eof => {
                break;
            }
            _ => {}
        }
        idx += 1;
    }
    idx
}

fn skip_type_arguments(tokens: &VecDeque<Token>, mut idx: usize) -> usize {
    // Assumes `tokens[idx]` is `<`. We do a shallow, token-based matching pass to
    // determine whether this looks like type arguments.
    let mut depth: i32 = 0;
    while let Some(tok) = tokens.get(idx) {
        if tok.kind.is_trivia() {
            idx += 1;
            continue;
        }
        match tok.kind {
            SyntaxKind::Less => {
                depth += 1;
            }
            SyntaxKind::Greater => {
                depth -= 1;
            }
            SyntaxKind::RightShift => {
                depth -= 2;
            }
            SyntaxKind::UnsignedRightShift => {
                depth -= 3;
            }
            SyntaxKind::Eof => break,
            _ => {}
        }
        idx += 1;
        if depth <= 0 {
            break;
        }
    }
    idx
}

fn is_primitive_type(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::BooleanKw
            | SyntaxKind::ByteKw
            | SyntaxKind::ShortKw
            | SyntaxKind::IntKw
            | SyntaxKind::LongKw
            | SyntaxKind::CharKw
            | SyntaxKind::FloatKw
            | SyntaxKind::DoubleKw
    )
}

fn can_start_expression(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Identifier
            | SyntaxKind::VarKw
            | SyntaxKind::YieldKw
            | SyntaxKind::RecordKw
            | SyntaxKind::SealedKw
            | SyntaxKind::PermitsKw
            | SyntaxKind::NonSealedKw
            | SyntaxKind::WhenKw
            | SyntaxKind::ModuleKw
            | SyntaxKind::OpenKw
            | SyntaxKind::OpensKw
            | SyntaxKind::RequiresKw
            | SyntaxKind::TransitiveKw
            | SyntaxKind::ExportsKw
            | SyntaxKind::ToKw
            | SyntaxKind::UsesKw
            | SyntaxKind::ProvidesKw
            | SyntaxKind::WithKw
            | SyntaxKind::ThisKw
            | SyntaxKind::SuperKw
            | SyntaxKind::NewKw
            | SyntaxKind::LParen
            | SyntaxKind::IntLiteral
            | SyntaxKind::LongLiteral
            | SyntaxKind::FloatLiteral
            | SyntaxKind::DoubleLiteral
            | SyntaxKind::CharLiteral
            | SyntaxKind::StringLiteral
            | SyntaxKind::TextBlock
            | SyntaxKind::TrueKw
            | SyntaxKind::FalseKw
            | SyntaxKind::NullKw
            | SyntaxKind::Plus
            | SyntaxKind::Minus
            | SyntaxKind::Bang
            | SyntaxKind::Tilde
            | SyntaxKind::PlusPlus
            | SyntaxKind::MinusMinus
    )
}

fn infix_binding_power(op: SyntaxKind) -> Option<(u8, u8, SyntaxKind)> {
    // Returns (left_bp, right_bp, node_kind).
    // Larger = tighter binding.
    let (l, r, kind) = match op {
        SyntaxKind::Star | SyntaxKind::Slash | SyntaxKind::Percent => (70, 71, SyntaxKind::BinaryExpression),
        SyntaxKind::Plus | SyntaxKind::Minus => (60, 61, SyntaxKind::BinaryExpression),
        SyntaxKind::LeftShift | SyntaxKind::RightShift | SyntaxKind::UnsignedRightShift => {
            (55, 56, SyntaxKind::BinaryExpression)
        }
        SyntaxKind::Less | SyntaxKind::LessEq | SyntaxKind::Greater | SyntaxKind::GreaterEq | SyntaxKind::InstanceofKw => {
            (50, 51, SyntaxKind::BinaryExpression)
        }
        SyntaxKind::EqEq | SyntaxKind::BangEq => (45, 46, SyntaxKind::BinaryExpression),
        SyntaxKind::Amp => (40, 41, SyntaxKind::BinaryExpression),
        SyntaxKind::Caret => (39, 40, SyntaxKind::BinaryExpression),
        SyntaxKind::Pipe => (38, 39, SyntaxKind::BinaryExpression),
        SyntaxKind::AmpAmp => (30, 31, SyntaxKind::BinaryExpression),
        SyntaxKind::PipePipe => (20, 21, SyntaxKind::BinaryExpression),

        // Assignment (right-associative).
        SyntaxKind::Eq
        | SyntaxKind::PlusEq
        | SyntaxKind::MinusEq
        | SyntaxKind::StarEq
        | SyntaxKind::SlashEq
        | SyntaxKind::PercentEq
        | SyntaxKind::AmpEq
        | SyntaxKind::PipeEq
        | SyntaxKind::CaretEq
        | SyntaxKind::LeftShiftEq
        | SyntaxKind::RightShiftEq
        | SyntaxKind::UnsignedRightShiftEq => (1, 0, SyntaxKind::AssignmentExpression),

        _ => return None,
    };
    Some((l, r, kind))
}

// --- debug helpers used by tests ---

#[cfg(test)]
pub fn debug_dump(node: &SyntaxNode) -> String {
    fn go(node: &SyntaxNode, indent: usize, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(out, "{:indent$}{:?}", "", node.kind(), indent = indent);
        for child in node.children_with_tokens() {
            match child {
                NodeOrToken::Node(n) => go(&n, indent + 2, out),
                NodeOrToken::Token(t) => {
                    let _ = writeln!(
                        out,
                        "{:indent$}{:?} {:?}",
                        "",
                        t.kind(),
                        t.text(),
                        indent = indent + 2
                    );
                }
            }
        }
    }

    let mut out = String::new();
    go(node, 0, &mut out);
    out
}
