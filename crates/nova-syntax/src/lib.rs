//! Syntax tree and parsing primitives.
//!
//! This crate provides three complementary entry points:
//! - [`parse`]: produces a small, serializable green tree used by Nova's on-disk
//!   cache layer (`nova-cache`). Tokens store byte ranges into the source text.
//! - [`parse_java`]: produces a full-fidelity rowan-based syntax tree suitable
//!   for interactive IDE features and semantic analysis.
//! - [`parse_java_expression`]: parses a single Java expression snippet (not a full
//!   compilation unit) into a rowan syntax tree. This is primarily used by debugger
//!   integrations for watch/evaluate expressions.

pub mod ast;
mod convert;
mod feature_gate;
mod incremental;
mod language_level;
mod lexer;
mod literals;
mod parser;
mod java_parse_store;
mod syntax_kind;
mod tree_store;
mod util;

pub use ast::*;
pub use convert::TextEditConvertError;
pub use incremental::{parse_java_incremental, reparse_java};
pub use language_level::{FeatureAvailability, JavaFeature, JavaLanguageLevel};
pub use lexer::{lex, lex_with_errors, LexError, Lexer, Token};
pub use literals::{
    parse_double_literal, parse_float_literal, parse_int_literal, parse_literal,
    parse_long_literal, unescape_char_literal, unescape_string_literal, unescape_text_block,
    LiteralError, LiteralValue,
};
pub use parser::{
    parse_expression, parse_java, parse_java_block_fragment, parse_java_class_member_fragment,
    parse_java_expression, parse_java_expression_fragment, parse_java_statement_fragment,
    JavaFragmentParseResult, JavaParseResult, SyntaxElement, SyntaxNode, SyntaxToken,
};
pub use syntax_kind::{JavaLanguage, SyntaxKind, SYNTAX_SCHEMA_VERSION};
pub use java_parse_store::JavaParseStore;
pub use tree_store::SyntaxTreeStore;

/// Options that influence parsing diagnostics.
///
/// The Java parser always accepts a modern (superset) grammar. The language
/// level only affects *post-parse* feature-gate diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseOptions {
    pub language_level: JavaLanguageLevel,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            language_level: JavaLanguageLevel::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JavaParse {
    pub result: JavaParseResult,
    pub diagnostics: Vec<nova_types::Diagnostic>,
}

pub fn parse_java_with_options(text: &str, opts: ParseOptions) -> JavaParse {
    let result = parser::parse_java(text);
    let diagnostics = feature_gate::feature_gate_diagnostics(&result.syntax(), opts.language_level);
    JavaParse {
        result,
        diagnostics,
    }
}

/// Parse a `module-info.java` file.
///
/// Nova's Java parser accepts a modern superset grammar, but callers sometimes want to
/// specifically assert that a given source is a *module-info* compilation unit. In addition to
/// requiring a module declaration, this rejects package/import/type declarations.
pub fn parse_module_info(text: &str) -> Result<JavaParseResult, Vec<ParseError>> {
    let result = parse_java(text);
    if !result.errors.is_empty() {
        return Err(result.errors);
    }

    let unit = CompilationUnit::cast(result.syntax()).expect("root node is a compilation unit");

    let mut errors = Vec::new();

    if let Some(pkg) = unit.package() {
        errors.push(ParseError {
            message: "module-info.java must not contain a package declaration".to_string(),
            range: syntax_text_range(pkg.syntax()),
        });
    }

    if let Some(first_import) = unit.imports().next() {
        errors.push(ParseError {
            message: "module-info.java must not contain import declarations".to_string(),
            range: syntax_text_range(first_import.syntax()),
        });
    }

    if let Some(first_type) = unit.type_declarations().next() {
        errors.push(ParseError {
            message: "module-info.java must not contain type declarations".to_string(),
            range: syntax_text_range(first_type.syntax()),
        });
    }

    if unit.module_declaration().is_none() {
        errors.push(ParseError {
            message: "module-info.java is missing a module declaration".to_string(),
            range: TextRange { start: 0, end: 0 },
        });
    }

    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}

