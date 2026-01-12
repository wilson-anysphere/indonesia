use std::collections::VecDeque;

#[cfg(test)]
use rowan::NodeOrToken;
use rowan::{GreenNode, GreenNodeBuilder};
use text_size::TextSize;

use crate::lexer::{lex_with_errors, Token};
use crate::syntax_kind::{JavaLanguage, SyntaxKind};
use crate::{ParseError, TextRange};

#[derive(Clone, Copy)]
struct TokenSet(&'static [SyntaxKind]);

impl TokenSet {
    const fn new(kinds: &'static [SyntaxKind]) -> Self {
        Self(kinds)
    }

    fn contains(self, kind: SyntaxKind) -> bool {
        self.0.contains(&kind)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatementContext {
    Normal,
    /// Within a switch expression body where `yield` should be parsed as a `YieldStatement`.
    SwitchExpression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchContext {
    Statement,
    Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchLabelTerminator {
    Colon,
    Arrow,
}

#[derive(Default, Clone, Copy)]
struct DelimiterDepth {
    braces: u32,
    parens: u32,
    brackets: u32,
    angles: u32,
}

impl DelimiterDepth {
    fn is_zero(self, track_angles: bool) -> bool {
        self.braces == 0
            && self.parens == 0
            && self.brackets == 0
            && (!track_angles || self.angles == 0)
    }

    fn update(&mut self, kind: SyntaxKind, track_angles: bool) {
        match kind {
            SyntaxKind::LBrace | SyntaxKind::StringTemplateExprStart => self.braces += 1,
            SyntaxKind::RBrace | SyntaxKind::StringTemplateExprEnd => {
                self.braces = self.braces.saturating_sub(1)
            }
            SyntaxKind::LParen => self.parens += 1,
            SyntaxKind::RParen => self.parens = self.parens.saturating_sub(1),
            SyntaxKind::LBracket => self.brackets += 1,
            SyntaxKind::RBracket => self.brackets = self.brackets.saturating_sub(1),
            _ => {}
        }

        if !track_angles {
            return;
        }

        match kind {
            SyntaxKind::Less => self.angles += 1,
            SyntaxKind::Greater => self.angles = self.angles.saturating_sub(1),
            SyntaxKind::RightShift => self.angles = self.angles.saturating_sub(2),
            SyntaxKind::UnsignedRightShift => self.angles = self.angles.saturating_sub(3),
            _ => {}
        }
    }
}

const TOP_LEVEL_RECOVERY: TokenSet = TokenSet::new(&[
    SyntaxKind::PackageKw,
    SyntaxKind::ImportKw,
    SyntaxKind::OpenKw,
    SyntaxKind::ModuleKw,
    SyntaxKind::ClassKw,
    SyntaxKind::InterfaceKw,
    SyntaxKind::EnumKw,
    SyntaxKind::RecordKw,
    SyntaxKind::At,
    SyntaxKind::PublicKw,
    SyntaxKind::PrivateKw,
    SyntaxKind::ProtectedKw,
    SyntaxKind::StaticKw,
    SyntaxKind::FinalKw,
    SyntaxKind::AbstractKw,
    SyntaxKind::SealedKw,
    SyntaxKind::NonSealedKw,
    SyntaxKind::StrictfpKw,
    SyntaxKind::Semicolon,
    SyntaxKind::Eof,
]);

const TYPE_DECL_RECOVERY: TokenSet = TokenSet::new(&[
    SyntaxKind::RBrace,
    SyntaxKind::StringTemplateExprEnd,
    SyntaxKind::PackageKw,
    SyntaxKind::ImportKw,
    SyntaxKind::OpenKw,
    SyntaxKind::ModuleKw,
    SyntaxKind::ClassKw,
    SyntaxKind::InterfaceKw,
    SyntaxKind::EnumKw,
    SyntaxKind::RecordKw,
    SyntaxKind::At,
    SyntaxKind::PublicKw,
    SyntaxKind::PrivateKw,
    SyntaxKind::ProtectedKw,
    SyntaxKind::StaticKw,
    SyntaxKind::FinalKw,
    SyntaxKind::AbstractKw,
    SyntaxKind::SealedKw,
    SyntaxKind::NonSealedKw,
    SyntaxKind::StrictfpKw,
    SyntaxKind::Semicolon,
    SyntaxKind::Eof,
]);

const MODULE_DIRECTIVE_RECOVERY: TokenSet = TokenSet::new(&[
    SyntaxKind::Semicolon,
    SyntaxKind::RBrace,
    SyntaxKind::StringTemplateExprEnd,
    SyntaxKind::RequiresKw,
    SyntaxKind::ExportsKw,
    SyntaxKind::OpensKw,
    SyntaxKind::UsesKw,
    SyntaxKind::ProvidesKw,
    SyntaxKind::Eof,
]);

const MEMBER_RECOVERY: TokenSet = TokenSet::new(&[
    SyntaxKind::Semicolon,
    SyntaxKind::RBrace,
    SyntaxKind::StringTemplateExprEnd,
    SyntaxKind::LBrace,
    SyntaxKind::ClassKw,
    SyntaxKind::InterfaceKw,
    SyntaxKind::EnumKw,
    SyntaxKind::RecordKw,
    SyntaxKind::At,
    SyntaxKind::PublicKw,
    SyntaxKind::PrivateKw,
    SyntaxKind::ProtectedKw,
    SyntaxKind::StaticKw,
    SyntaxKind::FinalKw,
    SyntaxKind::AbstractKw,
    SyntaxKind::NativeKw,
    SyntaxKind::SynchronizedKw,
    SyntaxKind::TransientKw,
    SyntaxKind::VolatileKw,
    SyntaxKind::StrictfpKw,
    SyntaxKind::DefaultKw,
    SyntaxKind::SealedKw,
    SyntaxKind::NonSealedKw,
    SyntaxKind::VoidKw,
    SyntaxKind::Identifier,
    SyntaxKind::BooleanKw,
    SyntaxKind::ByteKw,
    SyntaxKind::ShortKw,
    SyntaxKind::IntKw,
    SyntaxKind::LongKw,
    SyntaxKind::CharKw,
    SyntaxKind::FloatKw,
    SyntaxKind::DoubleKw,
    SyntaxKind::Eof,
]);

const STMT_RECOVERY: TokenSet = TokenSet::new(&[
    SyntaxKind::Semicolon,
    SyntaxKind::RBrace,
    SyntaxKind::StringTemplateExprEnd,
    SyntaxKind::LBrace,
    SyntaxKind::IfKw,
    SyntaxKind::SwitchKw,
    SyntaxKind::ForKw,
    SyntaxKind::WhileKw,
    SyntaxKind::DoKw,
    SyntaxKind::TryKw,
    SyntaxKind::ReturnKw,
    SyntaxKind::ThrowKw,
    SyntaxKind::BreakKw,
    SyntaxKind::ContinueKw,
    SyntaxKind::AssertKw,
    SyntaxKind::SynchronizedKw,
    SyntaxKind::CaseKw,
    SyntaxKind::DefaultKw,
    SyntaxKind::Eof,
]);

const IF_THEN_BLOCK_RECOVERY: TokenSet = TokenSet::new(&[SyntaxKind::ElseKw]);

const TRY_BLOCK_RECOVERY: TokenSet = TokenSet::new(&[SyntaxKind::CatchKw, SyntaxKind::FinallyKw]);

const DO_BODY_RECOVERY: TokenSet = TokenSet::new(&[SyntaxKind::WhileKw]);

const TYPE_ARGUMENT_RECOVERY: TokenSet = TokenSet::new(&[
    SyntaxKind::Comma,
    SyntaxKind::Greater,
    SyntaxKind::RightShift,
    SyntaxKind::UnsignedRightShift,
    SyntaxKind::RParen,
    SyntaxKind::RBracket,
    SyntaxKind::RBrace,
    SyntaxKind::StringTemplateExprEnd,
    SyntaxKind::Semicolon,
    SyntaxKind::Eof,
]);

const TYPE_PARAMETER_RECOVERY: TokenSet = TokenSet::new(&[
    SyntaxKind::Comma,
    SyntaxKind::Greater,
    SyntaxKind::RightShift,
    SyntaxKind::UnsignedRightShift,
    SyntaxKind::LParen,
    SyntaxKind::LBrace,
    SyntaxKind::Semicolon,
    SyntaxKind::RParen,
    SyntaxKind::RBrace,
    SyntaxKind::StringTemplateExprEnd,
    SyntaxKind::Eof,
]);

const EXPR_RECOVERY: TokenSet = TokenSet::new(&[
    SyntaxKind::Semicolon,
    SyntaxKind::Comma,
    SyntaxKind::RParen,
    SyntaxKind::RBracket,
    SyntaxKind::RBrace,
    SyntaxKind::StringTemplateExprEnd,
    SyntaxKind::Colon,
    SyntaxKind::Arrow,
    SyntaxKind::Eof,
]);

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

    /// Returns the top-level expression when this parse result was produced by
    /// [`parse_expression`].
    pub fn expression(&self) -> Option<SyntaxNode> {
        let root = self.syntax();
        if root.kind() != SyntaxKind::ExpressionRoot {
            return None;
        }
        root.children().next()
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

/// Result of parsing a Java fragment extracted from a larger source file.
///
/// The returned rowan tree is **fragment-relative**: all node/token
/// `text_range()` values start at `0` and are relative to the provided `text`
/// slice. Use the `*_in_file` helpers to convert ranges/offsets back to the
/// original file coordinates using [`JavaFragmentParseResult::offset`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaFragmentParseResult {
    pub parse: JavaParseResult,
    /// Byte offset of the fragment within the original source file.
    pub offset: u32,
}

impl JavaFragmentParseResult {
    fn fragment_len(&self) -> u32 {
        u32::from(self.parse.syntax().text_range().end())
    }

    /// Converts a file-relative byte offset to a fragment-relative offset.
    ///
    /// Returns `None` when the file offset lies outside this fragment.
    pub fn file_to_fragment_offset(&self, file: u32) -> Option<u32> {
        if file < self.offset {
            return None;
        }
        let fragment = file - self.offset;
        if fragment <= self.fragment_len() {
            Some(fragment)
        } else {
            None
        }
    }

    /// Converts a fragment-relative byte offset to a file-relative offset.
    #[inline]
    pub fn fragment_to_file_offset(&self, fragment: u32) -> u32 {
        self.offset.saturating_add(fragment)
    }

    /// Converts a file-relative range into a fragment-relative range.
    ///
    /// Returns `None` when the input range lies outside this fragment.
    pub fn file_to_fragment_range(&self, range: TextRange) -> Option<TextRange> {
        let len = self.fragment_len();
        if range.start < self.offset || range.end < self.offset {
            return None;
        }
        let start = range.start - self.offset;
        let end = range.end - self.offset;
        if end <= len {
            Some(TextRange { start, end })
        } else {
            None
        }
    }

    /// Converts a fragment-relative range into a file-relative range.
    #[inline]
    pub fn fragment_to_file_range(&self, range: TextRange) -> TextRange {
        TextRange {
            start: self.fragment_to_file_offset(range.start),
            end: self.fragment_to_file_offset(range.end),
        }
    }

    /// Returns the token at a file-relative byte offset, or `None` when the
    /// offset lies outside the fragment.
    pub fn token_at_file_offset(&self, file: u32) -> rowan::TokenAtOffset<SyntaxToken> {
        let Some(fragment) = self.file_to_fragment_offset(file) else {
            return rowan::TokenAtOffset::None;
        };
        self.parse.token_at_offset(fragment)
    }

    /// Returns the covering syntax element for a file-relative range.
    ///
    /// Returns `None` when the range lies outside the fragment.
    pub fn covering_element_in_file(&self, range: TextRange) -> Option<SyntaxElement> {
        let fragment = self.file_to_fragment_range(range)?;
        Some(self.parse.covering_element(fragment))
    }

    /// Returns a node's range in file coordinates.
    ///
    /// `SyntaxNode::text_range()` is fragment-relative; this helper adds
    /// [`JavaFragmentParseResult::offset`].
    pub fn node_range_in_file(&self, node: &SyntaxNode) -> TextRange {
        let range = node.text_range();
        TextRange {
            start: self.fragment_to_file_offset(u32::from(range.start())),
            end: self.fragment_to_file_offset(u32::from(range.end())),
        }
    }
}

pub fn parse_java(input: &str) -> JavaParseResult {
    Parser::new(input).parse()
}

/// Parses `input` as a standalone Java expression.
///
/// This entry point is intended for debugger/expression-evaluation scenarios where the input is
/// not a full compilation unit.
///
/// - The returned syntax tree is always rooted at [`SyntaxKind::ExpressionRoot`].
/// - Use [`JavaParseResult::expression`] to obtain the parsed expression node.
pub fn parse_java_expression(input: &str) -> JavaParseResult {
    Parser::new(input).parse_expression_root()
}

/// Parses `input` as a Java expression (not a full compilation unit).
///
/// This is the stable public entry point used by debugger integrations.
pub fn parse_expression(input: &str) -> JavaParseResult {
    let mut parser = Parser::new(input);
    parser.builder.start_node(SyntaxKind::ExpressionRoot.into());
    parser.eat_trivia();
    parser.parse_expression(0);
    parser.eat_trivia();
    if !parser.expect(SyntaxKind::Eof, "expected end of expression") {
        // Keep the parse lossless by consuming trailing junk tokens until EOF.
        parser.builder.start_node(SyntaxKind::Error.into());
        parser.recover_to(TokenSet::new(&[SyntaxKind::Eof]));
        parser.builder.finish_node();
        parser.expect(SyntaxKind::Eof, "expected end of expression");
    }
    parser.builder.finish_node();

    crate::util::sort_parse_errors(&mut parser.errors);

    JavaParseResult {
        green: parser.builder.finish(),
        errors: parser.errors,
    }
}

pub fn parse_java_block_fragment(text: &str, offset: u32) -> JavaFragmentParseResult {
    parse_java_fragment(text, offset, SyntaxKind::BlockFragment, |parser| {
        parser.parse_block(StatementContext::Normal);
    })
}

pub fn parse_java_statement_fragment(text: &str, offset: u32) -> JavaFragmentParseResult {
    parse_java_fragment(text, offset, SyntaxKind::StatementFragment, |parser| {
        parser.parse_statement(StatementContext::Normal);
    })
}

pub fn parse_java_expression_fragment(text: &str, offset: u32) -> JavaFragmentParseResult {
    parse_java_fragment(text, offset, SyntaxKind::ExpressionFragment, |parser| {
        parser.parse_expression(0);
    })
}

pub fn parse_java_class_member_fragment(text: &str, offset: u32) -> JavaFragmentParseResult {
    parse_java_fragment(text, offset, SyntaxKind::ClassMemberFragment, |parser| {
        parser.parse_class_member(SyntaxKind::ClassBody);
    })
}

fn parse_java_fragment(
    text: &str,
    offset: u32,
    root_kind: SyntaxKind,
    parse_inner: impl FnOnce(&mut Parser<'_>),
) -> JavaFragmentParseResult {
    let mut parse = Parser::new(text).parse_fragment(root_kind, parse_inner);
    for err in &mut parse.errors {
        err.range.start = err.range.start.saturating_add(offset);
        err.range.end = err.range.end.saturating_add(offset);
    }
    JavaFragmentParseResult { parse, offset }
}

pub(crate) fn parse_block_fragment(input: &str, stmt_ctx: StatementContext) -> JavaParseResult {
    let mut parser = Parser::new(input);
    parser.parse_block(stmt_ctx);
    crate::util::sort_parse_errors(&mut parser.errors);
    JavaParseResult {
        green: parser.builder.finish(),
        errors: parser.errors,
    }
}

fn parse_node_fragment(
    input: &str,
    root_kind: SyntaxKind,
    parse_inner: impl FnOnce(&mut Parser<'_>),
) -> JavaParseResult {
    let mut parser = Parser::new(input);
    parser.builder.start_node(root_kind.into());
    parse_inner(&mut parser);

    // Ensure fragment parsing is lossless: if the fragment parser stops early,
    // wrap remaining tokens in an `Error` node so everything in `input` is
    // represented in the syntax tree. We intentionally avoid bumping the EOF
    // token itself so we don't introduce `Eof` tokens into non-root nodes.
    parser.eat_trivia();
    if !parser.at(SyntaxKind::Eof) {
        parser.builder.start_node(SyntaxKind::Error.into());
        while !parser.at(SyntaxKind::Eof) {
            parser.bump_any();
        }
        parser.builder.finish_node(); // Error
    }

    parser.builder.finish_node(); // root_kind

    crate::util::sort_parse_errors(&mut parser.errors);

    JavaParseResult {
        green: parser.builder.finish(),
        errors: parser.errors,
    }
}

pub(crate) fn parse_switch_block_fragment(
    input: &str,
    stmt_ctx: StatementContext,
    switch_ctx: SwitchContext,
) -> JavaParseResult {
    let mut parser = Parser::new(input);
    parser.parse_switch_block(stmt_ctx, switch_ctx);
    crate::util::sort_parse_errors(&mut parser.errors);
    JavaParseResult {
        green: parser.builder.finish(),
        errors: parser.errors,
    }
}

pub(crate) fn parse_class_body_fragment(input: &str, body_kind: SyntaxKind) -> JavaParseResult {
    let mut parser = Parser::new(input);
    parser.parse_class_body(body_kind);
    crate::util::sort_parse_errors(&mut parser.errors);
    JavaParseResult {
        green: parser.builder.finish(),
        errors: parser.errors,
    }
}

pub(crate) fn parse_class_member_fragment(input: &str) -> JavaParseResult {
    let mut parser = Parser::new(input);
    // `parse_class_member` uses `start_node_at`, which requires an open parent
    // node. Wrap the member in a dummy compilation unit and extract it.
    parser
        .builder
        .start_node(SyntaxKind::CompilationUnit.into());
    parser.parse_class_member(SyntaxKind::ClassBody);
    parser.eat_trivia();
    parser.expect(SyntaxKind::Eof, "expected end of file");
    parser.builder.finish_node();

    let wrapper = parser.builder.finish();
    let member = wrapper
        .children()
        .find_map(|child| match child {
            rowan::NodeOrToken::Node(node) => Some(node.to_owned()),
            rowan::NodeOrToken::Token(_) => None,
        })
        .unwrap_or_else(|| wrapper.clone());

    crate::util::sort_parse_errors(&mut parser.errors);

    JavaParseResult {
        green: member,
        errors: parser.errors,
    }
}

pub(crate) fn parse_argument_list_fragment(input: &str) -> JavaParseResult {
    parse_node_fragment(input, SyntaxKind::ArgumentList, |parser| {
        parser.parse_argument_list_contents();
    })
}

pub(crate) fn parse_annotation_element_value_pair_list_fragment(input: &str) -> JavaParseResult {
    parse_node_fragment(
        input,
        SyntaxKind::AnnotationElementValuePairList,
        |parser| {
            parser.parse_annotation_element_value_pair_list_contents();
        },
    )
}

pub(crate) fn parse_parameter_list_fragment(input: &str) -> JavaParseResult {
    parse_node_fragment(input, SyntaxKind::ParameterList, |parser| {
        parser.parse_parameter_list_contents();
    })
}

pub(crate) fn parse_type_arguments_fragment(input: &str) -> JavaParseResult {
    parse_node_fragment(input, SyntaxKind::TypeArguments, |parser| {
        parser.parse_type_arguments_contents();
    })
}

pub(crate) fn parse_type_parameters_fragment(input: &str) -> JavaParseResult {
    parse_node_fragment(input, SyntaxKind::TypeParameters, |parser| {
        parser.parse_type_parameters_contents();
    })
}

struct Parser<'a> {
    input: &'a str,
    tokens: VecDeque<Token>,
    builder: GreenNodeBuilder<'static>,
    errors: Vec<ParseError>,
    last_non_trivia_end: u32,
    last_non_trivia_range: TextRange,
    last_non_trivia_kind: SyntaxKind,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        let (tokens, lex_errors) = lex_with_errors(input);
        let errors: Vec<ParseError> = lex_errors
            .into_iter()
            .map(|err| ParseError {
                message: err.message,
                range: err.range,
            })
            .collect();
        Self {
            input,
            tokens: VecDeque::from(tokens),
            builder: GreenNodeBuilder::new(),
            errors,
            last_non_trivia_end: 0,
            last_non_trivia_range: TextRange { start: 0, end: 0 },
            last_non_trivia_kind: SyntaxKind::Eof,
        }
    }

    fn parse(mut self) -> JavaParseResult {
        self.builder.start_node(SyntaxKind::CompilationUnit.into());
        self.eat_trivia();

        if self.at(SyntaxKind::PackageKw) || self.at_annotated_package_decl_start() {
            self.parse_package_decl();
        }

        while self.at(SyntaxKind::ImportKw) {
            self.parse_import_decl();
        }

        let mut parsed_module_decl = false;
        while !self.at(SyntaxKind::Eof) {
            let before = self.tokens.len();

            if !parsed_module_decl && self.at_module_decl_start() {
                self.parse_module_declaration();
                parsed_module_decl = true;

                if !self.at(SyntaxKind::Eof) {
                    self.builder.start_node(SyntaxKind::Error.into());
                    self.error_here("unexpected tokens after module declaration");
                    self.recover_to(TokenSet::new(&[SyntaxKind::Eof]));
                    self.builder.finish_node();
                }
            } else if self.at_type_decl_start() {
                self.parse_type_declaration();
            } else {
                self.recover_top_level();
            }

            self.force_progress(before, TOP_LEVEL_RECOVERY);
            if parsed_module_decl {
                // Module compilation units contain exactly one module declaration. We recover
                // any trailing junk to EOF above and stop parsing further top-level items.
                break;
            }
        }

        self.eat_trivia();
        self.expect(SyntaxKind::Eof, "expected end of file");
        self.builder.finish_node();

        crate::util::sort_parse_errors(&mut self.errors);

        JavaParseResult {
            green: self.builder.finish(),
            errors: self.errors,
        }
    }

    fn parse_expression_root(mut self) -> JavaParseResult {
        self.builder.start_node(SyntaxKind::ExpressionRoot.into());
        self.eat_trivia();
        self.parse_expression(0);
        self.eat_trivia();

        if self.at(SyntaxKind::Semicolon) {
            self.bump();
        }

        self.eat_trivia();

        if !self.at(SyntaxKind::Eof) {
            self.builder.start_node(SyntaxKind::Error.into());
            self.error_here("unexpected token after expression");
            self.recover_to(TokenSet::new(&[SyntaxKind::Eof]));
            self.builder.finish_node();
        }

        self.eat_trivia();
        self.expect(SyntaxKind::Eof, "expected end of expression");
        self.builder.finish_node();

        crate::util::sort_parse_errors(&mut self.errors);

        JavaParseResult {
            green: self.builder.finish(),
            errors: self.errors,
        }
    }

    fn parse_fragment(
        mut self,
        root_kind: SyntaxKind,
        parse_inner: impl FnOnce(&mut Parser<'a>),
    ) -> JavaParseResult {
        self.builder.start_node(root_kind.into());
        self.eat_trivia();

        parse_inner(&mut self);

        // Ensure we build a lossless tree: if the fragment parser stops early,
        // wrap remaining tokens in an `Error` node so everything is represented
        // in the syntax tree.
        self.eat_trivia();
        if !self.at(SyntaxKind::Eof) {
            self.builder.start_node(SyntaxKind::Error.into());
            while !self.at(SyntaxKind::Eof) {
                self.bump_any();
            }
            self.builder.finish_node();
        }

        self.eat_trivia();
        self.expect(SyntaxKind::Eof, "expected end of file");
        self.builder.finish_node();

        crate::util::sort_parse_errors(&mut self.errors);

        JavaParseResult {
            green: self.builder.finish(),
            errors: self.errors,
        }
    }

    fn parse_package_decl(&mut self) {
        self.builder
            .start_node(SyntaxKind::PackageDeclaration.into());
        while self.at(SyntaxKind::At) && self.nth(1) != Some(SyntaxKind::InterfaceKw) {
            self.parse_annotation();
        }
        self.expect(SyntaxKind::PackageKw, "expected `package`");
        self.parse_name();
        self.expect(
            SyntaxKind::Semicolon,
            "expected `;` after package declaration",
        );
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
        self.expect(
            SyntaxKind::Semicolon,
            "expected `;` after import declaration",
        );
        self.builder.finish_node();
    }

    fn parse_module_declaration(&mut self) {
        self.builder
            .start_node(SyntaxKind::ModuleDeclaration.into());

        // `module-info.java` supports annotations on the module declaration.
        while self.at(SyntaxKind::At) && self.nth(1) != Some(SyntaxKind::InterfaceKw) {
            self.parse_annotation();
        }

        // `open module ... { ... }`
        if self.at(SyntaxKind::OpenKw) && self.nth(1) == Some(SyntaxKind::ModuleKw) {
            self.bump(); // `open`
        }

        self.expect(SyntaxKind::ModuleKw, "expected `module`");
        self.parse_name();

        self.builder.start_node(SyntaxKind::ModuleBody.into());
        self.expect(SyntaxKind::LBrace, "expected `{` for module body");
        while !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::StringTemplateExprEnd)
            && !self.at(SyntaxKind::Eof)
        {
            let before = self.tokens.len();
            self.parse_module_directive();
            self.force_progress(before, MODULE_DIRECTIVE_RECOVERY);
        }
        self.expect(SyntaxKind::RBrace, "expected `}` to close module body");
        self.builder.finish_node(); // ModuleBody

        self.builder.finish_node(); // ModuleDeclaration
    }

    fn parse_module_directive(&mut self) {
        let checkpoint = self.builder.checkpoint();
        self.builder
            .start_node_at(checkpoint, SyntaxKind::ModuleDirective.into());

        match self.current() {
            SyntaxKind::RequiresKw => self.parse_requires_directive(),
            SyntaxKind::ExportsKw => self.parse_exports_directive(),
            SyntaxKind::OpensKw => self.parse_opens_directive(),
            SyntaxKind::UsesKw => self.parse_uses_directive(),
            SyntaxKind::ProvidesKw => self.parse_provides_directive(),
            SyntaxKind::Semicolon => {
                self.builder.start_node(SyntaxKind::Error.into());
                self.error_here("unexpected `;` in module body");
                self.bump();
                self.builder.finish_node();
            }
            _ => {
                self.builder.start_node(SyntaxKind::Error.into());
                self.error_here("expected module directive");
                self.recover_to_module_directive_boundary();
                self.builder.finish_node();
            }
        }

        self.builder.finish_node(); // ModuleDirective
    }

    fn parse_requires_directive(&mut self) {
        self.builder
            .start_node(SyntaxKind::RequiresDirective.into());
        self.expect(SyntaxKind::RequiresKw, "expected `requires`");

        // `requires transitive static foo.bar;`
        while matches!(
            self.current(),
            SyntaxKind::TransitiveKw | SyntaxKind::StaticKw
        ) {
            self.bump();
        }

        self.parse_name();
        if !self.expect(
            SyntaxKind::Semicolon,
            "expected `;` after requires directive",
        ) {
            self.recover_to_module_directive_boundary();
        }
        self.builder.finish_node();
    }

    fn parse_exports_directive(&mut self) {
        self.builder.start_node(SyntaxKind::ExportsDirective.into());
        self.expect(SyntaxKind::ExportsKw, "expected `exports`");
        self.parse_name();

        if self.at(SyntaxKind::ToKw) {
            self.bump();
            self.parse_name();
            while self.at(SyntaxKind::Comma) {
                self.bump();
                self.parse_name();
            }
        }

        if !self.expect(
            SyntaxKind::Semicolon,
            "expected `;` after exports directive",
        ) {
            self.recover_to_module_directive_boundary();
        }
        self.builder.finish_node();
    }

    fn parse_opens_directive(&mut self) {
        self.builder.start_node(SyntaxKind::OpensDirective.into());
        self.expect(SyntaxKind::OpensKw, "expected `opens`");
        self.parse_name();

        if self.at(SyntaxKind::ToKw) {
            self.bump();
            self.parse_name();
            while self.at(SyntaxKind::Comma) {
                self.bump();
                self.parse_name();
            }
        }

        if !self.expect(SyntaxKind::Semicolon, "expected `;` after opens directive") {
            self.recover_to_module_directive_boundary();
        }
        self.builder.finish_node();
    }

    fn parse_uses_directive(&mut self) {
        self.builder.start_node(SyntaxKind::UsesDirective.into());
        self.expect(SyntaxKind::UsesKw, "expected `uses`");
        self.parse_name();
        if !self.expect(SyntaxKind::Semicolon, "expected `;` after uses directive") {
            self.recover_to_module_directive_boundary();
        }
        self.builder.finish_node();
    }

    fn parse_provides_directive(&mut self) {
        self.builder
            .start_node(SyntaxKind::ProvidesDirective.into());
        self.expect(SyntaxKind::ProvidesKw, "expected `provides`");
        self.parse_name();
        self.expect(SyntaxKind::WithKw, "expected `with` in provides directive");
        self.parse_name();
        while self.at(SyntaxKind::Comma) {
            self.bump();
            self.parse_name();
        }
        if !self.expect(
            SyntaxKind::Semicolon,
            "expected `;` after provides directive",
        ) {
            self.recover_to_module_directive_boundary();
        }
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
            SyntaxKind::ClassKw => self.parse_class_decl(checkpoint),
            SyntaxKind::InterfaceKw => self.parse_interface_decl(checkpoint),
            SyntaxKind::EnumKw => self.parse_enum_decl(checkpoint),
            SyntaxKind::RecordKw => self.parse_record_decl(checkpoint),
            SyntaxKind::Semicolon => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::EmptyDeclaration.into());
                self.bump();
                self.builder.finish_node();
            }
            _ => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::Error.into());
                self.error_here("expected type declaration");
                self.recover_to(TYPE_DECL_RECOVERY);
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

    fn parse_class_decl(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::ClassDeclaration.into());
        // `class` keyword already in current()
        self.bump();
        self.expect_ident_like("expected name");
        self.parse_type_parameters_opt();

        if self.at(SyntaxKind::ExtendsKw) {
            self.parse_extends_clause(false);
        }
        if self.at(SyntaxKind::ImplementsKw) {
            self.parse_implements_clause();
        }
        self.parse_permits_clause_opt();

        self.parse_class_body(SyntaxKind::ClassBody);
        self.builder.finish_node();
    }

    fn parse_interface_decl(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::InterfaceDeclaration.into());
        // `interface` keyword already in current()
        self.bump();
        self.expect_ident_like("expected name");
        self.parse_type_parameters_opt();

        if self.at(SyntaxKind::ExtendsKw) {
            self.parse_extends_clause(true);
        }

        if self.at(SyntaxKind::ImplementsKw) {
            // Interfaces have `extends`, not `implements`.
            self.error_here("interfaces cannot have an `implements` clause");
            self.parse_implements_clause();
        }

        self.parse_permits_clause_opt();

        self.parse_class_body(SyntaxKind::InterfaceBody);
        self.builder.finish_node();
    }

    fn parse_type_parameters_opt(&mut self) {
        if !self.at(SyntaxKind::Less) {
            return;
        }
        self.parse_type_parameters();
    }

    fn parse_type_parameters(&mut self) {
        self.builder.start_node(SyntaxKind::TypeParameters.into());
        self.parse_type_parameters_contents();
        self.builder.finish_node(); // TypeParameters
    }

    fn parse_type_parameters_contents(&mut self) {
        self.expect(SyntaxKind::Less, "expected `<`");

        let mut parsed_any = false;
        while !self.at_type_parameters_end(parsed_any) {
            let before = self.tokens.len();
            self.builder.start_node(SyntaxKind::TypeParameter.into());
            self.eat_trivia();

            if self.at_ident_like() {
                self.bump();
                if self.at(SyntaxKind::ExtendsKw) {
                    self.bump();
                    self.parse_type_parameter_bound();
                    while self.at(SyntaxKind::Amp) {
                        self.bump();
                        self.parse_type_parameter_bound();
                    }
                }
            } else {
                self.builder.start_node(SyntaxKind::Error.into());
                self.error_here("expected type parameter name");
                self.recover_to_including_angles(TYPE_PARAMETER_RECOVERY);
                self.builder.finish_node(); // Error
            }

            self.builder.finish_node(); // TypeParameter
            parsed_any = true;
            self.force_progress(before, TYPE_PARAMETER_RECOVERY);

            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }

            if self.at_type_parameters_end(parsed_any) {
                break;
            }

            self.error_here("expected `,` or `>` after type parameter");
        }
        self.expect_gt();
    }

    fn at_type_parameters_end(&mut self, parsed_any: bool) -> bool {
        match self.current() {
            SyntaxKind::Greater
            | SyntaxKind::RightShift
            | SyntaxKind::UnsignedRightShift
            | SyntaxKind::LParen
            | SyntaxKind::LBrace
            | SyntaxKind::Semicolon
            | SyntaxKind::RParen
            | SyntaxKind::RBrace
            | SyntaxKind::StringTemplateExprEnd
            | SyntaxKind::Eof => true,
            SyntaxKind::ExtendsKw
            | SyntaxKind::ImplementsKw
            | SyntaxKind::PermitsKw
            | SyntaxKind::VoidKw
            | SyntaxKind::BooleanKw
            | SyntaxKind::ByteKw
            | SyntaxKind::ShortKw
            | SyntaxKind::IntKw
            | SyntaxKind::LongKw
            | SyntaxKind::CharKw
            | SyntaxKind::FloatKw
            | SyntaxKind::DoubleKw => parsed_any,
            _ => false,
        }
    }

    fn parse_type_parameter_bound(&mut self) {
        if self.at_type_start() {
            self.parse_type();
            return;
        }

        self.builder.start_node(SyntaxKind::Error.into());
        self.error_here("expected type bound");
        self.recover_to_including_angles(TokenSet::new(&[
            SyntaxKind::Amp,
            SyntaxKind::Comma,
            SyntaxKind::Greater,
            SyntaxKind::RightShift,
            SyntaxKind::UnsignedRightShift,
            SyntaxKind::LParen,
            SyntaxKind::LBrace,
            SyntaxKind::Semicolon,
            SyntaxKind::RParen,
            SyntaxKind::RBrace,
            SyntaxKind::StringTemplateExprEnd,
            SyntaxKind::Eof,
        ]));
        self.builder.finish_node(); // Error
    }

    fn parse_extends_clause(&mut self, allow_multiple: bool) {
        self.builder.start_node(SyntaxKind::ExtendsClause.into());
        self.expect(SyntaxKind::ExtendsKw, "expected `extends`");
        if self.at_type_start() {
            self.parse_type();
        } else {
            self.error_here("expected type after `extends`");
        }
        if self.at(SyntaxKind::Comma) && !allow_multiple {
            self.error_here("classes can only extend a single type");
        }
        while self.at(SyntaxKind::Comma) {
            self.bump();
            if self.at_type_start() {
                self.parse_type();
            } else {
                self.error_here("expected type after `,`");
                break;
            }
        }
        self.builder.finish_node();
    }

    fn parse_implements_clause(&mut self) {
        self.builder.start_node(SyntaxKind::ImplementsClause.into());
        self.expect(SyntaxKind::ImplementsKw, "expected `implements`");
        if self.at_type_start() {
            self.parse_type();
        } else {
            self.error_here("expected type after `implements`");
        }
        while self.at(SyntaxKind::Comma) {
            self.bump();
            if self.at_type_start() {
                self.parse_type();
            } else {
                self.error_here("expected type after `,`");
                break;
            }
        }
        self.builder.finish_node();
    }

    fn parse_permits_clause_opt(&mut self) {
        if !self.at(SyntaxKind::PermitsKw) {
            return;
        }
        self.builder.start_node(SyntaxKind::PermitsClause.into());
        self.bump();
        if self.at_type_start() {
            self.parse_type();
        } else {
            self.error_here("expected type after `permits`");
        }
        while self.at(SyntaxKind::Comma) {
            self.bump();
            if self.at_type_start() {
                self.parse_type();
            } else {
                self.error_here("expected type after `,`");
                break;
            }
        }
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
        while !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::StringTemplateExprEnd)
            && !self.at(SyntaxKind::Eof)
        {
            if self.at_ident_like() {
                self.builder.start_node(SyntaxKind::EnumConstant.into());
                self.bump();
                // Optional arguments.
                if self.at(SyntaxKind::LParen) {
                    self.parse_argument_list();
                }
                // Optional class body for enum constant: `A { ... }`.
                if self.at(SyntaxKind::LBrace) {
                    self.parse_class_body(SyntaxKind::ClassBody);
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
            while !self.at(SyntaxKind::RBrace)
                && !self.at(SyntaxKind::StringTemplateExprEnd)
                && !self.at(SyntaxKind::Eof)
            {
                self.parse_class_member(SyntaxKind::EnumBody);
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
        self.parse_type_parameters_opt();
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
        self.parse_permits_clause_opt();
        self.parse_class_body(SyntaxKind::RecordBody);
        self.builder.finish_node();
    }

    fn parse_class_body(&mut self, body_kind: SyntaxKind) {
        self.builder.start_node(body_kind.into());
        self.expect(SyntaxKind::LBrace, "expected `{`");

        let body_indent = if self.last_non_trivia_kind == SyntaxKind::LBrace {
            self.line_indent(self.last_non_trivia_range.start)
        } else {
            let start = self.current_range().start;
            self.line_indent(start)
        };

        while !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::StringTemplateExprEnd)
            && !self.at(SyntaxKind::Eof)
        {
            let start = self.current_range().start;
            let (indent, is_first) = self.line_indent_and_is_first_token(start);
            if is_first
                && indent <= body_indent
                && matches!(
                    self.current(),
                    SyntaxKind::PackageKw
                        | SyntaxKind::ImportKw
                        | SyntaxKind::ClassKw
                        | SyntaxKind::InterfaceKw
                        | SyntaxKind::EnumKw
                        | SyntaxKind::RecordKw
                        | SyntaxKind::At
                        | SyntaxKind::PublicKw
                        | SyntaxKind::PrivateKw
                        | SyntaxKind::ProtectedKw
                        | SyntaxKind::StaticKw
                        | SyntaxKind::FinalKw
                        | SyntaxKind::AbstractKw
                        | SyntaxKind::SealedKw
                        | SyntaxKind::NonSealedKw
                        | SyntaxKind::StrictfpKw
                )
            {
                // Likely a missing `}` for the current class body; stop here so we can
                // continue parsing sibling declarations.
                break;
            }

            let before = self.tokens.len();
            self.parse_class_member(body_kind);
            self.force_progress(before, MEMBER_RECOVERY);
        }

        if self.at(SyntaxKind::RBrace) {
            let start = self.current_range().start;
            let (indent, is_first) = self.line_indent_and_is_first_token(start);
            if is_first && indent < body_indent {
                self.error_here("expected `}` to close class body");
                self.insert_missing(SyntaxKind::MissingRBrace);
                self.builder.finish_node();
                return;
            }
        }

        self.expect(SyntaxKind::RBrace, "expected `}` to close class body");
        self.builder.finish_node();
    }

    fn parse_class_member(&mut self, body_kind: SyntaxKind) {
        let checkpoint = self.builder.checkpoint();
        self.parse_modifiers();
        self.parse_type_parameters_opt();

        // After parsing an optional type-parameter list, malformed member declarations can
        // still contain additional `<...>` sequences. Consume a type-argument list for recovery
        // so we don't get stuck on nested angle brackets.
        if self.at(SyntaxKind::Less) {
            self.parse_type_arguments();
        }

        // Initializer blocks.
        if self.at(SyntaxKind::LBrace) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::InitializerBlock.into());
            self.parse_block_with_recovery(StatementContext::Normal, MEMBER_RECOVERY);
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
            SyntaxKind::ClassKw
                | SyntaxKind::InterfaceKw
                | SyntaxKind::EnumKw
                | SyntaxKind::RecordKw
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
            self.parse_block_with_recovery(StatementContext::Normal, MEMBER_RECOVERY);
            self.builder.finish_node();
            return;
        }

        // Record compact constructor: `Ident '{' ... }`.
        //
        // Example (JEP 395):
        //   record Point(int x, int y) { Point { ... } }
        //
        // This is intentionally checked before the `at_type_start` branch below because
        // `Ident '{'` is otherwise misinterpreted as a type+member-name prefix and triggers
        // error recovery across the rest of the record body.
        if body_kind == SyntaxKind::RecordBody
            && self.at_ident_like()
            && matches!(
                self.nth(1),
                Some(SyntaxKind::LBrace) | Some(SyntaxKind::ThrowsKw)
            )
        {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::CompactConstructorDeclaration.into());
            self.bump(); // name
            self.parse_throws_opt();
            self.parse_block_with_recovery(StatementContext::Normal, MEMBER_RECOVERY);
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
            if self.at(SyntaxKind::DefaultKw) {
                self.parse_annotation_element_default();
                self.expect(SyntaxKind::Semicolon, "expected `;` after declaration");
            } else if self.at(SyntaxKind::LBrace) {
                self.parse_block_with_recovery(StatementContext::Normal, MEMBER_RECOVERY);
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
                if self.at(SyntaxKind::DefaultKw) {
                    self.parse_annotation_element_default();
                    self.expect(SyntaxKind::Semicolon, "expected `;` after declaration");
                } else if self.at(SyntaxKind::LBrace) {
                    self.parse_block_with_recovery(StatementContext::Normal, MEMBER_RECOVERY);
                } else {
                    self.expect(SyntaxKind::Semicolon, "expected `;` or method body");
                }
                self.builder.finish_node();
            } else {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::FieldDeclaration.into());
                self.parse_variable_declarator_list(false);
                self.expect(
                    SyntaxKind::Semicolon,
                    "expected `;` after field declaration",
                );
                self.builder.finish_node();
            }
            return;
        }

        // Give up: recover.
        self.builder
            .start_node_at(checkpoint, SyntaxKind::Error.into());
        self.error_here("unexpected token in class body");
        self.recover_to_class_member_boundary();
        self.builder.finish_node();
    }

    fn parse_throws_opt(&mut self) {
        if !self.at(SyntaxKind::ThrowsKw) {
            return;
        }
        self.builder.start_node(SyntaxKind::ThrowsClause.into());
        self.bump();
        self.parse_type();
        while self.at(SyntaxKind::Comma) {
            self.bump();
            self.parse_type();
        }
        self.builder.finish_node();
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
            self.parse_annotation_element_value_pair_list();
        }
        self.builder.finish_node();
    }

    fn parse_annotation_element_value_pair_list(&mut self) {
        self.builder
            .start_node(SyntaxKind::AnnotationElementValuePairList.into());
        self.parse_annotation_element_value_pair_list_contents();
        self.builder.finish_node(); // AnnotationElementValuePairList
    }

    fn parse_annotation_element_value_pair_list_contents(&mut self) {
        self.expect(SyntaxKind::LParen, "expected `(`");

        self.eat_trivia();
        if self.at(SyntaxKind::RParen) {
            self.bump();
            return;
        }

        // Disambiguate normal annotations (`name = value`) from single-element annotations
        // (`@Anno(expr)`); if there's no `=`, treat the contents as a single element value.
        if self.at_ident_like() && self.nth(1) == Some(SyntaxKind::Eq) {
            while !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
                self.eat_trivia();

                if self.at(SyntaxKind::Comma) {
                    // `@Anno(, x = 1)` – don't get stuck.
                    self.error_here("expected annotation element-value pair");
                    self.bump();
                    continue;
                }

                if !self.at_ident_like() {
                    // Likely hit the next declaration while typing `@Anno(`.
                    self.error_here("expected annotation element-value pair");
                    break;
                }

                self.parse_annotation_element_value_pair();

                self.eat_trivia();
                if self.at(SyntaxKind::Comma) {
                    self.bump();
                    // Trailing comma is allowed.
                    continue;
                }
                break;
            }
        } else {
            while !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
                self.eat_trivia();
                if self.at(SyntaxKind::Comma) {
                    // `@Anno(, x)` – treat as a missing element value.
                    self.error_here("expected annotation element value");
                    self.bump();
                    continue;
                }

                if !self.can_start_annotation_element_value() {
                    // Common recovery: `@Anno(` followed by a declaration.
                    self.error_here("expected annotation element value");
                    break;
                }

                self.parse_annotation_element_value();
                break;
            }

            self.eat_trivia();
            if self.at(SyntaxKind::Comma) {
                // Tolerate a trailing comma for partial code.
                self.bump();
            }
        }

        self.expect(SyntaxKind::RParen, "expected `)`");
    }

    fn parse_annotation_element_value_pair(&mut self) {
        self.builder
            .start_node(SyntaxKind::AnnotationElementValuePair.into());

        if self.at_ident_like() {
            self.bump();
        } else {
            self.error_here("expected annotation argument name");
        }

        if self.at(SyntaxKind::Eq) {
            self.bump();
            if matches!(
                self.current(),
                SyntaxKind::Comma | SyntaxKind::RParen | SyntaxKind::Eof
            ) {
                // `@Anno(x = )` – keep the pair but leave the value empty.
                self.error_here("expected annotation element value");
                self.builder
                    .start_node(SyntaxKind::AnnotationElementValue.into());
                self.builder.finish_node();
            } else {
                self.parse_annotation_element_value();
            }
        } else {
            // Missing `=`; treat it as a malformed pair and try to recover the value.
            self.error_here("expected `=` after annotation argument name");
            if !matches!(
                self.current(),
                SyntaxKind::Comma | SyntaxKind::RParen | SyntaxKind::Eof
            ) {
                self.parse_annotation_element_value();
            }
        }

        self.builder.finish_node(); // AnnotationElementValuePair
    }

    fn parse_annotation_element_value(&mut self) {
        self.builder
            .start_node(SyntaxKind::AnnotationElementValue.into());
        self.eat_trivia();

        match self.current() {
            SyntaxKind::At => {
                // Nested annotation.
                if self.nth(1) == Some(SyntaxKind::InterfaceKw) {
                    // `@interface` isn't valid here; avoid descending into a type decl.
                    self.error_here("expected annotation element value");
                } else {
                    self.parse_annotation();
                }
            }
            SyntaxKind::LBrace => {
                self.parse_annotation_element_value_array_initializer();
            }
            _ if self.can_start_expression_here() => {
                self.parse_expression(0);
            }
            _ => {
                self.error_here("expected annotation element value");
            }
        }

        self.builder.finish_node(); // AnnotationElementValue
    }

    fn can_start_annotation_element_value(&mut self) -> bool {
        match self.current() {
            SyntaxKind::At => self.nth(1) != Some(SyntaxKind::InterfaceKw),
            SyntaxKind::LBrace => true,
            _ => self.can_start_expression_here(),
        }
    }

    fn can_start_expression_here(&mut self) -> bool {
        let kind = self.current();
        if can_start_expression(kind) {
            return true;
        }
        if is_primitive_type(kind) {
            return self.at_primitive_type_suffix_start();
        }
        kind == SyntaxKind::VoidKw && self.at_primitive_class_literal_start()
    }

    fn parse_annotation_element_value_array_initializer(&mut self) {
        self.builder
            .start_node(SyntaxKind::AnnotationElementValueArrayInitializer.into());
        self.expect(
            SyntaxKind::LBrace,
            "expected `{` in annotation array initializer",
        );

        while !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::StringTemplateExprEnd)
            && !self.at(SyntaxKind::Eof)
        {
            self.eat_trivia();

            if self.at(SyntaxKind::Comma) {
                // `@Anno({, "x"})` – treat as a missing element value.
                self.error_here("expected annotation element value");
                self.bump();
                continue;
            }

            if !self.can_start_annotation_element_value() {
                // Likely hit the end of the array initializer while typing.
                self.error_here("expected annotation element value");
                break;
            }

            self.parse_annotation_element_value();

            self.eat_trivia();
            if self.at(SyntaxKind::Comma) {
                self.bump();
                // Trailing comma is allowed.
                continue;
            }
            break;
        }

        self.expect(
            SyntaxKind::RBrace,
            "expected `}` to close annotation array initializer",
        );
        self.builder.finish_node(); // AnnotationElementValueArrayInitializer
    }

    fn parse_name(&mut self) {
        self.builder.start_node(SyntaxKind::Name.into());
        self.expect_ident_like("expected name");
        while self.at(SyntaxKind::Dot)
            && self
                .nth(1)
                .is_some_and(|k| k.is_identifier_like() || k == SyntaxKind::Star)
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
        self.parse_parameter_list_contents();
        self.builder.finish_node();
    }

    fn parse_argument_list(&mut self) {
        self.builder.start_node(SyntaxKind::ArgumentList.into());
        self.parse_argument_list_contents();
        self.builder.finish_node();
    }

    fn parse_parameter_list_contents(&mut self) {
        self.expect(SyntaxKind::LParen, "expected `(`");
        while !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
            self.builder.start_node(SyntaxKind::Parameter.into());
            self.parse_modifiers();
            if self.at_type_start() {
                self.parse_type();
                // Permit type-use annotations on varargs ellipsis (e.g. `String @A ... args`).
                while self.at(SyntaxKind::At) && self.nth(1) != Some(SyntaxKind::InterfaceKw) {
                    self.parse_annotation();
                }
            } else {
                self.error_here("expected parameter type");
            }
            if self.at(SyntaxKind::Ellipsis) {
                // Varargs parameter: `String... args`.
                self.bump();
            }
            self.expect_ident_like("expected parameter name");
            // Support Java's `var x[]` / `String... args[]` style dims.
            while self.at(SyntaxKind::LBracket) && self.nth(1) == Some(SyntaxKind::RBracket) {
                self.bump();
                self.bump();
            }
            self.builder.finish_node(); // Parameter

            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect(SyntaxKind::RParen, "expected `)`");
    }

    fn parse_argument_list_contents(&mut self) {
        self.expect(SyntaxKind::LParen, "expected `(`");
        while !self.at(SyntaxKind::RParen) && !self.at(SyntaxKind::Eof) {
            self.eat_trivia();
            if self.at(SyntaxKind::Comma) {
                // `f(, x)` – don't get stuck; treat as a missing expression.
                self.error_here("expected argument expression");
                self.bump();
                continue;
            }
            if !self.can_start_expression_here() {
                // Common during typing: `@Anno(` followed by the next declaration.
                self.error_here("expected argument expression");
                break;
            }
            self.parse_expression(0);
            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect(SyntaxKind::RParen, "expected `)`");
    }

    fn parse_annotation_element_default(&mut self) {
        self.builder.start_node(SyntaxKind::DefaultValue.into());
        self.expect(SyntaxKind::DefaultKw, "expected `default`");
        if matches!(
            self.current(),
            SyntaxKind::Semicolon
                | SyntaxKind::RBrace
                | SyntaxKind::StringTemplateExprEnd
                | SyntaxKind::Eof
        ) {
            self.error_here("expected default value");
            // Preserve a stable subtree shape for downstream consumers.
            self.builder
                .start_node(SyntaxKind::AnnotationElementValue.into());
            self.builder.finish_node();
        } else {
            self.parse_annotation_element_value();
        }
        self.builder.finish_node();
    }

    fn parse_block(&mut self, stmt_ctx: StatementContext) {
        self.parse_block_with_recovery(stmt_ctx, TokenSet::new(&[]));
    }

    fn parse_block_with_recovery(&mut self, stmt_ctx: StatementContext, recovery: TokenSet) {
        self.builder.start_node(SyntaxKind::Block.into());
        self.expect(SyntaxKind::LBrace, "expected `{`");

        let (block_indent, _brace_is_first_token_on_line) =
            if self.last_non_trivia_kind == SyntaxKind::LBrace {
                let start = self.last_non_trivia_range.start;
                let (indent, is_first) = self.line_indent_and_is_first_token(start);
                (indent, is_first)
            } else {
                // `{` is missing; fall back to the indent of the first token in the block.
                let start = self.current_range().start;
                (self.line_indent(start), false)
            };
        // `block_indent` is a best-effort heuristic used for error recovery (to avoid consuming
        // outer-closing braces when an inner `}` is missing). However, it can be misleading when
        // the line containing `{` is itself over-indented (e.g. multi-line headers or simply
        // mis-indented code). Track whether we've already seen a statement start that is less
        // indented than `block_indent`; in that case, indentation is not reliable for deciding
        // whether a closing brace belongs to this block.
        let mut saw_statement_less_indented_than_block = false;

        // Indentation-based recovery in Java is necessarily heuristic, since indentation is not
        // syntactically meaningful. We use it as a best-effort signal to recover from missing
        // braces (common during edits), but we must avoid producing parse errors for valid yet
        // misindented code.
        //
        // If the first token inside the block is *less indented* than the line that introduced the
        // block, treat indentation as unreliable and disable the "outdented `}` belongs to an
        // outer construct" recovery below.
        let allow_outdent_recovery = {
            let start = self.current_range().start;
            let (indent, is_first) = self.line_indent_and_is_first_token(start);
            !(is_first && indent < block_indent)
        };

        while !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::StringTemplateExprEnd)
            && !self.at(SyntaxKind::Eof)
        {
            let start = self.current_range().start;
            let (indent, is_first) = self.line_indent_and_is_first_token(start);
            if is_first && indent <= block_indent && recovery.contains(self.current()) {
                break;
            }
            if is_first && indent < block_indent {
                saw_statement_less_indented_than_block = true;
            }

            let before = self.tokens.len();
            self.parse_statement(stmt_ctx);
            self.force_progress(before, STMT_RECOVERY);
        }

        if allow_outdent_recovery && self.at(SyntaxKind::RBrace) {
            let start = self.current_range().start;
            let (indent, is_first) = self.line_indent_and_is_first_token(start);
            if is_first && indent < block_indent && !saw_statement_less_indented_than_block {
                // A closing brace that is less-indented than the block start is very likely
                // meant for an outer construct. Insert a missing `}` and let the caller
                // consume the real one.
                self.error_here("expected `}` to close block");
                self.insert_missing(SyntaxKind::MissingRBrace);
                self.builder.finish_node();
                return;
            }
        }

        self.expect(SyntaxKind::RBrace, "expected `}` to close block");
        self.builder.finish_node();
    }

    fn parse_statement(&mut self, stmt_ctx: StatementContext) {
        self.eat_trivia();
        let checkpoint = self.builder.checkpoint();
        if self.at_ident_like() && self.nth(1) == Some(SyntaxKind::Colon) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::LabeledStatement.into());
            self.bump(); // label
            self.expect(SyntaxKind::Colon, "expected `:` after label");
            self.parse_statement(stmt_ctx);
            self.builder.finish_node();
            return;
        }

        if self.at_explicit_constructor_invocation_start() {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::ExplicitConstructorInvocation.into());
            self.parse_expression(0);
            self.expect(
                SyntaxKind::Semicolon,
                "expected `;` after constructor invocation",
            );
            self.builder.finish_node();
            return;
        }
        match self.current() {
            SyntaxKind::LBrace => self.parse_block(stmt_ctx),
            SyntaxKind::YieldKw if stmt_ctx == StatementContext::SwitchExpression => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::YieldStatement.into());
                self.bump();
                if self.at(SyntaxKind::Semicolon) {
                    self.error_here("expected expression after `yield`");
                } else {
                    self.parse_expression(0);
                }
                self.expect(SyntaxKind::Semicolon, "expected `;` after yield");
                self.builder.finish_node();
            }
            SyntaxKind::IfKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::IfStatement.into());
                self.bump();
                self.expect(SyntaxKind::LParen, "expected `(` after if");
                self.parse_expression(0);
                self.expect(SyntaxKind::RParen, "expected `)`");
                if self.at(SyntaxKind::LBrace) {
                    self.parse_block_with_recovery(stmt_ctx, IF_THEN_BLOCK_RECOVERY);
                } else {
                    self.parse_statement(stmt_ctx);
                }
                if self.at(SyntaxKind::ElseKw) {
                    self.bump();
                    self.parse_statement(stmt_ctx);
                }
                self.builder.finish_node();
            }
            SyntaxKind::SwitchKw => self.parse_switch_statement(checkpoint),
            SyntaxKind::ForKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::ForStatement.into());
                self.bump();
                self.builder.start_node(SyntaxKind::ForHeader.into());
                self.expect(SyntaxKind::LParen, "expected `(` after for");
                self.parse_for_header_contents();
                self.expect(SyntaxKind::RParen, "expected `)` after for header");
                self.builder.finish_node(); // ForHeader
                self.parse_statement(stmt_ctx);
                self.builder.finish_node();
            }
            SyntaxKind::WhileKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::WhileStatement.into());
                self.bump();
                self.expect(SyntaxKind::LParen, "expected `(` after while");
                self.parse_expression(0);
                self.expect(SyntaxKind::RParen, "expected `)`");
                self.parse_statement(stmt_ctx);
                self.builder.finish_node();
            }
            SyntaxKind::DoKw => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::DoWhileStatement.into());
                self.bump();
                if self.at(SyntaxKind::LBrace) {
                    self.parse_block_with_recovery(stmt_ctx, DO_BODY_RECOVERY);
                } else {
                    self.parse_statement(stmt_ctx);
                }
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
                self.parse_block(stmt_ctx);
                self.builder.finish_node();
            }
            SyntaxKind::TryKw => self.parse_try_statement(stmt_ctx, checkpoint),
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
                if self.at_local_type_decl_start() {
                    self.builder.start_node_at(
                        checkpoint,
                        SyntaxKind::LocalTypeDeclarationStatement.into(),
                    );
                    let decl_checkpoint = self.builder.checkpoint();
                    self.parse_modifiers();
                    self.parse_type_declaration_inner(decl_checkpoint);
                    self.builder.finish_node();
                } else if self.at_local_var_decl_start() {
                    self.builder.start_node_at(
                        checkpoint,
                        SyntaxKind::LocalVariableDeclarationStatement.into(),
                    );
                    self.parse_modifiers();
                    self.parse_type();
                    self.parse_variable_declarator_list(true);
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

    fn parse_switch_statement(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::SwitchStatement.into());
        self.bump();
        self.expect(SyntaxKind::LParen, "expected `(` after switch");
        self.parse_expression(0);
        self.expect(SyntaxKind::RParen, "expected `)` after switch expression");
        // A `switch` statement does not introduce `yield` statements.
        self.parse_switch_block(StatementContext::Normal, SwitchContext::Statement);
        self.builder.finish_node(); // SwitchStatement
    }

    fn parse_switch_expression(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::SwitchExpression.into());
        self.bump();
        self.expect(SyntaxKind::LParen, "expected `(` after switch");
        self.parse_expression(0);
        self.expect(SyntaxKind::RParen, "expected `)` after switch selector");
        self.parse_switch_block(
            StatementContext::SwitchExpression,
            SwitchContext::Expression,
        );
        self.builder.finish_node(); // SwitchExpression
    }

    fn peek_switch_label_terminator(&mut self) -> Option<SwitchLabelTerminator> {
        let mut i = 0usize;
        let mut depth = DelimiterDepth::default();
        let mut ternary_depth: u32 = 0;

        while let Some(tok) = self.tokens.get(i) {
            let kind = tok.kind;
            if kind.is_trivia() {
                i += 1;
                continue;
            }

            if depth.is_zero(false) {
                match kind {
                    SyntaxKind::Question => {
                        ternary_depth += 1;
                    }
                    SyntaxKind::Colon if ternary_depth > 0 => {
                        ternary_depth = ternary_depth.saturating_sub(1);
                    }
                    SyntaxKind::Colon => return Some(SwitchLabelTerminator::Colon),
                    SyntaxKind::Arrow => return Some(SwitchLabelTerminator::Arrow),
                    _ => {}
                }
            }

            depth.update(kind, false);
            i += 1;
        }

        None
    }

    fn parse_switch_label(&mut self) -> SwitchLabelTerminator {
        self.builder.start_node(SyntaxKind::SwitchLabel.into());
        let is_case = self.at(SyntaxKind::CaseKw);
        self.bump(); // case/default
        if is_case {
            if self.at(SyntaxKind::Colon) || self.at(SyntaxKind::Arrow) {
                self.error_here("expected case label");
            } else {
                self.parse_case_label_element();
                while self.at(SyntaxKind::Comma) {
                    self.bump();
                    if self.at(SyntaxKind::Colon) || self.at(SyntaxKind::Arrow) {
                        self.error_here("expected case label after `,`");
                        break;
                    }
                    self.parse_case_label_element();
                }
            }
        }

        let terminator = if self.at(SyntaxKind::Arrow) {
            self.bump();
            SwitchLabelTerminator::Arrow
        } else if self.at(SyntaxKind::Colon) {
            self.bump();
            SwitchLabelTerminator::Colon
        } else {
            self.error_here("expected `:` or `->` after switch label");
            self.recover_to(TokenSet::new(&[
                SyntaxKind::Colon,
                SyntaxKind::Arrow,
                SyntaxKind::CaseKw,
                SyntaxKind::DefaultKw,
                SyntaxKind::RBrace,
                SyntaxKind::StringTemplateExprEnd,
                SyntaxKind::Eof,
            ]));

            if self.at(SyntaxKind::Arrow) {
                self.bump();
                SwitchLabelTerminator::Arrow
            } else if self.at(SyntaxKind::Colon) {
                self.bump();
                SwitchLabelTerminator::Colon
            } else {
                SwitchLabelTerminator::Colon
            }
        };

        self.builder.finish_node();
        terminator
    }

    fn parse_switch_rule_body(&mut self, stmt_ctx: StatementContext, switch_ctx: SwitchContext) {
        self.eat_trivia();
        if self.at(SyntaxKind::LBrace) {
            self.parse_block(stmt_ctx);
            return;
        }
        if self.at(SyntaxKind::Semicolon) {
            // Common during typing: `case 1 -> ;`.
            self.bump();
            return;
        }
        if self.at(SyntaxKind::CaseKw)
            || self.at(SyntaxKind::DefaultKw)
            || self.at(SyntaxKind::RBrace)
            || self.at(SyntaxKind::StringTemplateExprEnd)
            || self.at(SyntaxKind::Eof)
        {
            self.error_here("expected switch rule body after `->`");
            return;
        }

        match switch_ctx {
            SwitchContext::Statement => self.parse_statement(StatementContext::Normal),
            SwitchContext::Expression => {
                if self.at(SyntaxKind::ThrowKw) {
                    self.parse_statement(StatementContext::Normal);
                } else {
                    self.parse_expression(0);
                    self.expect(
                        SyntaxKind::Semicolon,
                        "expected `;` after switch rule expression",
                    );
                }
            }
        }
    }

    fn parse_switch_block(&mut self, stmt_ctx: StatementContext, switch_ctx: SwitchContext) {
        self.builder.start_node(SyntaxKind::SwitchBlock.into());
        self.expect(SyntaxKind::LBrace, "expected `{` after switch");
        while !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::StringTemplateExprEnd)
            && !self.at(SyntaxKind::Eof)
        {
            let before = self.tokens.len();
            self.eat_trivia();

            if self.at(SyntaxKind::CaseKw) || self.at(SyntaxKind::DefaultKw) {
                let checkpoint = self.builder.checkpoint();
                let terminator = self.parse_switch_label();
                match terminator {
                    SwitchLabelTerminator::Colon => {
                        self.builder
                            .start_node_at(checkpoint, SyntaxKind::SwitchGroup.into());
                        while (self.at(SyntaxKind::CaseKw) || self.at(SyntaxKind::DefaultKw))
                            && self.peek_switch_label_terminator()
                                == Some(SwitchLabelTerminator::Colon)
                        {
                            self.parse_switch_label();
                        }

                        while !(self.at(SyntaxKind::RBrace)
                            || self.at(SyntaxKind::StringTemplateExprEnd)
                            || self.at(SyntaxKind::Eof)
                            || self.at(SyntaxKind::CaseKw)
                            || self.at(SyntaxKind::DefaultKw))
                        {
                            let stmt_before = self.tokens.len();
                            self.parse_statement(stmt_ctx);
                            self.force_progress(stmt_before, STMT_RECOVERY);
                        }

                        self.builder.finish_node(); // SwitchGroup
                    }
                    SwitchLabelTerminator::Arrow => {
                        self.builder
                            .start_node_at(checkpoint, SyntaxKind::SwitchRule.into());
                        self.parse_switch_rule_body(stmt_ctx, switch_ctx);
                        self.builder.finish_node(); // SwitchRule
                    }
                }
            } else {
                self.parse_statement(stmt_ctx);
            }

            self.force_progress(before, STMT_RECOVERY);
        }
        self.expect(SyntaxKind::RBrace, "expected `}` after switch block");
        self.builder.finish_node(); // SwitchBlock
    }

    fn parse_case_label_element(&mut self) {
        self.builder.start_node(SyntaxKind::CaseLabelElement.into());
        self.eat_trivia();

        let mut is_pattern = false;
        match self.current() {
            // Java 21+: `case null, default -> ...`
            SyntaxKind::DefaultKw => {
                self.bump();
            }
            _ => {
                if self.at_pattern_start() {
                    is_pattern = true;
                    self.parse_pattern(true);
                } else {
                    self.parse_expression_no_lambda(0);
                }
            }
        }

        if is_pattern && (self.at(SyntaxKind::WhenKw) || self.at(SyntaxKind::AmpAmp)) {
            self.parse_guard();
        }

        self.builder.finish_node(); // CaseLabelElement
    }

    fn parse_guard(&mut self) {
        self.builder.start_node(SyntaxKind::Guard.into());
        if self.at(SyntaxKind::WhenKw) {
            self.bump();
        } else if self.at(SyntaxKind::AmpAmp) {
            // Early preview builds of pattern matching for switch used `&&` for guards.
            self.bump();
        } else {
            self.error_here("expected `when` or `&&`");
        }
        // Avoid consuming the label terminator on malformed guards.
        if matches!(
            self.current(),
            SyntaxKind::Arrow
                | SyntaxKind::Colon
                | SyntaxKind::Comma
                | SyntaxKind::RParen
                | SyntaxKind::RBrace
                | SyntaxKind::StringTemplateExprEnd
                | SyntaxKind::Eof
        ) {
            self.error_here("expected guard expression");
        } else {
            // `->`/`:` are switch-label terminators, not part of the guard expression; disable
            // lambda parsing here so `when (i > 0) ->` doesn't get misparsed as a lambda.
            self.parse_expression_no_lambda(0);
        }
        self.builder.finish_node(); // Guard
    }

    fn at_pattern_start(&mut self) -> bool {
        self.at_record_pattern_start()
            || self.at_type_pattern_start()
            || self.at_underscore_identifier()
            || self.at(SyntaxKind::FinalKw)
            || (self.at(SyntaxKind::At) && self.nth(1) != Some(SyntaxKind::InterfaceKw))
    }

    fn at_type_pattern_start(&mut self) -> bool {
        // Type patterns share the same prefix shape as local variable declarations:
        // `[final|@Anno]* Type Identifier`.
        self.at_local_var_decl_start()
    }

    fn at_record_pattern_start(&mut self) -> bool {
        let mut i = skip_trivia(&self.tokens, 0);

        // Record patterns can be preceded by annotations (type annotations). Be permissive and
        // treat `final` the same way for recovery.
        loop {
            match self.tokens.get(i).map(|t| t.kind) {
                Some(SyntaxKind::FinalKw) => {
                    i = skip_trivia(&self.tokens, i + 1);
                }
                Some(SyntaxKind::At) => {
                    // Skip `@Name(...)` loosely (same strategy as `at_local_var_decl_start`).
                    i = skip_trivia(&self.tokens, i + 1);
                    if self
                        .tokens
                        .get(i)
                        .is_some_and(|t| t.kind.is_identifier_like())
                    {
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
                                .is_some_and(|t| t.kind.is_identifier_like())
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

        let Some(first) = self.tokens.get(i).map(|t| t.kind) else {
            return false;
        };
        if !first.is_identifier_like() {
            return false;
        }

        // Qualified name + optional type arguments.
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
                .is_some_and(|t| t.kind.is_identifier_like())
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
        self.tokens.get(i).map(|t| t.kind) == Some(SyntaxKind::LParen)
    }

    fn parse_pattern(&mut self, allow_guard: bool) {
        self.builder.start_node(SyntaxKind::Pattern.into());
        if self.at_underscore_identifier() {
            self.parse_unnamed_pattern();
        } else if self.at_record_pattern_start() {
            self.parse_record_pattern();
        } else {
            self.parse_type_pattern(allow_guard);
        }
        self.builder.finish_node(); // Pattern
    }

    fn parse_type_pattern(&mut self, allow_guard: bool) {
        self.builder.start_node(SyntaxKind::TypePattern.into());
        self.parse_pattern_modifiers();

        if self.at_type_start() {
            self.parse_type();
        } else {
            self.error_here("expected pattern type");
            // Ensure progress on malformed patterns without consuming the label terminator.
            if !matches!(
                self.current(),
                SyntaxKind::Arrow
                    | SyntaxKind::Colon
                    | SyntaxKind::Comma
                    | SyntaxKind::RParen
                    | SyntaxKind::StringTemplateExprEnd
                    | SyntaxKind::Eof
            ) {
                self.bump_any();
            }
        }

        // Avoid swallowing the label terminator on incomplete patterns.
        if matches!(
            self.current(),
            SyntaxKind::Arrow
                | SyntaxKind::Colon
                | SyntaxKind::Comma
                | SyntaxKind::RParen
                | SyntaxKind::StringTemplateExprEnd
                | SyntaxKind::Eof
        ) {
            self.error_here("expected binding identifier");
        } else if self.at_underscore_identifier() {
            self.parse_unnamed_pattern();
        } else if allow_guard
            && self.at(SyntaxKind::WhenKw)
            // In a switch case label, `when` introduces a guard if it is followed by an expression.
            // Otherwise, it can still be a binding identifier (contextual keyword).
            && self.nth(1).is_none_or(can_start_expression)
        {
            self.error_here("expected binding identifier");
        } else {
            self.expect_ident_like("expected binding identifier");
        }

        self.builder.finish_node(); // TypePattern
    }

    fn parse_record_pattern(&mut self) {
        self.builder.start_node(SyntaxKind::RecordPattern.into());
        self.parse_pattern_modifiers();

        if self.at_type_start() {
            self.parse_type();
        } else {
            self.error_here("expected record pattern type");
        }

        self.expect(SyntaxKind::LParen, "expected `(` in record pattern");
        // Recover aggressively if we hit a switch-label boundary before the closing `)`.
        // This is important for IDE use-cases where users might type `case Point( ->` and we
        // don't want to consume the `->` (and subsequent tokens) as record pattern components.
        while !matches!(
            self.current(),
            SyntaxKind::RParen
                | SyntaxKind::Arrow
                | SyntaxKind::Colon
                | SyntaxKind::CaseKw
                | SyntaxKind::DefaultKw
                | SyntaxKind::RBrace
                | SyntaxKind::StringTemplateExprEnd
                | SyntaxKind::Eof
        ) {
            if self.at(SyntaxKind::Comma) {
                self.error_here("expected record pattern component");
                self.bump();
                continue;
            }

            if self.at_pattern_start() {
                self.parse_pattern(false);
            } else {
                self.builder.start_node(SyntaxKind::Error.into());
                self.error_here("expected record pattern component");
                // Ensure progress without consuming the list terminator.
                if !matches!(
                    self.current(),
                    SyntaxKind::Comma
                        | SyntaxKind::RParen
                        | SyntaxKind::StringTemplateExprEnd
                        | SyntaxKind::Eof
                ) {
                    self.bump_any();
                }
                self.builder.finish_node();
            }

            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect(SyntaxKind::RParen, "expected `)` in record pattern");
        self.builder.finish_node(); // RecordPattern
    }

    fn parse_pattern_modifiers(&mut self) {
        self.builder.start_node(SyntaxKind::Modifiers.into());
        loop {
            self.eat_trivia();
            if self.at(SyntaxKind::At) {
                // `@interface` is not a pattern modifier.
                if self.nth(1) == Some(SyntaxKind::InterfaceKw) {
                    break;
                }
                self.parse_annotation();
                continue;
            }
            if self.at(SyntaxKind::FinalKw) {
                self.bump();
                continue;
            }
            break;
        }
        self.builder.finish_node(); // Modifiers
    }

    fn parse_try_statement(&mut self, stmt_ctx: StatementContext, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::TryStatement.into());
        self.expect(SyntaxKind::TryKw, "expected `try`");
        if self.at(SyntaxKind::LParen) {
            self.parse_resource_specification();
        }
        self.parse_block_with_recovery(stmt_ctx, TRY_BLOCK_RECOVERY);
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
            if self.at_underscore_identifier() {
                self.parse_unnamed_pattern();
            } else {
                self.expect_ident_like("expected catch parameter name");
            }
            self.expect(SyntaxKind::RParen, "expected `)` after catch parameter");
            self.parse_block_with_recovery(stmt_ctx, TRY_BLOCK_RECOVERY);
            self.builder.finish_node();
        }
        if self.at(SyntaxKind::FinallyKw) {
            self.builder.start_node(SyntaxKind::FinallyClause.into());
            self.bump();
            self.parse_block_with_recovery(stmt_ctx, TRY_BLOCK_RECOVERY);
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
                self.parse_variable_declarator(true);
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
        self.expect(
            SyntaxKind::RParen,
            "expected `)` after resource specification",
        );
        self.builder.finish_node(); // ResourceSpecification
    }

    fn parse_for_header_contents(&mut self) {
        // Enhanced-for and classic-for share the same outer structure: `for ( ... )`.
        if self.at_local_var_decl_start() {
            self.parse_modifiers();
            self.parse_type();
            self.parse_variable_declarator_list(true);

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

    fn parse_variable_declarator_list(&mut self, allow_unnamed: bool) {
        self.builder
            .start_node(SyntaxKind::VariableDeclaratorList.into());
        self.parse_variable_declarator(allow_unnamed);
        while self.at(SyntaxKind::Comma) {
            self.bump();
            self.parse_variable_declarator(allow_unnamed);
        }
        self.builder.finish_node();
    }

    fn parse_variable_declarator(&mut self, allow_unnamed: bool) {
        self.builder
            .start_node(SyntaxKind::VariableDeclarator.into());
        if allow_unnamed && self.at_underscore_identifier() {
            self.parse_unnamed_pattern();
        } else {
            self.expect_ident_like("expected variable name");
        }

        // Support Java's `int x[]` style dims (and dimension annotations) after the declarator
        // identifier, for both fields and local variables.
        loop {
            self.eat_trivia();
            while self.at_type_annotation_start() {
                self.parse_annotation();
                self.eat_trivia();
            }

            if self.at(SyntaxKind::LBracket) && self.nth(1) == Some(SyntaxKind::RBracket) {
                self.bump();
                self.bump();
                continue;
            }
            break;
        }

        if self.at(SyntaxKind::Eq) {
            self.bump();
            if self.at(SyntaxKind::Semicolon) || self.at(SyntaxKind::Comma) {
                self.error_here("expected initializer expression");
            } else if self.at(SyntaxKind::LBrace) {
                self.parse_array_initializer(true);
            } else {
                self.parse_expression(0);
            }
        }
        self.builder.finish_node();
    }

    fn parse_type(&mut self) {
        self.builder.start_node(SyntaxKind::Type.into());
        self.eat_trivia();
        // Java 8+: type-use annotations (JSR 308) can appear in most type positions, notably
        // within type arguments: `List<@A String>`.
        while self.at_type_annotation_start() {
            self.parse_annotation();
        }
        if self.at_primitive_type() {
            self.builder.start_node(SyntaxKind::PrimitiveType.into());
            self.bump();
            self.builder.finish_node();
        } else {
            self.builder.start_node(SyntaxKind::NamedType.into());
            self.expect_ident_like("expected type name");
            loop {
                if !self.at(SyntaxKind::Dot) {
                    break;
                }
                self.bump(); // '.'
                self.eat_trivia();
                // Java 8+: type-use annotations can appear before qualified name segments:
                // `Outer.@A Inner`.
                while self.at_type_annotation_start() {
                    self.parse_annotation();
                    self.eat_trivia();
                }
                if !self.at_ident_like() {
                    // `expect_ident_like` does not consume tokens on failure, so ensure we don't
                    // loop forever on malformed inputs like `Outer.@A`.
                    self.error_here("expected type name segment");
                    break;
                }
                self.expect_ident_like("expected type name segment");
            }
            if self.at(SyntaxKind::Less) {
                self.parse_type_arguments();
            }
            self.builder.finish_node();
        }

        // Array dims: `T @A [] @B []`.
        loop {
            self.eat_trivia();
            while self.at_type_annotation_start() {
                self.parse_annotation();
                self.eat_trivia();
            }

            if self.at(SyntaxKind::LBracket) && self.nth(1) == Some(SyntaxKind::RBracket) {
                self.bump();
                self.bump();
                continue;
            }

            break;
        }

        // Varargs parameters: `T @A ... args`.
        while self.at_type_annotation_start() {
            self.parse_annotation();
            self.eat_trivia();
        }
        if self.at(SyntaxKind::Ellipsis) {
            self.bump();
        }
        self.builder.finish_node();
    }

    fn parse_type_arguments(&mut self) {
        self.builder.start_node(SyntaxKind::TypeArguments.into());
        self.parse_type_arguments_contents();
        self.builder.finish_node();
    }

    fn parse_type_arguments_contents(&mut self) {
        self.expect(SyntaxKind::Less, "expected `<`");
        while !matches!(
            self.current(),
            SyntaxKind::Greater
                | SyntaxKind::RightShift
                | SyntaxKind::UnsignedRightShift
                | SyntaxKind::Eof
        ) {
            self.builder.start_node(SyntaxKind::TypeArgument.into());
            if self.at(SyntaxKind::Question) {
                self.builder.start_node(SyntaxKind::WildcardType.into());
                self.bump();
                if self.at(SyntaxKind::ExtendsKw) || self.at(SyntaxKind::SuperKw) {
                    self.bump();
                    self.parse_type();
                }
                self.builder.finish_node(); // WildcardType
            } else if self.at_type_start() {
                self.parse_type();
            } else {
                self.builder.start_node(SyntaxKind::Error.into());
                self.error_here("expected type argument");
                self.recover_to_including_angles(TYPE_ARGUMENT_RECOVERY);
                self.builder.finish_node(); // Error
            }
            self.builder.finish_node(); // TypeArgument
            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect_gt();
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
                self.insert_missing(SyntaxKind::MissingGreater);
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
        self.parse_expression_inner(min_bp, true);
    }

    fn parse_expression_no_lambda(&mut self, min_bp: u8) {
        self.parse_expression_inner(min_bp, false);
    }

    fn parse_expression_inner(&mut self, min_bp: u8, allow_lambda: bool) {
        self.eat_trivia();
        let checkpoint = self.builder.checkpoint();

        // Prefix / primary.
        match self.current() {
            SyntaxKind::SwitchKw => {
                // Java switch expressions share the same surface syntax as switch statements, but
                // appear in expression position.
                self.parse_switch_expression(checkpoint);
            }
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
                self.parse_new_expression_or_array_creation(checkpoint, allow_lambda);
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
                self.parse_expression_inner(100, allow_lambda);
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
                if allow_lambda && self.nth(1) == Some(SyntaxKind::Arrow) {
                    self.parse_lambda_expression(checkpoint);
                } else {
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::NameExpression.into());
                    self.bump();
                    loop {
                        // Permit parsing type arguments in a handful of expression contexts where
                        // the grammar expects a reference type, such as `List<String>::new`.
                        if self.nth(0) == Some(SyntaxKind::Less)
                            && self.at_reference_type_arguments_start()
                        {
                            self.parse_type_arguments();
                        }

                        if self.at(SyntaxKind::Dot)
                            && self.nth(1).is_some_and(|k| k.is_identifier_like())
                        {
                            self.bump();
                            self.bump();
                            continue;
                        }

                        break;
                    }
                    if self.at(SyntaxKind::LBracket)
                        && self.nth(1) == Some(SyntaxKind::RBracket)
                        && self.at_array_type_suffix_start()
                    {
                        // Preserve array type suffixes used in class literals and method
                        // references: `String[].class`, `String[]::new`, etc.
                        while self.at(SyntaxKind::LBracket)
                            && self.nth(1) == Some(SyntaxKind::RBracket)
                        {
                            self.bump();
                            self.bump();
                        }
                    }
                    self.builder.finish_node();
                }
            }
            SyntaxKind::LParen => {
                if allow_lambda && self.is_lambda_paren() {
                    self.parse_lambda_expression(checkpoint);
                } else if self.is_cast_expression() {
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::CastExpression.into());
                    self.bump();
                    self.parse_type();
                    self.expect(SyntaxKind::RParen, "expected `)` in cast");
                    self.parse_expression_inner(100, allow_lambda);
                    self.builder.finish_node();
                } else {
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::ParenthesizedExpression.into());
                    self.bump();
                    self.parse_expression_inner(0, allow_lambda);
                    self.expect(SyntaxKind::RParen, "expected `)`");
                    self.builder.finish_node();
                }
            }
            SyntaxKind::Less if self.at_explicit_generic_invocation_start() => {
                // Explicit generic invocation: `<T>method(args)`.
                //
                // Model this as a `FieldAccessExpression` (without a receiver) so the
                // `TypeArguments` sit in the same place as they do for
                // `expr.<T>method(args)`.
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::FieldAccessExpression.into());
                self.parse_type_arguments();
                self.expect_ident_like("expected name after type arguments");
                self.builder.finish_node();
            }
            SyntaxKind::Less if self.type_arguments_followed_by_this_or_super().is_some() => {
                // Explicit type arguments for constructor invocation: `<T>this(...)`,
                // `<T>super(...)`.
                let keyword = self
                    .type_arguments_followed_by_this_or_super()
                    .expect("guard ensures this is Some");
                let kind = match keyword {
                    SyntaxKind::ThisKw => SyntaxKind::ThisExpression,
                    SyntaxKind::SuperKw => SyntaxKind::SuperExpression,
                    _ => unreachable!("expected `this` or `super`, got {keyword:?}"),
                };

                self.builder.start_node_at(checkpoint, kind.into());
                self.parse_type_arguments();
                if self.at(keyword) {
                    self.bump();
                } else {
                    self.error_here("expected `this` or `super` after type arguments");
                }
                self.builder.finish_node();
            }
            kind if (is_primitive_type(kind) && self.at_primitive_type_suffix_start())
                || (kind == SyntaxKind::VoidKw && self.at_primitive_class_literal_start()) =>
            {
                // `int.class`, `int[]::new`, etc are valid Java expressions, but primitive type
                // keywords are not normally accepted as expression primaries. Treat them like a
                // name in this narrow context so the postfix parser can build class literals and
                // method/constructor references without producing spurious parse errors.
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::NameExpression.into());
                self.bump();
                // Preserve array type suffixes: `int[]::new`, `int[].class`, etc.
                while self.at(SyntaxKind::LBracket) && self.nth(1) == Some(SyntaxKind::RBracket) {
                    self.bump();
                    self.bump();
                }
                self.builder.finish_node();
            }
            _ => {
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::Error.into());
                self.error_here("expected expression");
                // If we're sitting at a structural delimiter, don't consume it: callers
                // typically expect to see it and can recover locally.
                self.eat_trivia();
                let kind = self
                    .tokens
                    .front()
                    .map(|t| t.kind)
                    .unwrap_or(SyntaxKind::Eof);
                if kind != SyntaxKind::Eof && !EXPR_RECOVERY.contains(kind) {
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
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::MethodCallExpression.into());
                    self.parse_argument_list();
                    self.builder.finish_node();
                    continue;
                }
                SyntaxKind::Dot => {
                    if min_bp > 120 {
                        break;
                    }
                    // String templates (preview): `processor."..."` / `processor."""..."""`.
                    //
                    // The lexer emits dedicated `StringTemplate*` tokens, so templates are always
                    // parsed via the `StringTemplateStart..StringTemplateEnd` token sequence.
                    if self.nth(1) == Some(SyntaxKind::StringTemplateStart) {
                        self.builder
                            .start_node_at(checkpoint, SyntaxKind::StringTemplateExpression.into());
                        self.bump(); // .
                        self.parse_string_template();
                        continue;
                    }
                    if self.nth(1) == Some(SyntaxKind::ClassKw) {
                        self.builder
                            .start_node_at(checkpoint, SyntaxKind::ClassLiteralExpression.into());
                        self.bump(); // .
                        self.bump(); // class
                        self.builder.finish_node();
                        continue;
                    }
                    // Qualified `this` / `super`: `TypeName.this`, `TypeName.super`.
                    if self.nth(1) == Some(SyntaxKind::ThisKw) {
                        self.builder
                            .start_node_at(checkpoint, SyntaxKind::ThisExpression.into());
                        self.bump(); // .
                        self.bump(); // this
                        self.builder.finish_node();
                        continue;
                    }
                    let mut super_lookahead = skip_trivia(&self.tokens, 1);
                    if self.tokens.get(super_lookahead).map(|t| t.kind) == Some(SyntaxKind::Less) {
                        super_lookahead = skip_trivia(
                            &self.tokens,
                            skip_type_arguments(&self.tokens, super_lookahead),
                        );
                    }
                    if self.tokens.get(super_lookahead).map(|t| t.kind) == Some(SyntaxKind::SuperKw)
                    {
                        // Qualified `super`: `TypeName.super` and qualified explicit constructor
                        // invocation: `Primary.<T>super(...)`.
                        self.builder
                            .start_node_at(checkpoint, SyntaxKind::SuperExpression.into());
                        self.bump(); // .
                        if self.nth(0) == Some(SyntaxKind::Less) {
                            self.parse_type_arguments();
                        }
                        self.bump(); // super
                        self.builder.finish_node();
                        continue;
                    }
                    // Qualified class instance creation: `expr.new T(...)`.
                    let mut lookahead = skip_trivia(&self.tokens, 1);
                    if self.tokens.get(lookahead).map(|t| t.kind) == Some(SyntaxKind::Less) {
                        lookahead =
                            skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, lookahead));
                    }
                    if self.tokens.get(lookahead).map(|t| t.kind) == Some(SyntaxKind::NewKw) {
                        self.bump(); // .
                                     // Java allows optional type arguments between `.` and `new` for qualified
                                     // instance creations: `expr.<T>new Foo(...)`.
                        if self.nth(0) == Some(SyntaxKind::Less) {
                            self.parse_type_arguments();
                        }
                        self.parse_new_expression_or_array_creation(checkpoint, allow_lambda);
                        continue;
                    }

                    // Field access / method call receiver segment. Java also allows explicit type
                    // arguments on method invocations: `expr.<T>method()`.
                    let mut lookahead = skip_trivia(&self.tokens, 1);
                    if self.tokens.get(lookahead).map(|t| t.kind) == Some(SyntaxKind::Less) {
                        lookahead =
                            skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, lookahead));
                    }
                    if self
                        .tokens
                        .get(lookahead)
                        .is_some_and(|t| t.kind.is_identifier_like())
                    {
                        self.builder
                            .start_node_at(checkpoint, SyntaxKind::FieldAccessExpression.into());
                        self.bump(); // .
                        if self.at(SyntaxKind::Less) {
                            self.parse_type_arguments();
                        }
                        self.expect_ident_like("expected name after `.`");
                        self.builder.finish_node();
                        continue;
                    }
                    break;
                }
                SyntaxKind::DoubleColon => {
                    if min_bp > 120 {
                        break;
                    }
                    // Method reference (`Foo::bar`) / constructor reference (`Foo::new`).
                    //
                    // Java allows optional type arguments after the `::`:
                    // `Foo::<T>bar`, `Foo::<T>new`.
                    let mut lookahead = skip_trivia(&self.tokens, 1);
                    if self.tokens.get(lookahead).map(|t| t.kind) == Some(SyntaxKind::Less) {
                        lookahead =
                            skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, lookahead));
                    }
                    let is_constructor_ref =
                        self.tokens.get(lookahead).map(|t| t.kind) == Some(SyntaxKind::NewKw);
                    let kind = if is_constructor_ref {
                        SyntaxKind::ConstructorReferenceExpression
                    } else {
                        SyntaxKind::MethodReferenceExpression
                    };
                    self.builder.start_node_at(checkpoint, kind.into());
                    self.bump(); // ::

                    if self.at(SyntaxKind::Less) {
                        self.parse_type_arguments();
                    }

                    if is_constructor_ref {
                        if self.at(SyntaxKind::NewKw) {
                            self.bump();
                        } else {
                            self.builder.start_node(SyntaxKind::Error.into());
                            self.error_here("expected `new` after `::`");
                            self.builder.finish_node();
                        }
                    } else if self.at_ident_like() {
                        self.bump();
                    } else {
                        // Keep the parse lossless and avoid consuming arbitrary tokens. This
                        // commonly happens while typing (`Foo::`).
                        self.builder.start_node(SyntaxKind::Error.into());
                        self.error_here("expected method name after `::`");
                        self.builder.finish_node();
                    }

                    self.builder.finish_node();
                    continue;
                }
                SyntaxKind::LBracket => {
                    if min_bp > 120 {
                        break;
                    }
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::ArrayAccessExpression.into());
                    self.bump();
                    if !self.at(SyntaxKind::RBracket) {
                        self.parse_expression_inner(0, allow_lambda);
                    }
                    self.expect(SyntaxKind::RBracket, "expected `]`");
                    self.builder.finish_node();
                    continue;
                }
                SyntaxKind::PlusPlus | SyntaxKind::MinusMinus => {
                    if min_bp > 120 {
                        break;
                    }
                    self.builder
                        .start_node_at(checkpoint, SyntaxKind::UnaryExpression.into());
                    self.bump();
                    self.builder.finish_node();
                    continue;
                }
                _ => {}
            }

            // `instanceof` (type test / pattern match) has relational precedence but the RHS is
            // not a normal expression in modern Java.
            if op == SyntaxKind::InstanceofKw {
                let (l_bp, _r_bp) = (50, 51);
                if l_bp < min_bp {
                    break;
                }
                self.builder
                    .start_node_at(checkpoint, SyntaxKind::InstanceofExpression.into());
                self.bump(); // instanceof
                self.parse_instanceof_type_or_pattern();
                self.builder.finish_node();
                continue;
            }

            if let Some((l_bp, r_bp, expr_kind)) = infix_binding_power(op) {
                if l_bp < min_bp {
                    break;
                }
                self.builder.start_node_at(checkpoint, expr_kind.into());
                self.bump();
                self.parse_expression_inner(r_bp, allow_lambda);
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
                self.parse_expression_inner(0, allow_lambda);
                self.expect(SyntaxKind::Colon, "expected `:` in conditional expression");
                self.parse_expression_inner(r_bp, allow_lambda);
                self.builder.finish_node();
                continue;
            }

            break;
        }
    }

    fn parse_string_template(&mut self) {
        self.builder.start_node(SyntaxKind::StringTemplate.into());

        // We only enter this parser after recognizing `.` + `StringTemplateStart` in the postfix
        // expression parser, but keep it resilient for partial code.
        self.expect(
            SyntaxKind::StringTemplateStart,
            "expected string template delimiter",
        );

        while !self.at(SyntaxKind::StringTemplateEnd) && !self.at(SyntaxKind::Eof) {
            match self.current() {
                SyntaxKind::StringTemplateText => {
                    self.bump();
                }
                SyntaxKind::StringTemplateExprStart => {
                    self.builder
                        .start_node(SyntaxKind::StringTemplateInterpolation.into());
                    self.bump(); // \{
                    self.parse_expression(0);
                    self.expect(
                        SyntaxKind::StringTemplateExprEnd,
                        "expected `}` to close template expression",
                    );
                    self.builder.finish_node(); // StringTemplateInterpolation
                }
                _ => {
                    // Preserve a stable subtree shape and avoid infinite loops for unexpected
                    // tokens in the template body (common while typing).
                    self.builder.start_node(SyntaxKind::Error.into());
                    self.error_here("expected template text or interpolation");
                    if !self.at(SyntaxKind::Eof) {
                        self.bump_any();
                    }
                    self.builder.finish_node(); // Error
                }
            }
        }

        self.expect(
            SyntaxKind::StringTemplateEnd,
            "expected closing string template delimiter",
        );

        self.builder.finish_node(); // StringTemplate
        self.builder.finish_node(); // StringTemplateExpression
    }

    fn at_explicit_generic_invocation_start(&mut self) -> bool {
        if !self.at(SyntaxKind::Less) {
            return false;
        }

        // Ensure we can match a complete `<...>` and that it's followed by an
        // identifier-like token (`<T>foo(...)`). This avoids producing a
        // cascading error node while typing an incomplete `<`.
        let lookahead = skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, 0));
        self.tokens
            .get(lookahead)
            .is_some_and(|t| t.kind.is_identifier_like())
    }

    fn type_arguments_followed_by_this_or_super(&mut self) -> Option<SyntaxKind> {
        if self.nth(0) != Some(SyntaxKind::Less) {
            return None;
        }

        let lookahead = skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, 0));
        self.tokens.get(lookahead).map(|t| t.kind).and_then(|kind| {
            matches!(kind, SyntaxKind::ThisKw | SyntaxKind::SuperKw).then_some(kind)
        })
    }

    fn at_explicit_constructor_invocation_start(&mut self) -> bool {
        // Supports:
        // - `this(...) ;`
        // - `super(...) ;`
        // - `<T>this(...) ;`
        // - `<T>super(...) ;`
        // - `Primary.super(...) ;`
        // - `Primary.<T>super(...) ;`
        let start = skip_trivia(&self.tokens, 0);
        let Some(tok) = self.tokens.get(start) else {
            return false;
        };

        match tok.kind {
            SyntaxKind::ThisKw | SyntaxKind::SuperKw => {
                let next = skip_trivia(&self.tokens, start + 1);
                return self.tokens.get(next).map(|t| t.kind) == Some(SyntaxKind::LParen);
            }
            SyntaxKind::Less => {
                let after_args =
                    skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, start));
                match self.tokens.get(after_args).map(|t| t.kind) {
                    Some(SyntaxKind::ThisKw) | Some(SyntaxKind::SuperKw) => {
                        let after_kw = skip_trivia(&self.tokens, after_args + 1);
                        return self.tokens.get(after_kw).map(|t| t.kind)
                            == Some(SyntaxKind::LParen);
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        let mut i = start;
        let mut depth = DelimiterDepth::default();
        while let Some(tok) = self.tokens.get(i) {
            let kind = tok.kind;
            if kind.is_trivia() {
                i += 1;
                continue;
            }

            if depth.is_zero(false) {
                // Don't scan beyond the end of the current statement.
                if kind == SyntaxKind::Semicolon {
                    break;
                }

                if kind == SyntaxKind::Dot {
                    let mut lookahead = skip_trivia(&self.tokens, i + 1);
                    if self.tokens.get(lookahead).map(|t| t.kind) == Some(SyntaxKind::Less) {
                        lookahead =
                            skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, lookahead));
                    }
                    if self.tokens.get(lookahead).map(|t| t.kind) == Some(SyntaxKind::SuperKw) {
                        let after_super = skip_trivia(&self.tokens, lookahead + 1);
                        if self.tokens.get(after_super).map(|t| t.kind) == Some(SyntaxKind::LParen)
                        {
                            return true;
                        }
                    }
                }
            }

            depth.update(kind, false);
            i += 1;
        }

        false
    }

    fn at_primitive_class_literal_start(&mut self) -> bool {
        let mut offset = 1usize;
        while self.nth(offset) == Some(SyntaxKind::LBracket)
            && self.nth(offset + 1) == Some(SyntaxKind::RBracket)
        {
            offset += 2;
        }
        self.nth(offset) == Some(SyntaxKind::Dot)
            && self.nth(offset + 1) == Some(SyntaxKind::ClassKw)
    }

    fn at_primitive_method_reference_start(&mut self) -> bool {
        let mut offset = 1usize;
        while self.nth(offset) == Some(SyntaxKind::LBracket)
            && self.nth(offset + 1) == Some(SyntaxKind::RBracket)
        {
            offset += 2;
        }
        self.nth(offset) == Some(SyntaxKind::DoubleColon)
    }

    fn at_primitive_type_suffix_start(&mut self) -> bool {
        self.at_primitive_class_literal_start() || self.at_primitive_method_reference_start()
    }

    fn at_array_type_suffix_start(&mut self) -> bool {
        let mut offset = 0usize;
        while self.nth(offset) == Some(SyntaxKind::LBracket)
            && self.nth(offset + 1) == Some(SyntaxKind::RBracket)
        {
            offset += 2;
        }
        (self.nth(offset) == Some(SyntaxKind::Dot)
            && self.nth(offset + 1) == Some(SyntaxKind::ClassKw))
            || self.nth(offset) == Some(SyntaxKind::DoubleColon)
    }

    fn at_reference_type_arguments_start(&mut self) -> bool {
        // Determines whether the token stream looks like type arguments that are part of a
        // reference type suffix, such as:
        // - `List<String>::new`
        // - `Outer<String>.Inner::new`
        // - `List<String>[].class`
        //
        // This is intentionally conservative to avoid stealing `<` from binary expressions.
        let mut idx = skip_trivia(&self.tokens, 0);
        if self.tokens.get(idx).map(|t| t.kind) != Some(SyntaxKind::Less) {
            return false;
        }

        idx = skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, idx));

        // Allow additional `. Ident <...>?` segments (for nested types).
        loop {
            let dot = skip_trivia(&self.tokens, idx);
            if self.tokens.get(dot).map(|t| t.kind) != Some(SyntaxKind::Dot) {
                idx = dot;
                break;
            }

            let seg = skip_trivia(&self.tokens, dot + 1);
            if !self
                .tokens
                .get(seg)
                .is_some_and(|t| t.kind.is_identifier_like())
            {
                idx = dot;
                break;
            }

            idx = seg + 1;
            idx = skip_trivia(&self.tokens, idx);
            if self.tokens.get(idx).map(|t| t.kind) == Some(SyntaxKind::Less) {
                idx = skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, idx));
            }
        }

        idx = skip_reference_type_array_suffix(&self.tokens, idx);
        idx = skip_trivia(&self.tokens, idx);

        match self.tokens.get(idx).map(|t| t.kind) {
            Some(SyntaxKind::DoubleColon) => true,
            Some(SyntaxKind::Dot) => {
                let next = skip_trivia(&self.tokens, idx + 1);
                self.tokens.get(next).map(|t| t.kind) == Some(SyntaxKind::ClassKw)
            }
            _ => false,
        }
    }

    fn parse_instanceof_type_or_pattern(&mut self) {
        self.eat_trivia();

        // Java 16+: pattern matching for instanceof.
        if self.at_record_pattern_start() || self.at_type_pattern_start() {
            self.parse_pattern(false);
            return;
        }

        // Classic type-test (`x instanceof String`). Allow annotations/final for recovery and for
        // type-use annotations, even though we don't model them precisely.
        if self.at(SyntaxKind::FinalKw) || self.at(SyntaxKind::At) {
            self.parse_pattern_modifiers();
        }

        if self.at_type_start() {
            self.parse_type();
        } else {
            self.error_here("expected type or pattern after `instanceof`");
            // Ensure progress without swallowing expression terminators.
            if !matches!(
                self.current(),
                SyntaxKind::RParen
                    | SyntaxKind::Semicolon
                    | SyntaxKind::Comma
                    | SyntaxKind::StringTemplateExprEnd
                    | SyntaxKind::Eof
            ) {
                self.bump_any();
            }
        }
    }

    fn parse_new_expression_or_array_creation(
        &mut self,
        checkpoint: rowan::Checkpoint,
        allow_lambda: bool,
    ) {
        // We parse `new` first, then decide whether this is a class instance creation (`new T(...)`)
        // or an array creation (`new T[...]` / `new T[] {...}`), and finally wrap the whole thing
        // in the appropriate expression node.
        self.bump(); // new
                     // Java allows explicit type arguments on constructor invocations:
                     // `new <T> Foo(...)`.
        if self.nth(0) == Some(SyntaxKind::Less) {
            self.parse_type_arguments();
        }
        self.parse_type_no_dims();

        if self.at(SyntaxKind::LParen) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::NewExpression.into());
            self.parse_argument_list();
            // Anonymous class instance creation: `new T(...) { ... }`.
            if self.at(SyntaxKind::LBrace) {
                self.parse_class_body(SyntaxKind::ClassBody);
            }
            self.builder.finish_node();
            return;
        }

        // Array creation:
        // - `new T[expr]...`
        // - `new T[] { ... }`
        // - `new T[expr][] { ... }` (mixed dims)
        if self.at(SyntaxKind::LBracket) || self.at(SyntaxKind::LBrace) {
            self.builder
                .start_node_at(checkpoint, SyntaxKind::ArrayCreationExpression.into());
            if self.at(SyntaxKind::LBracket) {
                self.parse_array_creation_dimensions(allow_lambda);
            }
            if self.at(SyntaxKind::LBrace) {
                self.parse_array_initializer(allow_lambda);
            }
            self.builder.finish_node();
            return;
        }

        // Recovery: keep the old `new Type` node shape when there's no argument list or dims.
        self.builder
            .start_node_at(checkpoint, SyntaxKind::NewExpression.into());
        self.builder.finish_node();
    }

    fn parse_type_no_dims(&mut self) {
        self.builder.start_node(SyntaxKind::Type.into());
        self.eat_trivia();
        while self.at_type_annotation_start() {
            self.parse_annotation();
        }
        if self.at_primitive_type() {
            self.builder.start_node(SyntaxKind::PrimitiveType.into());
            self.bump();
            self.builder.finish_node();
        } else {
            self.builder.start_node(SyntaxKind::NamedType.into());
            self.expect_ident_like("expected type name");
            if self.nth(0) == Some(SyntaxKind::Less) {
                self.parse_type_arguments();
            }
            while self.at(SyntaxKind::Dot) && self.nth(1).is_some_and(|k| k.is_identifier_like()) {
                self.bump();
                self.expect_ident_like("expected type name segment");
                if self.nth(0) == Some(SyntaxKind::Less) {
                    self.parse_type_arguments();
                }
            }
            self.builder.finish_node(); // NamedType
        }
        // `new` array creation puts the first `[` outside the `Type` node, but annotations can
        // still appear immediately after the element type (dimension annotations). Consume them
        // here so the caller can continue with `[ ... ]` parsing.
        self.eat_trivia();
        while self.at_type_annotation_start() {
            self.parse_annotation();
            self.eat_trivia();
        }
        self.builder.finish_node(); // Type
    }

    fn parse_array_creation_dimensions(&mut self, allow_lambda: bool) {
        self.eat_trivia();

        if self.at(SyntaxKind::LBracket) && self.nth(1) != Some(SyntaxKind::RBracket) {
            self.builder.start_node(SyntaxKind::DimExprs.into());
            while self.at(SyntaxKind::LBracket) && self.nth(1) != Some(SyntaxKind::RBracket) {
                self.parse_dim_expr(allow_lambda);
            }
            self.builder.finish_node(); // DimExprs
        }

        if self.at(SyntaxKind::LBracket) && self.nth(1) == Some(SyntaxKind::RBracket) {
            self.builder.start_node(SyntaxKind::Dims.into());
            while self.at(SyntaxKind::LBracket) && self.nth(1) == Some(SyntaxKind::RBracket) {
                self.parse_dim();
            }
            self.builder.finish_node(); // Dims
        }

        // Recovery: consume any remaining bracket pairs so we don't get stuck on malformed mixes.
        while self.at(SyntaxKind::LBracket) {
            if self.nth(1) == Some(SyntaxKind::RBracket) {
                self.parse_dim();
            } else {
                self.parse_dim_expr(allow_lambda);
            }
        }
    }

    fn parse_dim_expr(&mut self, allow_lambda: bool) {
        self.builder.start_node(SyntaxKind::DimExpr.into());
        self.expect(SyntaxKind::LBracket, "expected `[`");
        if self.at(SyntaxKind::RBracket) {
            self.error_here("expected array dimension expression");
        } else {
            self.parse_expression_inner(0, allow_lambda);
        }
        self.expect(SyntaxKind::RBracket, "expected `]` after array dimension");
        self.builder.finish_node(); // DimExpr
    }

    fn parse_dim(&mut self) {
        self.builder.start_node(SyntaxKind::Dim.into());
        self.expect(SyntaxKind::LBracket, "expected `[`");
        self.expect(SyntaxKind::RBracket, "expected `]`");
        self.builder.finish_node(); // Dim
    }

    fn parse_array_initializer(&mut self, allow_lambda: bool) {
        self.builder.start_node(SyntaxKind::ArrayInitializer.into());
        self.expect(
            SyntaxKind::LBrace,
            "expected `{` to start array initializer",
        );

        self.eat_trivia();
        if !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::StringTemplateExprEnd)
            && !self.at(SyntaxKind::Eof)
        {
            self.builder
                .start_node(SyntaxKind::ArrayInitializerList.into());
            while !self.at(SyntaxKind::RBrace)
                && !self.at(SyntaxKind::StringTemplateExprEnd)
                && !self.at(SyntaxKind::Eof)
            {
                self.eat_trivia();
                if self.at(SyntaxKind::Comma) {
                    // Common during typing: `{, 1}`. Don't get stuck; treat as a missing element.
                    self.error_here("expected array initializer element");
                    self.bump();
                    continue;
                }

                // If we hit a clear statement boundary, bail out and let the caller recover.
                if matches!(
                    self.current(),
                    SyntaxKind::Semicolon
                        | SyntaxKind::RParen
                        | SyntaxKind::RBracket
                        | SyntaxKind::StringTemplateExprEnd
                ) {
                    break;
                }

                self.parse_variable_initializer(allow_lambda);
                self.eat_trivia();
                if self.at(SyntaxKind::Comma) {
                    self.bump();
                    continue;
                }
                break;
            }
            self.builder.finish_node(); // ArrayInitializerList
        }

        self.expect(
            SyntaxKind::RBrace,
            "expected `}` to close array initializer",
        );
        self.builder.finish_node(); // ArrayInitializer
    }

    fn parse_variable_initializer(&mut self, allow_lambda: bool) {
        self.builder
            .start_node(SyntaxKind::VariableInitializer.into());
        self.eat_trivia();
        if self.at(SyntaxKind::LBrace) {
            self.parse_array_initializer(allow_lambda);
        } else if self.can_start_expression_here() {
            self.parse_expression_inner(0, allow_lambda);
        } else {
            self.builder.start_node(SyntaxKind::Error.into());
            self.error_here("expected initializer");
            // Avoid consuming expression terminators; callers recover at `,` / `}`.
            if !matches!(
                self.current(),
                SyntaxKind::Comma
                    | SyntaxKind::RBrace
                    | SyntaxKind::StringTemplateExprEnd
                    | SyntaxKind::Eof
            ) {
                self.bump_any();
            }
            self.builder.finish_node(); // Error
        }
        self.builder.finish_node(); // VariableInitializer
    }

    fn parse_lambda_expression(&mut self, checkpoint: rowan::Checkpoint) {
        self.builder
            .start_node_at(checkpoint, SyntaxKind::LambdaExpression.into());
        self.builder.start_node(SyntaxKind::LambdaParameters.into());
        if self.at(SyntaxKind::LParen) {
            self.parse_lambda_parameter_list();
        } else {
            self.parse_lambda_parameter(false);
        }
        self.builder.finish_node(); // LambdaParameters

        // Robust recovery: lambda parameter parsing should never consume the `->` token. If we
        // encountered malformed parameters, skip forward to the arrow and continue parsing the
        // body.
        if !self.at(SyntaxKind::Arrow) {
            self.builder.start_node(SyntaxKind::Error.into());
            self.error_here("expected `->` in lambda");
            while !matches!(
                self.current(),
                SyntaxKind::Arrow | SyntaxKind::StringTemplateExprEnd | SyntaxKind::Eof
            ) {
                self.bump_any();
            }
            self.builder.finish_node(); // Error
        }

        self.expect(SyntaxKind::Arrow, "expected `->` in lambda");

        self.builder.start_node(SyntaxKind::LambdaBody.into());
        if self.at(SyntaxKind::LBrace) {
            self.parse_block(StatementContext::Normal);
        } else {
            self.parse_expression(0);
        }
        self.builder.finish_node(); // LambdaBody

        self.builder.finish_node(); // LambdaExpression
    }

    fn parse_lambda_parameter_list(&mut self) {
        self.builder
            .start_node(SyntaxKind::LambdaParameterList.into());
        self.expect(SyntaxKind::LParen, "expected `(` in lambda parameters");

        let formal = self.lambda_parameter_list_is_formal();

        while !matches!(
            self.current(),
            SyntaxKind::RParen
                | SyntaxKind::Arrow
                | SyntaxKind::StringTemplateExprEnd
                | SyntaxKind::Eof
        ) {
            self.eat_trivia();
            if self.at(SyntaxKind::Comma) {
                // `() ->` and `(, x) ->` should not get stuck; treat as a missing parameter.
                self.error_here("expected lambda parameter");
                self.bump();
                continue;
            }

            self.parse_lambda_parameter(formal);

            if self.at(SyntaxKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }

        if !matches!(
            self.current(),
            SyntaxKind::RParen
                | SyntaxKind::Arrow
                | SyntaxKind::StringTemplateExprEnd
                | SyntaxKind::Eof
        ) {
            self.builder.start_node(SyntaxKind::Error.into());
            self.error_here("expected `)` in lambda parameters");
            while !matches!(
                self.current(),
                SyntaxKind::RParen
                    | SyntaxKind::Arrow
                    | SyntaxKind::StringTemplateExprEnd
                    | SyntaxKind::Eof
            ) {
                self.bump_any();
            }
            self.builder.finish_node(); // Error
        }

        self.expect(SyntaxKind::RParen, "expected `)` in lambda parameters");
        self.builder.finish_node(); // LambdaParameterList
    }

    fn lambda_parameter_list_is_formal(&mut self) -> bool {
        self.eat_trivia();
        match self.current() {
            SyntaxKind::At | SyntaxKind::FinalKw => true,
            kind if is_primitive_type(kind) => true,
            SyntaxKind::VarKw => self.nth(1).is_some_and(|k| k.is_identifier_like()),
            kind if kind.is_identifier_like() => {
                !matches!(self.nth(1), Some(SyntaxKind::Comma | SyntaxKind::RParen))
            }
            _ => false,
        }
    }

    fn parse_lambda_parameter(&mut self, formal: bool) {
        self.builder.start_node(SyntaxKind::LambdaParameter.into());
        if formal {
            self.parse_modifiers();
            if self.at_type_start() {
                self.parse_type();
                // Permit type-use annotations on varargs ellipsis (e.g. `String @A ... args`).
                while self.at(SyntaxKind::At) && self.nth(1) != Some(SyntaxKind::InterfaceKw) {
                    self.parse_annotation();
                }
            } else {
                self.error_here("expected lambda parameter type");
                // Ensure progress without swallowing the lambda arrow.
                if !matches!(
                    self.current(),
                    SyntaxKind::Comma
                        | SyntaxKind::RParen
                        | SyntaxKind::Arrow
                        | SyntaxKind::StringTemplateExprEnd
                        | SyntaxKind::Eof
                ) {
                    self.bump_any();
                }
            }
            if self.at(SyntaxKind::Ellipsis) {
                self.bump();
            }
            if self.at_underscore_identifier() {
                self.parse_unnamed_pattern();
            } else {
                self.expect_ident_like("expected lambda parameter name");
            }
            // Support Java's `var x[]` / `String... args[]` style dims.
            while self.at(SyntaxKind::LBracket) && self.nth(1) == Some(SyntaxKind::RBracket) {
                self.bump();
                self.bump();
            }
        } else if self.at_underscore_identifier() {
            self.parse_unnamed_pattern();
        } else {
            self.expect_ident_like("expected lambda parameter");
        }
        self.builder.finish_node(); // LambdaParameter
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

        // Disambiguate reference casts vs parenthesized expressions in cases like:
        // `(x) + y`
        //
        // In Java's grammar, reference casts require a `UnaryExpressionNotPlusMinus` as the cast
        // operand, so `+` / `-` cannot appear immediately after the closing `)` unless the cast
        // type is a primitive type. Without this check, we would misclassify many common
        // parenthesized expressions as casts (and later semantic passes would treat `x` as a type
        // name).
        let mut type_probe = skip_trivia(&self.tokens, 1);
        loop {
            if self.tokens.get(type_probe).map(|t| t.kind) != Some(SyntaxKind::At) {
                break;
            }
            // `@interface` starts an annotation type declaration; not a type-use annotation.
            let next = skip_trivia(&self.tokens, type_probe + 1);
            if self.tokens.get(next).map(|t| t.kind) == Some(SyntaxKind::InterfaceKw) {
                break;
            }
            let after = skip_annotation(&self.tokens, type_probe);
            if after <= type_probe {
                break;
            }
            type_probe = skip_trivia(&self.tokens, after);
        }
        let is_primitive_cast = self
            .tokens
            .get(type_probe)
            .is_some_and(|t| is_primitive_type(t.kind));

        // Track nested parens so we don't stop at a `)` that appears inside a type-use annotation
        // argument list (e.g. `(@A(x=1) String) expr`).
        let mut paren_depth = 0usize;
        let mut i = 1usize;
        while let Some(tok) = self.tokens.get(i) {
            if tok.kind.is_trivia() {
                i += 1;
                continue;
            }
            match tok.kind {
                SyntaxKind::LParen => {
                    paren_depth += 1;
                }
                SyntaxKind::RParen => {
                    if paren_depth > 0 {
                        paren_depth = paren_depth.saturating_sub(1);
                    } else {
                        // Need an expression after ')'.
                        let mut j = i + 1;
                        while let Some(next) = self.tokens.get(j) {
                            if next.kind.is_trivia() {
                                j += 1;
                                continue;
                            }
                            if !is_primitive_cast
                                && matches!(
                                    next.kind,
                                    SyntaxKind::Plus
                                        | SyntaxKind::Minus
                                        | SyntaxKind::PlusPlus
                                        | SyntaxKind::MinusMinus
                                )
                            {
                                return false;
                            }
                            if can_start_expression(next.kind) {
                                return true;
                            }
                            // Expressions that can begin with type arguments (`<T>foo()`,
                            // `<T>this`, ...) or primitive type class literals (`int.class`,
                            // `int[]::new`) are not included in `can_start_expression`, but they are
                            // valid cast operands.
                            if next.kind == SyntaxKind::Less {
                                return at_type_arguments_expression_start(&self.tokens, j);
                            }
                            if is_primitive_type(next.kind) {
                                if next.kind == SyntaxKind::VoidKw {
                                    return at_primitive_class_literal_start(&self.tokens, j);
                                }
                                return at_primitive_type_suffix_start(&self.tokens, j);
                            }
                            return false;
                        }
                        return false;
                    }
                }
                _ => {}
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
                    if self
                        .tokens
                        .get(i)
                        .is_some_and(|t| t.kind.is_identifier_like())
                    {
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
                                .is_some_and(|t| t.kind.is_identifier_like())
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
                .is_some_and(|t| t.kind.is_identifier_like());
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
                    .is_some_and(|t| t.kind.is_identifier_like())
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
            i = skip_trivia(&self.tokens, i);

            // Dimension annotations: `int @A [] x;`
            if self.tokens.get(i).map(|t| t.kind) == Some(SyntaxKind::At) {
                let j = skip_trivia(&self.tokens, i + 1);
                if self.tokens.get(j).map(|t| t.kind) != Some(SyntaxKind::InterfaceKw) {
                    let after = skip_annotation(&self.tokens, i);
                    if after > i {
                        i = after;
                        continue;
                    }
                }
            }

            if self.tokens.get(i).map(|t| t.kind) != Some(SyntaxKind::LBracket) {
                break;
            }
            let after_l = skip_trivia(&self.tokens, i + 1);
            if self.tokens.get(after_l).map(|t| t.kind) != Some(SyntaxKind::RBracket) {
                break;
            }
            i = after_l + 1;
        }

        i = skip_trivia(&self.tokens, i);
        self.tokens
            .get(i)
            .is_some_and(|t| t.kind.is_identifier_like())
    }

    fn at_local_type_decl_start(&mut self) -> bool {
        let mut i = skip_trivia(&self.tokens, 0);

        // Skip modifiers and annotations. This is intentionally permissive so we can
        // recognize local class declarations while typing.
        loop {
            match self.tokens.get(i).map(|t| t.kind) {
                Some(SyntaxKind::At) => {
                    let next = skip_trivia(&self.tokens, i + 1);
                    // `@interface` starts an annotation type declaration.
                    if self.tokens.get(next).map(|t| t.kind) == Some(SyntaxKind::InterfaceKw) {
                        return true;
                    }
                    let after = skip_annotation(&self.tokens, i);
                    if after <= i {
                        break;
                    }
                    i = skip_trivia(&self.tokens, after);
                }
                Some(
                    SyntaxKind::PublicKw
                    | SyntaxKind::PrivateKw
                    | SyntaxKind::ProtectedKw
                    | SyntaxKind::StaticKw
                    | SyntaxKind::FinalKw
                    | SyntaxKind::AbstractKw
                    | SyntaxKind::SealedKw
                    | SyntaxKind::NonSealedKw
                    | SyntaxKind::StrictfpKw,
                ) => {
                    i = skip_trivia(&self.tokens, i + 1);
                }
                _ => break,
            }
        }

        match self.tokens.get(i).map(|t| t.kind) {
            Some(SyntaxKind::ClassKw | SyntaxKind::InterfaceKw | SyntaxKind::EnumKw) => true,
            Some(SyntaxKind::RecordKw) => {
                // `record Name [<...>] ( ... )`
                let mut j = skip_trivia(&self.tokens, i + 1);
                if !self
                    .tokens
                    .get(j)
                    .is_some_and(|t| t.kind.is_identifier_like())
                {
                    return false;
                }
                j = skip_trivia(&self.tokens, j + 1);
                if self.tokens.get(j).map(|t| t.kind) == Some(SyntaxKind::Less) {
                    j = skip_trivia(&self.tokens, skip_type_arguments(&self.tokens, j));
                }
                self.tokens.get(j).map(|t| t.kind) == Some(SyntaxKind::LParen)
            }
            Some(SyntaxKind::At) => {
                let j = skip_trivia(&self.tokens, i + 1);
                self.tokens.get(j).map(|t| t.kind) == Some(SyntaxKind::InterfaceKw)
            }
            _ => false,
        }
    }

    fn recover_top_level(&mut self) {
        self.builder.start_node(SyntaxKind::Error.into());
        self.error_here("unexpected token at top level");
        self.recover_to(TOP_LEVEL_RECOVERY);
        self.builder.finish_node();
    }

    fn recover_to_class_member_boundary(&mut self) {
        self.recover_to(MEMBER_RECOVERY);
    }

    fn recover_to_module_directive_boundary(&mut self) {
        self.recover_to(MODULE_DIRECTIVE_RECOVERY);
        // If we stopped at `;`, consume it to avoid loops.
        if self.at(SyntaxKind::Semicolon) {
            self.bump();
        }
    }

    fn recover_to(&mut self, recovery: TokenSet) {
        self.recover_to_inner(recovery, false);
    }

    fn recover_to_including_angles(&mut self, recovery: TokenSet) {
        self.recover_to_inner(recovery, true);
    }

    fn recover_to_inner(&mut self, recovery: TokenSet, track_angles: bool) {
        let mut depth = DelimiterDepth::default();
        loop {
            self.eat_trivia();
            let kind = self
                .tokens
                .front()
                .map(|t| t.kind)
                .unwrap_or(SyntaxKind::Eof);
            if kind == SyntaxKind::Eof {
                break;
            }
            if depth.is_zero(track_angles) && recovery.contains(kind) {
                break;
            }
            depth.update(kind, track_angles);
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

    fn at_module_decl_start(&mut self) -> bool {
        let mut i = skip_trivia(&self.tokens, 0);

        // Skip leading annotations. We treat `@interface` as an annotation type
        // declaration, not a module annotation.
        loop {
            if self.tokens.get(i).map(|t| t.kind) != Some(SyntaxKind::At) {
                break;
            }

            if self
                .tokens
                .get(skip_trivia(&self.tokens, i + 1))
                .map(|t| t.kind)
                == Some(SyntaxKind::InterfaceKw)
            {
                return false;
            }

            let next = skip_annotation(&self.tokens, i);
            if next <= i {
                break;
            }
            i = skip_trivia(&self.tokens, next);
        }

        match self.tokens.get(i).map(|t| t.kind) {
            Some(SyntaxKind::OpenKw) => {
                let j = skip_trivia(&self.tokens, i + 1);
                self.tokens.get(j).map(|t| t.kind) == Some(SyntaxKind::ModuleKw)
            }
            Some(SyntaxKind::ModuleKw) => true,
            _ => false,
        }
    }

    fn at_annotated_package_decl_start(&mut self) -> bool {
        // Recognize `@Anno ... package foo;` (package-info) without consuming tokens.
        // We only treat `@` as an annotation here when it is NOT `@interface`.
        let mut i = skip_trivia(&self.tokens, 0);
        if self.tokens.get(i).map(|t| t.kind) != Some(SyntaxKind::At) {
            return false;
        }

        loop {
            i = skip_trivia(&self.tokens, i);
            if self.tokens.get(i).map(|t| t.kind) != Some(SyntaxKind::At) {
                break;
            }
            // `@interface` starts an annotation type declaration, not a package annotation.
            if self
                .tokens
                .get(skip_trivia(&self.tokens, i + 1))
                .map(|t| t.kind)
                == Some(SyntaxKind::InterfaceKw)
            {
                return false;
            }
            let next = skip_annotation(&self.tokens, i);
            if next <= i {
                break;
            }
            i = next;
        }

        i = skip_trivia(&self.tokens, i);
        self.tokens.get(i).map(|t| t.kind) == Some(SyntaxKind::PackageKw)
    }

    fn at_type_start(&mut self) -> bool {
        self.at_primitive_type() || self.at_ident_like() || self.at_type_annotation_start()
    }

    fn at_type_annotation_start(&mut self) -> bool {
        self.at(SyntaxKind::At) && self.nth(1) != Some(SyntaxKind::InterfaceKw)
    }

    fn at_primitive_type(&mut self) -> bool {
        is_primitive_type(self.current())
    }

    fn current(&mut self) -> SyntaxKind {
        self.eat_trivia();
        self.tokens
            .front()
            .map(|t| t.kind)
            .unwrap_or(SyntaxKind::Eof)
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

    fn at_underscore_identifier(&mut self) -> bool {
        self.at(SyntaxKind::Identifier)
            && self
                .tokens
                .front()
                .is_some_and(|tok| tok.is_underscore_identifier(self.input))
    }

    fn parse_unnamed_pattern(&mut self) {
        self.builder.start_node(SyntaxKind::UnnamedPattern.into());
        self.bump();
        self.builder.finish_node();
    }

    fn eat_trivia(&mut self) {
        while self.tokens.front().is_some_and(|t| t.kind.is_trivia()) {
            self.bump_any();
        }
    }

    fn bump(&mut self) {
        self.eat_trivia();
        self.bump_any();
    }

    fn bump_any(&mut self) {
        if let Some(tok) = self.tokens.pop_front() {
            if !tok.kind.is_trivia() && tok.kind != SyntaxKind::Eof {
                self.last_non_trivia_end = tok.range.end;
                self.last_non_trivia_range = tok.range;
                self.last_non_trivia_kind = tok.kind;
            }
            let text = tok.text(self.input);
            self.builder.token(tok.kind.into(), text);
        }
    }

    fn expect(&mut self, kind: SyntaxKind, message: &str) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            let range = match kind {
                SyntaxKind::Semicolon => {
                    let pos = self.last_non_trivia_end;
                    TextRange {
                        start: pos,
                        end: pos,
                    }
                }
                _ => self.current_range(),
            };

            let found = self.current_token_display();
            self.error_at(range, format!("{message}, found {found}"));

            let missing = match kind {
                SyntaxKind::Semicolon => Some(SyntaxKind::MissingSemicolon),
                SyntaxKind::RParen => Some(SyntaxKind::MissingRParen),
                SyntaxKind::RBrace => Some(SyntaxKind::MissingRBrace),
                // The lexer emits a dedicated token kind for the `}` that closes a string template
                // interpolation. Use the existing `MissingRBrace` synthetic token to keep
                // incomplete template expressions structurally similar to other brace-delimited
                // constructs.
                SyntaxKind::StringTemplateExprEnd => Some(SyntaxKind::MissingRBrace),
                SyntaxKind::RBracket => Some(SyntaxKind::MissingRBracket),
                _ => None,
            };
            if let Some(missing) = missing {
                self.insert_missing(missing);
            }
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
        let found = self.current_token_display();
        self.error_at(range, format!("{message}, found {found}"));
    }

    fn error_at(&mut self, range: TextRange, message: String) {
        self.errors.push(ParseError { message, range });
    }

    fn insert_missing(&mut self, kind: SyntaxKind) {
        self.builder.token(kind.into(), "");
    }

    fn current_token_display(&mut self) -> String {
        self.eat_trivia();
        match self.tokens.front() {
            None => "end of file".to_string(),
            Some(tok) if tok.kind == SyntaxKind::Eof => "end of file".to_string(),
            Some(tok) => format!("{:?} `{}`", tok.kind, tok.text(self.input)),
        }
    }

    fn force_progress(&mut self, before_len: usize, recovery: TokenSet) {
        if self.tokens.len() != before_len {
            return;
        }
        if self.at(SyntaxKind::Eof) {
            return;
        }

        self.builder.start_node(SyntaxKind::Error.into());
        self.error_here("unexpected token");
        // Consume at least one token, then synchronize to the next boundary.
        self.bump_any();
        self.recover_to(recovery);
        self.builder.finish_node();
    }

    fn line_indent(&self, offset: u32) -> usize {
        let bytes = self.input.as_bytes();
        let mut i = (offset as usize).min(bytes.len());
        while i > 0 {
            match bytes[i - 1] {
                b'\n' | b'\r' => break,
                _ => i -= 1,
            }
        }

        let mut indent = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' => {
                    indent += 1;
                    i += 1;
                }
                _ => break,
            }
        }
        indent
    }

    fn line_indent_and_is_first_token(&self, offset: u32) -> (usize, bool) {
        let bytes = self.input.as_bytes();
        let offset = (offset as usize).min(bytes.len());

        let mut i = offset;
        while i > 0 {
            match bytes[i - 1] {
                b'\n' | b'\r' => break,
                _ => i -= 1,
            }
        }

        let mut indent = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' => {
                    indent += 1;
                    i += 1;
                }
                _ => break,
            }
        }

        (indent, i == offset)
    }

    fn current_range(&mut self) -> TextRange {
        self.eat_trivia();
        self.tokens.front().map(|t| t.range).unwrap_or_else(|| {
            let end = self.input.len() as u32;
            TextRange { start: end, end }
        })
    }
}