/// Run the Java syntax feature gate pass on an already-parsed syntax tree.
pub fn feature_gate_diagnostics(
    root: &SyntaxNode,
    language_level: JavaLanguageLevel,
) -> Vec<nova_types::Diagnostic> {
    feature_gate::feature_gate_diagnostics(root, language_level)
}

fn syntax_text_range(node: &SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange {
        start: u32::from(range.start()),
        end: u32::from(range.end()),
    }
}

use serde::{Deserialize, Serialize};

/// A half-open byte range within a source file (`start..end`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

impl TextRange {
    #[inline]
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end);
        Self {
            start: start as u32,
            end: end as u32,
        }
    }

    #[inline]
    pub fn len(self) -> u32 {
        self.end - self.start
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A single edit to a UTF-8 source buffer.
///
/// The edit uses byte offsets and applies `replacement` over `range` (half-open).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: TextRange,
    pub replacement: String,
}

impl TextEdit {
    pub fn new(range: TextRange, replacement: impl Into<String>) -> Self {
        Self {
            range,
            replacement: replacement.into(),
        }
    }

    pub fn insert(offset: u32, text: impl Into<String>) -> Self {
        Self::new(
            TextRange {
                start: offset,
                end: offset,
            },
            text,
        )
    }

    /// Net byte change produced by this edit (`replacement.len() - range.len()`).
    pub fn delta(&self) -> isize {
        self.replacement.len() as isize - self.range.len() as isize
    }
}
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct GreenToken {
    pub kind: SyntaxKind,
    pub range: TextRange,
}

impl GreenToken {
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        let start = self.range.start as usize;
        let end = self.range.end as usize;
        &source[start..end]
    }
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub enum GreenChild {
    Node(#[with(GreenNodeRkyvBox)] Box<GreenNode>),
    Token(GreenToken),
}

impl GreenChild {
    #[inline]
    pub fn text_len(&self) -> u32 {
        match self {
            GreenChild::Node(node) => node.text_len,
            GreenChild::Token(tok) => tok.range.len(),
        }
    }
}

/// A green node is immutable and position-independent.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct GreenNode {
    pub kind: SyntaxKind,
    pub text_len: u32,
    pub children: Vec<GreenChild>,
}

impl GreenNode {
    pub fn new(kind: SyntaxKind, children: Vec<GreenChild>) -> Self {
        let text_len = children.iter().map(|c| c.text_len()).sum();
        Self {
            kind,
            text_len,
            children,
        }
    }
}

#[repr(transparent)]
struct ArchivedGreenNodeBox(rkyv::boxed::ArchivedBox<ArchivedGreenNode>);

impl<C> rkyv::bytecheck::CheckBytes<C> for ArchivedGreenNodeBox
where
    C: rkyv::validation::ArchiveContext + ?Sized,
    <C as rkyv::Fallible>::Error: std::error::Error,
{
    type Error =
        <rkyv::boxed::ArchivedBox<ArchivedGreenNode> as rkyv::bytecheck::CheckBytes<C>>::Error;

    unsafe fn check_bytes<'a>(
        value: *const Self,
        context: &mut C,
    ) -> Result<&'a Self, Self::Error> {
        <rkyv::boxed::ArchivedBox<ArchivedGreenNode> as rkyv::bytecheck::CheckBytes<C>>::check_bytes(
            value.cast(),
            context,
        )?;
        Ok(&*value)
    }
}

/// Rkyv helper that archives `Box<GreenNode>` as an `ArchivedBox<ArchivedGreenNode>`.
///
/// This avoids a trait solver overflow when deriving `rkyv` traits for recursive green tree
/// structures (`GreenChild` <-> `GreenNode`).
struct GreenNodeRkyvBox;

impl rkyv::with::ArchiveWith<Box<GreenNode>> for GreenNodeRkyvBox {
    type Archived = ArchivedGreenNodeBox;
    type Resolver = rkyv::boxed::BoxResolver<()>;

    unsafe fn resolve_with(
        field: &Box<GreenNode>,
        pos: usize,
        resolver: Self::Resolver,
        out: *mut Self::Archived,
    ) {
        rkyv::boxed::ArchivedBox::resolve_from_ref(field.as_ref(), pos, resolver, out.cast());
    }
}

impl<S> rkyv::with::SerializeWith<Box<GreenNode>, S> for GreenNodeRkyvBox
where
    S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer + ?Sized,
{
    fn serialize_with(
        field: &Box<GreenNode>,
        serializer: &mut S,
    ) -> Result<Self::Resolver, S::Error> {
        let pos = serializer.serialize_value(field.as_ref())?;
        Ok(unsafe { rkyv::boxed::BoxResolver::from_raw_parts(pos, ()) })
    }
}

impl<D> rkyv::with::DeserializeWith<ArchivedGreenNodeBox, Box<GreenNode>, D> for GreenNodeRkyvBox
where
    D: rkyv::Fallible + ?Sized,
{
    fn deserialize_with(
        field: &ArchivedGreenNodeBox,
        deserializer: &mut D,
    ) -> Result<Box<GreenNode>, D::Error> {
        Ok(Box::new(rkyv::Deserialize::<GreenNode, D>::deserialize(
            field.0.as_ref(),
            deserializer,
        )?))
    }
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ParseError {
    pub message: String,
    pub range: TextRange,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct ParseResult {
    pub root: GreenNode,
    pub errors: Vec<ParseError>,
}

impl ParseResult {
    pub fn tokens(&self) -> impl Iterator<Item = &GreenToken> {
        self.root.children.iter().filter_map(|child| match child {
            GreenChild::Token(tok) => Some(tok),
            GreenChild::Node(_) => None,
        })
    }
}

/// Alias used by formatting integrations.
///
/// The parser currently produces a flat `ParseResult`, so this is sufficient.
pub type SyntaxTree = ParseResult;

/// Parse source text into a persistent, lossless green tree and error list.
///
/// This is currently a token-level "parser" that produces a flat
/// `CompilationUnit` node. The full Java grammar lives under [`parse_java`].
pub fn parse(text: &str) -> ParseResult {
    fn map_kind(kind: SyntaxKind, text: &str) -> SyntaxKind {
        match kind {
            SyntaxKind::Whitespace
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment => kind,

            // Identifiers and keywords are all treated as identifier-like tokens in the cache layer.
            SyntaxKind::Identifier
            | SyntaxKind::AbstractKw
            | SyntaxKind::AssertKw
            | SyntaxKind::BooleanKw
            | SyntaxKind::BreakKw
            | SyntaxKind::ByteKw
            | SyntaxKind::CaseKw
            | SyntaxKind::CatchKw
            | SyntaxKind::CharKw
            | SyntaxKind::ClassKw
            | SyntaxKind::ConstKw
            | SyntaxKind::ContinueKw
            | SyntaxKind::DefaultKw
            | SyntaxKind::DoKw
            | SyntaxKind::DoubleKw
            | SyntaxKind::ElseKw
            | SyntaxKind::EnumKw
            | SyntaxKind::ExtendsKw
            | SyntaxKind::FinalKw
            | SyntaxKind::FinallyKw
            | SyntaxKind::FloatKw
            | SyntaxKind::ForKw
            | SyntaxKind::GotoKw
            | SyntaxKind::IfKw
            | SyntaxKind::ImplementsKw
            | SyntaxKind::ImportKw
            | SyntaxKind::InstanceofKw
            | SyntaxKind::IntKw
            | SyntaxKind::InterfaceKw
            | SyntaxKind::LongKw
            | SyntaxKind::NativeKw
            | SyntaxKind::NewKw
            | SyntaxKind::PackageKw
            | SyntaxKind::PrivateKw
            | SyntaxKind::ProtectedKw
            | SyntaxKind::PublicKw
            | SyntaxKind::ReturnKw
            | SyntaxKind::ShortKw
            | SyntaxKind::StaticKw
            | SyntaxKind::StrictfpKw
            | SyntaxKind::SuperKw
            | SyntaxKind::SwitchKw
            | SyntaxKind::SynchronizedKw
            | SyntaxKind::ThisKw
            | SyntaxKind::ThrowKw
            | SyntaxKind::ThrowsKw
            | SyntaxKind::TransientKw
            | SyntaxKind::TryKw
            | SyntaxKind::VoidKw
            | SyntaxKind::VolatileKw
            | SyntaxKind::WhileKw
            | SyntaxKind::TrueKw
            | SyntaxKind::FalseKw
            | SyntaxKind::NullKw
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
            | SyntaxKind::WithKw => SyntaxKind::Identifier,

            SyntaxKind::IntLiteral
            | SyntaxKind::LongLiteral
            | SyntaxKind::FloatLiteral
            | SyntaxKind::DoubleLiteral => SyntaxKind::Number,

            SyntaxKind::CharLiteral => SyntaxKind::CharLiteral,
            SyntaxKind::StringLiteral | SyntaxKind::TextBlock => SyntaxKind::StringLiteral,

            SyntaxKind::Error => {
                // The cache layer wants to preserve "string-like"/"comment-like" behavior even when
                // the lexer reports an error (e.g. unterminated literals).
                if text.starts_with("/*") {
                    if text.starts_with("/**") {
                        SyntaxKind::DocComment
                    } else {
                        SyntaxKind::BlockComment
                    }
                } else if text.starts_with('"') {
                    SyntaxKind::StringLiteral
                } else if text.starts_with('\'') {
                    SyntaxKind::CharLiteral
                } else if text
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
                {
                    SyntaxKind::Number
                } else {
                    SyntaxKind::Punctuation
                }
            }

            // Everything else is treated as punctuation in the cache layer.
            _ => SyntaxKind::Punctuation,
        }
    }

    let (tokens, lex_errors) = lexer::lex_with_errors(text);
    let errors = lex_errors
        .into_iter()
        .map(|err| ParseError {
            message: err.message,
            range: err.range,
        })
        .collect();

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TemplateBoundary {
        Start,
        End,
    }

    fn template_boundary(kind: SyntaxKind) -> Option<TemplateBoundary> {
        match kind {
            SyntaxKind::StringTemplateStart => Some(TemplateBoundary::Start),
            SyntaxKind::StringTemplateEnd => Some(TemplateBoundary::End),
            _ => None,
        }
    }

    let mut children: Vec<GreenChild> = Vec::new();
    let mut idx = 0usize;
    while idx < tokens.len() {
        let token = &tokens[idx];
        if token.kind == SyntaxKind::Eof {
            idx += 1;
            continue;
        }

        // Keep string templates (preview feature) opaque to the legacy token-only formatter by
        // collapsing the entire template token run into a single `StringLiteral` token.
        //
        // The formatter will re-tokenize punctuation token text; if we emit `\{`/`}` pieces as
        // punctuation tokens, it can misinterpret `{`/`}` as real block braces and corrupt
        // indentation/spacing.
        if matches!(template_boundary(token.kind), Some(TemplateBoundary::Start)) {
            let start = token.range.start;
            let mut end = token.range.end;
            let mut template_depth = 0u32;

            while idx < tokens.len() {
                let tok = &tokens[idx];
                end = tok.range.end;

                if tok.kind == SyntaxKind::Eof {
                    break;
                }

                match template_boundary(tok.kind) {
                    Some(TemplateBoundary::Start) => {
                        template_depth += 1;
                    }
                    Some(TemplateBoundary::End) => {
                        template_depth = template_depth.saturating_sub(1);
                        idx += 1;
                        if template_depth == 0 {
                            break;
                        }
                        continue;
                    }
                    None => {}
                }

                idx += 1;
            }

            children.push(GreenChild::Token(GreenToken {
                kind: SyntaxKind::StringLiteral,
                range: TextRange { start, end },
            }));
            continue;
        }

        let kind = map_kind(token.kind, token.text(text));
        children.push(GreenChild::Token(GreenToken {
            kind,
            range: token.range,
        }));
        idx += 1;
    }

    ParseResult {
        root: GreenNode::new(SyntaxKind::CompilationUnit, children),
        errors,
    }
}

/// Experimental Java AST used by semantic lowering passes.
pub mod java;
#[cfg(test)]
mod tests;