fn skip_trivia(tokens: &VecDeque<Token>, mut idx: usize) -> usize {
    while tokens.get(idx).is_some_and(|t| t.kind.is_trivia()) {
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

fn skip_annotation(tokens: &VecDeque<Token>, idx: usize) -> usize {
    // Assumes `tokens[idx]` is `@` and not `@interface`.
    let mut i = idx;
    if tokens.get(i).map(|t| t.kind) != Some(SyntaxKind::At) {
        return idx;
    }
    i += 1;
    i = skip_trivia(tokens, i);

    // Annotation name.
    if !tokens.get(i).is_some_and(|t| t.kind.is_identifier_like()) {
        return i;
    }
    i += 1;

    loop {
        let dot = skip_trivia(tokens, i);
        if tokens.get(dot).map(|t| t.kind) != Some(SyntaxKind::Dot) {
            i = dot;
            break;
        }
        let seg = skip_trivia(tokens, dot + 1);
        if !tokens.get(seg).is_some_and(|t| t.kind.is_identifier_like()) {
            i = dot;
            break;
        }
        i = seg + 1;
    }

    i = skip_trivia(tokens, i);
    if tokens.get(i).map(|t| t.kind) == Some(SyntaxKind::LParen) {
        i = skip_balanced_parens(tokens, i);
    }
    i
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

fn at_type_arguments_expression_start(tokens: &VecDeque<Token>, idx: usize) -> bool {
    if tokens.get(idx).map(|t| t.kind) != Some(SyntaxKind::Less) {
        return false;
    }
    let lookahead = skip_trivia(tokens, skip_type_arguments(tokens, idx));
    match tokens.get(lookahead).map(|t| t.kind) {
        Some(kind) if kind.is_identifier_like() => true,
        Some(SyntaxKind::ThisKw) | Some(SyntaxKind::SuperKw) => true,
        _ => false,
    }
}

fn at_primitive_class_literal_start(tokens: &VecDeque<Token>, idx: usize) -> bool {
    let after_dims = skip_reference_type_array_suffix(tokens, idx.saturating_add(1));
    let dot = skip_trivia(tokens, after_dims);
    if tokens.get(dot).map(|t| t.kind) != Some(SyntaxKind::Dot) {
        return false;
    }
    let class_kw = skip_trivia(tokens, dot + 1);
    tokens.get(class_kw).map(|t| t.kind) == Some(SyntaxKind::ClassKw)
}

fn at_primitive_method_reference_start(tokens: &VecDeque<Token>, idx: usize) -> bool {
    let after_dims = skip_reference_type_array_suffix(tokens, idx.saturating_add(1));
    let colons = skip_trivia(tokens, after_dims);
    tokens.get(colons).map(|t| t.kind) == Some(SyntaxKind::DoubleColon)
}

fn at_primitive_type_suffix_start(tokens: &VecDeque<Token>, idx: usize) -> bool {
    at_primitive_class_literal_start(tokens, idx) || at_primitive_method_reference_start(tokens, idx)
}

fn skip_reference_type_array_suffix(tokens: &VecDeque<Token>, mut idx: usize) -> usize {
    // Skips a sequence of `[]` pairs (allowing trivia between tokens).
    loop {
        let lbracket = skip_trivia(tokens, idx);
        if tokens.get(lbracket).map(|t| t.kind) != Some(SyntaxKind::LBracket) {
            return lbracket;
        }
        let rbracket = skip_trivia(tokens, lbracket + 1);
        if tokens.get(rbracket).map(|t| t.kind) != Some(SyntaxKind::RBracket) {
            return lbracket;
        }
        idx = rbracket + 1;
    }
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
            // Parseable everywhere for resilience; semantic analysis rejects it in most positions.
            | SyntaxKind::VoidKw
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
            | SyntaxKind::SwitchKw
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
        SyntaxKind::Star | SyntaxKind::Slash | SyntaxKind::Percent => {
            (70, 71, SyntaxKind::BinaryExpression)
        }
        SyntaxKind::Plus | SyntaxKind::Minus => (60, 61, SyntaxKind::BinaryExpression),
        SyntaxKind::LeftShift | SyntaxKind::RightShift | SyntaxKind::UnsignedRightShift => {
            (55, 56, SyntaxKind::BinaryExpression)
        }
        SyntaxKind::Less | SyntaxKind::LessEq | SyntaxKind::Greater | SyntaxKind::GreaterEq => {
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
#[allow(dead_code)]
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
